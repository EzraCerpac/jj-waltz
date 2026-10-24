//! Find untracked checkout files that deleting a workspace directory would lose.
//!
//! JJ preserves tracked files in the working-copy commit. Ignored files are treated as
//! regenerable, like dependencies and build output, and are deleted with the workspace.
//! What remains are files JJ leaves untracked although they are not ignored: those
//! excluded by `snapshot.auto-track`, such as local secrets, and files JJ refused to
//! snapshot. One with an identical copy at the same path in a reference checkout is not
//! lost either, as copy-on-write clones of the default workspace show.

use crate::jj::UntrackedPath;
use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::fs::{self, File, Metadata};
use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};

/// Return the regular files among `candidates` under `root` that have no identical copy
/// at the same relative path under `reference`, sorted.
///
/// A directory candidate is walked whole. JJ reports such a directory as one entry, so
/// ignored files inside it cannot be told apart and are checked too. Symlinks are skipped:
/// deleting one loses no data, and workspace links are symlinks to shared targets.
/// Identity is the same size plus the same modification time, which clones preserve;
/// contents are only read when sizes match but times differ.
pub(crate) fn unpreserved_files(
    root: &Path,
    candidates: &[UntrackedPath],
    reference: &Path,
) -> Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    let mut pending = Vec::new();
    for candidate in candidates {
        if candidate.directory {
            pending.push(candidate.path.clone());
        } else {
            check_file(root, &candidate.path, reference, &mut found)?;
        }
    }
    while let Some(relative) = pending.pop() {
        let directory = root.join(&relative);
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            // Removed since the snapshot: nothing left to lose.
            Err(error) if error.kind() == ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to read {}", directory.display()));
            }
        };
        for entry in entries {
            let entry = entry.with_context(|| format!("failed to read {}", directory.display()))?;
            let child = relative.join(entry.file_name());
            let file_type = entry
                .file_type()
                .with_context(|| format!("failed to inspect {}", root.join(&child).display()))?;
            if file_type.is_dir() {
                pending.push(child);
            } else if file_type.is_file() {
                check_file(root, &child, reference, &mut found)?;
            }
        }
    }
    found.sort();
    Ok(found)
}

fn check_file(
    root: &Path,
    relative: &Path,
    reference: &Path,
    found: &mut Vec<PathBuf>,
) -> Result<()> {
    let path = root.join(relative);
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.is_file() => {}
        Ok(_) => return Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to inspect {}", path.display()));
        }
    }
    if !has_identical_copy(&path, &reference.join(relative))? {
        found.push(relative.to_path_buf());
    }
    Ok(())
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

/// Summarize files for an error message: one line per top-level entry, collapsing
/// directories into a file count, capped at `limit` lines.
pub(crate) fn summarize(files: &[PathBuf], limit: usize) -> String {
    let mut groups: BTreeMap<PathBuf, Vec<&PathBuf>> = BTreeMap::new();
    for file in files {
        let top = file
            .components()
            .next()
            .map_or_else(PathBuf::new, |component| {
                PathBuf::from(component.as_os_str())
            });
        groups.entry(top).or_default().push(file);
    }
    let mut lines = groups
        .iter()
        .take(limit)
        .map(|(top, members)| match members.as_slice() {
            [only] => format!("  {}", only.display()),
            members => format!("  {}/ ({} files)", top.display(), members.len()),
        })
        .collect::<Vec<_>>();
    if groups.len() > limit {
        lines.push(format!("  … and {} more", groups.len() - limit));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    fn file(path: &str) -> UntrackedPath {
        UntrackedPath {
            path: PathBuf::from(path),
            directory: false,
        }
    }

    #[test]
    fn reports_candidates_without_an_identical_reference_copy() {
        let temp = tempfile::tempdir().unwrap();
        let (root, reference) = (temp.path().join("work"), temp.path().join("default"));
        write(&root.join(".env.local"), "unique secret");
        write(&root.join("shared.local"), "same");
        write(&reference.join("shared.local"), "same");
        write(&root.join("notes/a.md"), "only here");
        write(&root.join("notes/deep/b.md"), "same");
        write(&reference.join("notes/deep/b.md"), "same");
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
        // Not a candidate: ignored or tracked files are never checked.
        write(&root.join("target/debug/app"), "built here");
        #[cfg(unix)]
        std::os::unix::fs::symlink("/nonexistent/shared", root.join("notes/linked")).unwrap();
        let candidates = [
            file(".env.local"),
            file("shared.local"),
            file("cache.local"),
            file("copied.local"),
            file("gone.local"),
            UntrackedPath {
                path: PathBuf::from("notes"),
                directory: true,
            },
        ];

        let found = unpreserved_files(&root, &candidates, &reference).unwrap();

        assert_eq!(
            found,
            [".env.local", "cache.local", "notes/a.md"].map(PathBuf::from)
        );
    }

    #[test]
    fn summarizes_by_top_level_entry_with_a_cap() {
        let files = [".env", "target/a", "target/b", "z/one", "zz"].map(PathBuf::from);
        assert_eq!(
            summarize(&files, 3),
            "  .env\n  target/ (2 files)\n  z/one\n  … and 1 more"
        );
    }
}
