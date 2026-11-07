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

/// Classifies paths in one workspace, reading each ignore file's sections once.
pub(crate) struct Classifier {
    git_dir: PathBuf,
    root: PathBuf,
    /// Per ignore file, whether each line (zero-based) is inside a trash section.
    sections: HashMap<PathBuf, Vec<bool>>,
}

impl Classifier {
    /// `git_dir` is the Git directory backing the repository, as `jj git root` reports
    /// it; `root` is the workspace root that classified paths are relative to.
    pub(crate) fn new(git_dir: PathBuf, root: PathBuf) -> Self {
        Self {
            git_dir,
            root,
            sections: HashMap::new(),
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
                Some(rule) if self.in_trash_section(&rule) => Verdict::Trash,
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

    fn in_trash_section(&mut self, rule: &IgnoreRule) -> bool {
        let path = self.root.join(&rule.source);
        let sections = self.sections.entry(path).or_insert_with_key(|path| {
            // An ignore file that cannot be read now marks nothing as trash.
            fs::read_to_string(path)
                .map(|contents| trash_lines(&contents))
                .unwrap_or_default()
        });
        rule.line
            .checked_sub(1)
            .and_then(|index| sections.get(index))
            .copied()
            .unwrap_or(false)
    }
}

/// Whether each line of an ignore file is inside a trash section.
///
/// A section starts after a [`TRASH_MARKER`] line and ends at the next blank line or
/// marker line. Comment lines inside it are allowed. Git skips a leading byte-order
/// mark, so a marker on the first line still counts.
fn trash_lines(contents: &str) -> Vec<bool> {
    let contents = contents.strip_prefix('\u{feff}').unwrap_or(contents);
    let mut in_section = false;
    contents
        .lines()
        .map(|line| {
            let line = line.trim();
            if line.is_empty() {
                in_section = false;
                false
            } else if line.starts_with(MARKER_PREFIX) {
                in_section = line == TRASH_MARKER;
                false
            } else {
                in_section
            }
        })
        .collect()
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
            trash_lines(contents),
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
            trash_lines("#jw:trash\na\n# jw: trash\nb\n# JW:TRASH\nc\n"),
            [false, false, false, false, false, false]
        );
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
