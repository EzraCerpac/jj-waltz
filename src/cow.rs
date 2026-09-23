//! Copy-on-write cloning of checkout files into a new workspace.
//!
//! A clone shares storage with its source until either copy changes: APFS
//! `clonefile(2)` on macOS and `FICLONE` reflinks on Linux filesystems such as
//! Btrfs and XFS. JJ still owns the checkout. The workspace adapter creates the
//! workspace with empty sparse patterns, clones the source's tracked files into
//! it, and lets JJ reconcile them against the creation base.

use anyhow::Result;
use std::path::{Path, PathBuf};

/// Explain why files under `source` cannot be cloned into directories created in
/// `destination_parent`, or return `None` when cloning is available.
///
/// This probes the destination filesystem without touching either checkout, so
/// callers can choose a full checkout before any workspace mutation.
pub(crate) fn unsupported_reason(source: &Path, destination_parent: &Path) -> Option<String> {
    imp::unsupported_reason(source, destination_parent)
}

/// Clone workspace-relative `files` from `source` into the existing `destination`.
///
/// Symlinks are recreated. Files missing from the source are skipped, leaving JJ to
/// check them out, as are paths that are no longer regular files or symlinks.
pub(crate) fn clone_files(source: &Path, files: &[PathBuf], destination: &Path) -> Result<()> {
    imp::clone_files(source, files, destination)
}

#[cfg(unix)]
mod imp {
    use anyhow::{Context, Result};
    use std::collections::HashSet;
    use std::fs::{self, Metadata};
    use std::io::ErrorKind;
    use std::os::unix::fs::{MetadataExt, symlink};
    use std::path::{Path, PathBuf};

    pub(super) fn unsupported_reason(source: &Path, destination_parent: &Path) -> Option<String> {
        let device = |path: &Path| {
            fs::metadata(path)
                .map(|metadata| metadata.dev())
                .map_err(|error| format!("cannot inspect {}: {error}", path.display()))
        };
        match (device(source), device(destination_parent)) {
            (Ok(source_device), Ok(destination_device)) if source_device != destination_device => {
                return Some(format!(
                    "{} and {} are on different filesystems",
                    source.display(),
                    destination_parent.display()
                ));
            }
            (Err(reason), _) | (_, Err(reason)) => return Some(reason),
            _ => {}
        }

        let probe = destination_parent.join(format!(".jw-cow-probe-{}", std::process::id()));
        let probe_clone =
            destination_parent.join(format!(".jw-cow-probe-{}.clone", std::process::id()));
        let result =
            fs::write(&probe, b"probe").and_then(|()| reflink_copy::reflink(&probe, &probe_clone));
        let _ = fs::remove_file(&probe_clone);
        let _ = fs::remove_file(&probe);
        result.err().map(|error| {
            format!(
                "the filesystem at {} cannot clone files: {error}",
                destination_parent.display()
            )
        })
    }

    pub(super) fn clone_files(source: &Path, files: &[PathBuf], destination: &Path) -> Result<()> {
        let mut created_directories = HashSet::new();
        for relative in files {
            let from = source.join(relative);
            let to = destination.join(relative);
            let metadata = match fs::symlink_metadata(&from) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("failed to inspect {}", from.display()));
                }
            };
            let file_type = metadata.file_type();
            if !file_type.is_file() && !file_type.is_symlink() {
                continue;
            }
            if let Some(parent) = to.parent()
                && created_directories.insert(parent.to_path_buf())
            {
                fs::create_dir_all(parent)
                    .with_context(|| format!("failed to create {}", parent.display()))?;
            }
            if file_type.is_symlink() {
                let link = fs::read_link(&from)
                    .with_context(|| format!("failed to read {}", from.display()))?;
                symlink(&link, &to)
                    .with_context(|| format!("failed to create {}", to.display()))?;
            } else {
                reflink_copy::reflink(&from, &to).with_context(|| {
                    format!("failed to clone {} to {}", from.display(), to.display())
                })?;
                preserve_modified_time(&to, &metadata)?;
            }
        }
        Ok(())
    }

    /// `clonefile(2)` clones timestamps along with the data.
    #[cfg(target_os = "macos")]
    fn preserve_modified_time(_target: &Path, _metadata: &Metadata) -> Result<()> {
        Ok(())
    }

    /// A reflink shares extents with a freshly created inode. Restore the source
    /// modification time so JJ can trust the copied working-copy state.
    #[cfg(not(target_os = "macos"))]
    fn preserve_modified_time(target: &Path, metadata: &Metadata) -> Result<()> {
        let modified = metadata.modified()?;
        fs::File::open(target)
            .and_then(|file| file.set_modified(modified))
            .with_context(|| format!("failed to set modification time on {}", target.display()))
    }
}

#[cfg(not(unix))]
mod imp {
    use anyhow::{Result, bail};
    use std::path::{Path, PathBuf};

    pub(super) fn unsupported_reason(_source: &Path, _destination_parent: &Path) -> Option<String> {
        Some("copy-on-write cloning is supported on macOS and Linux only".to_owned())
    }

    pub(super) fn clone_files(
        _source: &Path,
        _files: &[PathBuf],
        _destination: &Path,
    ) -> Result<()> {
        bail!("copy-on-write cloning is supported on macOS and Linux only")
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};

    #[test]
    fn clones_listed_files_and_symlinks_only() {
        let temp = tempfile::tempdir().expect("tempdir");
        let source = temp.path().join("source");
        let destination = temp.path().join("destination");
        if let Some(reason) = unsupported_reason(temp.path(), temp.path()) {
            eprintln!("skipping clone test: {reason}");
            return;
        }

        fs::create_dir_all(source.join("nested/deeper")).unwrap();
        fs::create_dir_all(destination.join(".jj")).unwrap();
        fs::write(source.join("nested/deeper/tool.sh"), "#!/bin/sh\n").unwrap();
        fs::set_permissions(
            source.join("nested/deeper/tool.sh"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        symlink("deeper/tool.sh", source.join("nested/link")).unwrap();
        fs::write(source.join(".env"), "SECRET=1\n").unwrap();
        let files = [
            "nested/deeper/tool.sh",
            "nested/link",
            "deleted.txt",
            "nested",
        ]
        .map(PathBuf::from);

        clone_files(&source, &files, &destination).expect("clone files");

        let tool = destination.join("nested/deeper/tool.sh");
        assert_eq!(fs::read_to_string(&tool).unwrap(), "#!/bin/sh\n");
        assert_eq!(
            fs::metadata(&tool).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert_eq!(
            fs::read_link(destination.join("nested/link")).unwrap(),
            Path::new("deeper/tool.sh")
        );
        assert!(!destination.join(".env").exists());
        assert!(!destination.join("deleted.txt").exists());
    }
}
