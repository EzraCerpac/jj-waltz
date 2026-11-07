//! Find untracked checkout files that deleting a workspace directory would lose.
//!
//! JJ preserves tracked files in the working-copy commit. An untracked file is safe to
//! delete only when a trash section of an ignore file covers it (see [`crate::trash`]),
//! or when a reference checkout has an identical copy at the same path, as copy-on-write
//! clones of the default workspace do. Every other untracked file is unpreserved:
//! ignored by an unmarked rule, such as a local secret, excluded by
//! `snapshot.auto-track`, or refused by JJ's snapshot.

use crate::trash::Verdict;
use anyhow::{Context, Result};
use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs::{self, File, Metadata};
use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};

/// Untracked top-level entry, file or directory, holding files deletion would lose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Unpreserved {
    /// Relative to the workspace root; a directory when its rule covers it whole.
    pub path: PathBuf,
    pub directory: bool,
    /// Why no trash section covers the entry.
    pub verdict: Verdict,
    /// Files under `path` with no identical reference copy, sorted.
    pub files: Vec<PathBuf>,
}

/// Find untracked files under `root` that no trash rule covers and that have no
/// identical copy at the same relative path under `reference`, grouped by the entry
/// whose rule decides them, sorted by path.
///
/// `tracked` lists the tracked files after a snapshot. Only untracked entries are
/// passed to `classify`, a batch at a time: those beside tracked files first, then the
/// contents of untracked directories that are not ignored, so a workspace typically
/// needs one batch. Trash directories are never entered. Symlinks are skipped: deleting
/// one loses no data, and workspace links are symlinks to shared targets. The workspace's
/// `.jj` and colocated `.git` are never candidates.
pub(crate) fn unpreserved_files(
    root: &Path,
    tracked: &[PathBuf],
    reference: &Path,
    mut classify: impl FnMut(&[PathBuf]) -> Result<Vec<Verdict>>,
) -> Result<Vec<Unpreserved>> {
    let tracked_files = tracked.iter().map(PathBuf::as_path).collect::<HashSet<_>>();
    let tracked_dirs = tracked
        .iter()
        .flat_map(|file| file.ancestors().skip(1))
        .filter(|dir| !dir.as_os_str().is_empty())
        .collect::<HashSet<_>>();
    let mut found = Vec::new();
    // Directories whose children are listed rather than classified as a whole.
    let mut open = vec![PathBuf::new()];
    while !open.is_empty() {
        let mut candidates = Vec::new();
        while let Some(relative) = open.pop() {
            let Some(entries) = read_dir(root, &relative)? else {
                continue;
            };
            for (child, file_type) in entries {
                if relative.as_os_str().is_empty()
                    && [".jj", ".git"].map(OsStr::new).contains(&child.as_os_str())
                {
                    continue;
                }
                if file_type.is_dir() {
                    if tracked_dirs.contains(child.as_path()) {
                        open.push(child);
                    } else {
                        candidates.push((child, true));
                    }
                } else if file_type.is_file() && !tracked_files.contains(child.as_path()) {
                    candidates.push((child, false));
                }
            }
        }
        if candidates.is_empty() {
            break;
        }
        let paths = candidates
            .iter()
            .map(|(path, _)| path.clone())
            .collect::<Vec<_>>();
        let verdicts = classify(&paths)?;
        anyhow::ensure!(
            verdicts.len() == paths.len(),
            "ignore classification returned {} verdicts for {} paths",
            verdicts.len(),
            paths.len()
        );
        for ((path, directory), verdict) in candidates.into_iter().zip(verdicts) {
            match verdict {
                Verdict::Trash => {}
                // Its contents may be decided by other rules.
                Verdict::NotIgnored if directory => open.push(path),
                verdict => {
                    let files = if directory {
                        unpreserved_in_directory(root, &path, reference)?
                    } else {
                        unpreserved_file(root, &path, reference)?
                            .into_iter()
                            .collect()
                    };
                    if !files.is_empty() {
                        found.push(Unpreserved {
                            path,
                            directory,
                            verdict,
                            files,
                        });
                    }
                }
            }
        }
    }
    found.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(found)
}

/// List a directory's entries as root-relative paths with their non-followed types.
/// `None` when it was removed since the snapshot: nothing is left to lose.
fn read_dir(root: &Path, relative: &Path) -> Result<Option<Vec<(PathBuf, fs::FileType)>>> {
    let directory = root.join(relative);
    let entries = match fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", directory.display()));
        }
    };
    let mut listed = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| format!("failed to read {}", directory.display()))?;
        let child = relative.join(entry.file_name());
        let file_type = entry
            .file_type()
            .with_context(|| format!("failed to inspect {}", root.join(&child).display()))?;
        listed.push((child, file_type));
    }
    Ok(Some(listed))
}

/// Every regular file under an untracked directory that lacks an identical reference copy.
/// The rule deciding the directory decides everything inside it, so nothing is classified.
fn unpreserved_in_directory(
    root: &Path,
    relative: &Path,
    reference: &Path,
) -> Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    let mut pending = vec![relative.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for (child, file_type) in read_dir(root, &directory)?.unwrap_or_default() {
            if file_type.is_dir() {
                pending.push(child);
            } else if file_type.is_file() {
                found.extend(unpreserved_file(root, &child, reference)?);
            }
        }
    }
    found.sort();
    Ok(found)
}

/// The file itself when it is still a regular file without an identical reference copy.
fn unpreserved_file(root: &Path, relative: &Path, reference: &Path) -> Result<Option<PathBuf>> {
    let path = root.join(relative);
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.is_file() => {}
        Ok(_) => return Ok(None),
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to inspect {}", path.display()));
        }
    }
    Ok((!has_identical_copy(&path, &reference.join(relative))?).then(|| relative.to_path_buf()))
}

fn has_identical_copy(path: &Path, reference: &Path) -> Result<bool> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect {}", path.display()))?;
    let reference_metadata = match fs::symlink_metadata(reference) {
        Ok(metadata) if metadata.is_file() => metadata,
        Ok(_) => return Ok(false),
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to inspect {}", reference.display()));
        }
    };
    if metadata.len() != reference_metadata.len() {
        return Ok(false);
    }
    if same_modified_time(&metadata, &reference_metadata) {
        return Ok(true);
    }
    same_contents(path, reference)
}

fn same_modified_time(left: &Metadata, right: &Metadata) -> bool {
    matches!((left.modified(), right.modified()), (Ok(left), Ok(right)) if left == right)
}

fn same_contents(left: &Path, right: &Path) -> Result<bool> {
    let open = |path: &Path| {
        File::open(path).with_context(|| format!("failed to read {}", path.display()))
    };
    let (mut left_file, mut right_file) = (open(left)?, open(right)?);
    let (mut left_buffer, mut right_buffer) = (vec![0; 64 * 1024], vec![0; 64 * 1024]);
    loop {
        let read = read_full(&mut left_file, &mut left_buffer)
            .with_context(|| format!("failed to read {}", left.display()))?;
        let right_read = read_full(&mut right_file, &mut right_buffer)
            .with_context(|| format!("failed to read {}", right.display()))?;
        if read != right_read || left_buffer[..read] != right_buffer[..read] {
            return Ok(false);
        }
        if read == 0 {
            return Ok(true);
        }
    }
}

/// Fill `buffer` unless the reader ends first, so both sides compare equal-sized chunks.
fn read_full(reader: &mut impl Read, buffer: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        match reader.read(&mut buffer[filled..])? {
            0 => break,
            read => filled += read,
        }
    }
    Ok(filled)
}

/// Summarize unpreserved entries for an error message, one line each with the rule that
/// keeps it from being trash, capped at `limit` lines.
pub(crate) fn summarize(entries: &[Unpreserved], limit: usize) -> String {
    let mut lines = entries
        .iter()
        .take(limit)
        .map(|entry| {
            let name = if entry.directory {
                let count = entry.files.len();
                let noun = if count == 1 { "file" } else { "files" };
                format!("{}/ ({count} {noun})", entry.path.display())
            } else {
                entry.path.display().to_string()
            };
            format!("  {name}  [{}]", entry.verdict)
        })
        .collect::<Vec<_>>();
    if entries.len() > limit {
        lines.push(format!("  … and {} more", entries.len() - limit));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trash::IgnoreRule;

    fn write(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    fn rule(pattern: &str) -> Verdict {
        Verdict::Ignored(IgnoreRule {
            source: PathBuf::from(".gitignore"),
            line: 1,
            pattern: pattern.to_owned(),
        })
    }

    /// Classify by name the way the ignore rules of a typical repository would.
    fn fake_rules(paths: &[PathBuf]) -> Result<Vec<Verdict>> {
        Ok(paths
            .iter()
            .map(|path| match path.file_name().unwrap().to_str().unwrap() {
                "target" | "debug.log" => Verdict::Trash,
                ".env.local" | "cache.local" | "copied.local" | "shared.local" => rule(".env*"),
                ".vscode" => rule(".vscode/"),
                _ => Verdict::NotIgnored,
            })
            .collect())
    }

    #[test]
    fn reports_untracked_files_outside_trash_without_an_identical_reference_copy() {
        let temp = tempfile::tempdir().unwrap();
        let (root, reference) = (temp.path().join("work"), temp.path().join("default"));
        write(&root.join("src/main.rs"), "tracked");
        write(&root.join("src/.env.local"), "unique secret");
        write(&root.join("shared.local"), "same");
        write(&reference.join("shared.local"), "same");
        write(&root.join("notes/a.md"), "only here");
        write(&root.join("notes/deep/b.md"), "same");
        write(&reference.join("notes/deep/b.md"), "same");
        write(&root.join(".vscode/settings.json"), "mine");
        write(&root.join(".vscode/launch.json"), "mine too");
        // Same size, different times: contents decide.
        write(&root.join("cache.local"), "changed");
        write(&reference.join("cache.local"), "CHANGED");
        write(&root.join("copied.local"), "same bytes");
        write(&reference.join("copied.local"), "same bytes");
        for path in ["cache.local", "copied.local"] {
            File::options()
                .write(true)
                .open(reference.join(path))
                .unwrap()
                .set_modified(std::time::SystemTime::UNIX_EPOCH)
                .unwrap();
        }
        // Trash, and JJ's own state, are never reported or entered.
        write(&root.join("target/debug/app"), "built here");
        write(&root.join("src/debug.log"), "trace");
        write(&root.join(".jj/working_copy/state"), "jj");
        #[cfg(unix)]
        std::os::unix::fs::symlink("/nonexistent/shared", root.join("notes/linked")).unwrap();

        let mut batches = Vec::new();
        let found = unpreserved_files(
            &root,
            &[PathBuf::from("src/main.rs")],
            &reference,
            |paths| {
                batches.push(paths.len());
                fake_rules(paths)
            },
        )
        .unwrap();

        let summary = found
            .iter()
            .map(|entry| {
                (
                    entry.path.to_str().unwrap(),
                    entry.files.len(),
                    entry.verdict.clone(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            summary,
            [
                (".vscode", 2, rule(".vscode/")),
                ("cache.local", 1, rule(".env*")),
                ("notes/a.md", 1, Verdict::NotIgnored),
                ("src/.env.local", 1, rule(".env*")),
            ]
        );
        // One batch beside tracked files, then one per level of untracked, unignored
        // directories: `notes`, then `notes/deep`.
        assert_eq!(batches.len(), 3);
    }

    #[test]
    fn summarizes_entries_with_their_rule_and_a_cap() {
        let entry = |path: &str, directory, files: &[&str]| Unpreserved {
            path: PathBuf::from(path),
            directory,
            verdict: rule(".env*"),
            files: files.iter().map(PathBuf::from).collect(),
        };
        let entries = [
            entry(".env", false, &[".env"]),
            entry(".vscode", true, &[".vscode/a", ".vscode/b"]),
            entry("zz", false, &["zz"]),
        ];
        assert_eq!(
            summarize(&entries, 2),
            "  .env  [.gitignore:1: .env*]\n  .vscode/ (2 files)  [.gitignore:1: .env*]\n  … and 1 more"
        );
    }
}
