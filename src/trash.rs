//! Classify untracked paths by the ignore rule that decides them.
//!
//! Ignore files mix two kinds of untracked file: regenerable output such as dependencies
//! and build products, which is safe to delete, and local data such as secrets, which is
//! not. A repository marks the regenerable kind by putting its patterns in a trash
//! section of any ignore file Git and JJ read: a `# jw:trash` line starts the section,
//! and it runs until the next blank line or `# jw:` line. Every other rule is precious,
//! so an unmarked pattern, including one a tool appends later, never makes a file
//! deletable.
//!
//! Some tools write their own `.gitignore`, often a bare `*`, and rewrite it on every run,
//! so it cannot hold a marker. Git lets the deepest ignore file decide, so no rule above
//! it can override it either. A `# jw:trash-rules <path>` line in a `.gitignore` above
//! such a file adopts it instead: every ignoring rule in the adopted file is trash. The
//! path is relative to the directory of the `.gitignore` holding the line, as Git
//! patterns are.
//!
//! `git check-ignore --verbose` names the ignore file and line of the pattern that
//! decides each path, which is what ties a path to its section. Git and JJ read the same
//! ignore files: `.gitignore` in any directory, `info/exclude` in the Git directory, and
//! `core.excludesFile`.

use crate::paths::external_command_path;
use anyhow::{Context, Result, bail};
use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Starts a trash section in an ignore file.
pub(crate) const TRASH_MARKER: &str = "# jw:trash";
/// Followed by a path, adopts the `.gitignore` at that path: all its rules are trash.
pub(crate) const ADOPT_MARKER: &str = "# jw:trash-rules";
/// Any line with this prefix ends a section; only [`TRASH_MARKER`] starts a trash one.
const MARKER_PREFIX: &str = "# jw:";

/// How the ignore rules decide one untracked path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// Ignored by a pattern in a trash section.
    Trash,
    /// Ignored by a pattern outside any trash section.
    Ignored(IgnoreRule),
    /// Not ignored, or re-included by a negated pattern. JJ leaves such a file untracked
    /// only when `snapshot.auto-track` excludes it or JJ refused to snapshot it.
    NotIgnored,
    /// The name cannot be passed to Git, so the path is never treated as trash.
    Unclassifiable,
}

impl fmt::Display for Verdict {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Trash => write!(formatter, "trash"),
            Self::Ignored(rule) => write!(formatter, "{rule}"),
            Self::NotIgnored => write!(formatter, "not ignored"),
            Self::Unclassifiable => write!(formatter, "name is not valid UTF-8"),
        }
    }
}

/// The pattern that decided a path, as `git check-ignore --verbose` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IgnoreRule {
    /// The ignore file, relative to the workspace root when it is inside it.
    pub source: PathBuf,
    /// One-based line number of the pattern in `source`.
    pub line: usize,
    pub pattern: String,
}

impl fmt::Display for IgnoreRule {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{}:{}: {}",
            self.source.display(),
            self.line,
            self.pattern
        )
    }
}

/// Classifies paths in one workspace, reading each ignore file once.
pub(crate) struct Classifier {
    git_dir: PathBuf,
    root: PathBuf,
    /// Parsed ignore files by absolute path.
    files: HashMap<PathBuf, IgnoreFile>,
}

/// What an ignore file's `# jw:` lines say.
#[derive(Debug, Default, PartialEq, Eq)]
struct IgnoreFile {
    /// Whether each line (zero-based) is inside a trash section.
    trash_lines: Vec<bool>,
    /// The `.gitignore` files it adopts, relative to its own directory.
    adopted: Vec<PathBuf>,
}

impl Classifier {
    /// `git_dir` is the Git directory backing the repository, as `jj git root` reports
    /// it; `root` is the workspace root that classified paths are relative to.
    pub(crate) fn new(git_dir: PathBuf, root: PathBuf) -> Self {
        Self {
            git_dir,
            root,
            files: HashMap::new(),
        }
    }

    /// Classify `paths`, relative to the workspace root, with one `git check-ignore`
    /// process. A directory is decided by the rule that decides the directory itself;
    /// Git applies that rule to everything inside an ignored directory.
    pub(crate) fn classify(&mut self, paths: &[PathBuf]) -> Result<Vec<Verdict>> {
        let mut verdicts = vec![Verdict::Unclassifiable; paths.len()];
        let mut queried = Vec::new();
        let mut input = Vec::new();
        for (index, path) in paths.iter().enumerate() {
            if let Some(name) = git_path(path) {
                input.extend_from_slice(name.as_bytes());
                input.push(0);
                queried.push((index, name));
            }
        }
        if queried.is_empty() {
            return Ok(verdicts);
        }
        let output = self.check_ignore(input)?;
        let records = parse_check_ignore(&output)?;
        if records.len() != queried.len() {
            bail!(
                "git check-ignore returned {} records for {} paths",
                records.len(),
                queried.len()
            )
        }
        for ((index, name), record) in queried.into_iter().zip(records) {
            if record.path != name {
                bail!(
                    "git check-ignore answered for {:?} instead of {name:?}",
                    record.path
                )
            }
            verdicts[index] = match record.rule {
                None => Verdict::NotIgnored,
                Some(rule) if rule.pattern.starts_with('!') => Verdict::NotIgnored,
                Some(rule) if self.is_trash(&rule) => Verdict::Trash,
                Some(rule) => Verdict::Ignored(rule),
            };
        }
        Ok(verdicts)
    }

    fn check_ignore(&self, input: Vec<u8>) -> Result<Vec<u8>> {
        let mut command = Command::new("git");
        // The caller's Git-hook environment must not redirect the query.
        for variable in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_COMMON_DIR",
            "GIT_INDEX_FILE",
            "GIT_CEILING_DIRECTORIES",
        ] {
            command.env_remove(variable);
        }
        // `--no-index` judges paths by the ignore rules alone; the caller already knows
        // which files JJ tracks. `--non-matching` answers every path, in input order.
        let mut child = command
            .current_dir(&self.root)
            .arg("--no-optional-locks")
            .arg("--git-dir")
            .arg(external_command_path(&self.git_dir).as_ref())
            .arg("--work-tree")
            .arg(external_command_path(&self.root).as_ref())
            .args([
                "check-ignore",
                "--verbose",
                "--non-matching",
                "--no-index",
                "-z",
                "--stdin",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("failed to run git check-ignore; classifying ignored files needs Git")?;
        let mut stdin = child.stdin.take().expect("piped stdin");
        // Write from another thread: Git answers while reading, and a full stdout pipe
        // would otherwise stall both processes.
        let writer = std::thread::spawn(move || stdin.write_all(&input));
        let output = child
            .wait_with_output()
            .context("failed to run git check-ignore")?;
        writer
            .join()
            .expect("git check-ignore input thread panicked")
            .context("failed to send paths to git check-ignore")?;
        // Exit status 1 only means that no path is ignored.
        if !matches!(output.status.code(), Some(0 | 1)) {
            bail!(
                "git check-ignore failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )
        }
        Ok(output.stdout)
    }

    /// Whether `rule` sits in a trash section or in an adopted ignore file.
    fn is_trash(&mut self, rule: &IgnoreRule) -> bool {
        let in_section = rule
            .line
            .checked_sub(1)
            .and_then(|index| {
                let path = self.root.join(&rule.source);
                self.ignore_file(path).trash_lines.get(index).copied()
            })
            .unwrap_or(false);
        in_section || self.is_adopted(&rule.source)
    }

    /// Whether a `.gitignore` in a directory above `source`'s own adopts it.
    fn is_adopted(&mut self, source: &Path) -> bool {
        let Some(own_directory) = adoptable_directory(source) else {
            return false;
        };
        own_directory.ancestors().skip(1).any(|directory| {
            let path = self.root.join(directory).join(GITIGNORE);
            self.ignore_file(path)
                .adopted
                .iter()
                .any(|adopted| directory.join(adopted) == source)
        })
    }

    fn ignore_file(&mut self, path: PathBuf) -> &IgnoreFile {
        self.files.entry(path).or_insert_with_key(|path| {
            // An ignore file that cannot be read, or does not exist, marks nothing as trash.
            fs::read_to_string(path)
                .map(|contents| parse_ignore_file(&contents))
                .unwrap_or_default()
        })
    }
}

const GITIGNORE: &str = ".gitignore";

/// The directory of an ignore file that a `# jw:trash-rules` line could adopt: a
/// `.gitignore` inside the workspace, below its root, since the adopting `.gitignore`
/// must sit in a directory above it.
pub(crate) fn adoptable_directory(source: &Path) -> Option<&Path> {
    if source.is_absolute() || source.file_name()? != GITIGNORE {
        return None;
    }
    source
        .parent()
        .filter(|directory| !directory.as_os_str().is_empty())
}

/// Read an ignore file's trash sections and adopted files.
///
/// A section starts after a [`TRASH_MARKER`] line and ends at the next blank line or
/// marker line. Comment lines inside it are allowed. Git skips a leading byte-order
/// mark, so a marker on the first line still counts. An [`ADOPT_MARKER`] line is a
/// marker line too, so it ends a section. One whose path is empty or absolute, or has an
/// empty, `.`, `..`, or backslashed component, adopts nothing.
fn parse_ignore_file(contents: &str) -> IgnoreFile {
    let contents = contents.strip_prefix('\u{feff}').unwrap_or(contents);
    let mut file = IgnoreFile::default();
    let mut in_section = false;
    for line in contents.lines() {
        let line = line.trim();
        let in_trash = if line.is_empty() {
            in_section = false;
            false
        } else if line.starts_with(MARKER_PREFIX) {
            in_section = line == TRASH_MARKER;
            file.adopted.extend(adopted_path(line));
            false
        } else {
            in_section
        };
        file.trash_lines.push(in_trash);
    }
    file
}

/// The relative path an [`ADOPT_MARKER`] line names, `/`-separated like Git patterns.
fn adopted_path(line: &str) -> Option<PathBuf> {
    let rest = line.strip_prefix(ADOPT_MARKER)?;
    // The marker must stand alone: `# jw:trash-rulesx` is another marker.
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let mut path = PathBuf::new();
    for part in rest.trim().split('/') {
        if part.is_empty() || part == "." || part == ".." || part.contains('\\') {
            return None;
        }
        path.push(part);
    }
    Some(path)
}

/// A workspace-relative path as Git expects it on the command line: `/`-separated UTF-8.
fn git_path(path: &Path) -> Option<String> {
    let parts = path
        .components()
        .map(|component| component.as_os_str().to_str())
        .collect::<Option<Vec<_>>>()?;
    Some(parts.join("/"))
}

#[derive(Debug, PartialEq, Eq)]
struct CheckIgnoreRecord {
    path: String,
    rule: Option<IgnoreRule>,
}

/// Parse `git check-ignore --verbose --non-matching -z` output: four NUL-terminated
/// fields per path, source, line, pattern, and path, with empty rule fields when no
/// pattern matches.
fn parse_check_ignore(output: &[u8]) -> Result<Vec<CheckIgnoreRecord>> {
    let text = std::str::from_utf8(output).context("git check-ignore output is not UTF-8")?;
    let Some(text) = text.strip_suffix('\0') else {
        if text.is_empty() {
            return Ok(Vec::new());
        }
        bail!("git check-ignore output is not NUL-terminated")
    };
    let fields = text.split('\0').collect::<Vec<_>>();
    let (records, []) = fields.as_chunks::<4>() else {
        bail!("git check-ignore returned an incomplete record")
    };
    records
        .iter()
        .map(|[source, line, pattern, path]| {
            let rule = if source.is_empty() {
                None
            } else {
                Some(IgnoreRule {
                    source: PathBuf::from(source),
                    line: line
                        .parse()
                        .with_context(|| format!("invalid check-ignore line number {line:?}"))?,
                    pattern: (*pattern).to_owned(),
                })
            };
            Ok(CheckIgnoreRecord {
                path: (*path).to_owned(),
                rule,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trash_sections_end_at_blank_and_marker_lines() {
        let contents = "\u{feff}# jw:trash\ntarget/\n# rust\n*.rlib\n\nnode_modules/\n# jw:trash\ndist/\n# jw:precious\n.env*\n# jw:trash\r\n*.log\r\n";
        assert_eq!(
            parse_ignore_file(contents).trash_lines,
            [
                false, // # jw:trash
                true,  // target/
                true,  // # rust
                true,  // *.rlib
                false, // blank
                false, // node_modules/
                false, // # jw:trash
                true,  // dist/
                false, // # jw:precious
                false, // .env*
                false, // # jw:trash
                true,  // *.log
            ]
        );
    }

    #[test]
    fn near_miss_markers_mark_nothing() {
        assert_eq!(
            parse_ignore_file("#jw:trash\na\n# jw: trash\nb\n# JW:TRASH\nc\n").trash_lines,
            [false, false, false, false, false, false]
        );
    }

    #[test]
    fn trash_rules_lines_adopt_relative_paths_and_end_sections() {
        let file = parse_ignore_file(
            "# jw:trash\ndist/\n# jw:trash-rules project.inlang/.gitignore\n*.log\n# jw:trash-rules\ta b/.gitignore \n",
        );
        assert_eq!(file.trash_lines, [false, true, false, false, false]);
        assert_eq!(
            file.adopted,
            [
                PathBuf::from("project.inlang/.gitignore"),
                PathBuf::from("a b/.gitignore")
            ]
        );
    }

    #[test]
    fn malformed_trash_rules_lines_adopt_nothing() {
        for line in [
            "# jw:trash-rules",
            "# jw:trash-rules   ",
            "# jw:trash-rulesx/.gitignore",
            "# jw:trash-rules /abs/.gitignore",
            "# jw:trash-rules ../up/.gitignore",
            "# jw:trash-rules a/./.gitignore",
            "# jw:trash-rules a//.gitignore",
            "# jw:trash-rules a\\.gitignore",
            "#jw:trash-rules a/.gitignore",
        ] {
            assert_eq!(
                parse_ignore_file(line).adopted,
                [] as [PathBuf; 0],
                "{line}"
            );
        }
    }

    #[test]
    fn only_nested_gitignore_files_are_adoptable() {
        assert_eq!(
            adoptable_directory(Path::new("project.inlang/.gitignore")),
            Some(Path::new("project.inlang"))
        );
        assert_eq!(adoptable_directory(Path::new(".gitignore")), None);
        assert_eq!(adoptable_directory(Path::new("app/ignore")), None);
        assert_eq!(adoptable_directory(Path::new("/home/u/.gitignore")), None);
    }

    #[test]
    fn parses_check_ignore_records() {
        let output = b".gitignore\x002\x00target/\x00target/debug\x00\x00\x00\x00keep.txt\x00/home/u/.config/git/ignore\x001\x00!x\x00x\x00";
        assert_eq!(
            parse_check_ignore(output).unwrap(),
            [
                CheckIgnoreRecord {
                    path: "target/debug".to_owned(),
                    rule: Some(IgnoreRule {
                        source: PathBuf::from(".gitignore"),
                        line: 2,
                        pattern: "target/".to_owned(),
                    }),
                },
                CheckIgnoreRecord {
                    path: "keep.txt".to_owned(),
                    rule: None,
                },
                CheckIgnoreRecord {
                    path: "x".to_owned(),
                    rule: Some(IgnoreRule {
                        source: PathBuf::from("/home/u/.config/git/ignore"),
                        line: 1,
                        pattern: "!x".to_owned(),
                    }),
                },
            ]
        );
        assert!(parse_check_ignore(b"").unwrap().is_empty());
        assert!(parse_check_ignore(b"a\x001\x00").is_err());
    }
}
