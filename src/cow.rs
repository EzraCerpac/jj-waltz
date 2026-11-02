//! Copy-on-write cloning of checkout files into a new workspace.
//!
//! A clone shares storage with its source until either copy changes: APFS
//! `clonefile(2)` on macOS and `FICLONE` reflinks on Linux filesystems such as
//! Btrfs and XFS. JJ still owns the checkout. The workspace adapter creates the
//! workspace with empty sparse patterns, clones the selected source entries into
//! it, and lets JJ reconcile them against the creation base.
//!
//! The clone is planned as a set of maximal subtrees: a directory whose every entry
//! is selected is cloned with one call, which `clonefile(2)` performs in the kernel.
//! Only directories that mix selected and unselected entries are walked further.

use anyhow::Result;
use std::path::{Path, PathBuf};

/// Which source entries a copy-on-write clone copies.
#[derive(Debug, Clone, Copy)]
pub(crate) enum CloneSelection<'a> {
    /// Only these workspace-relative paths that JJ tracks.
    Tracked(&'a [PathBuf]),
    /// Every entry, including ignored and untracked files, except JJ and Git
    /// metadata and these workspace-relative paths.
    All { excluded: &'a [PathBuf] },
}

/// Explain why files under `source` cannot be cloned into directories created in
/// `destination_parent`, or return `None` when cloning is available.
///
/// This probes the destination filesystem without touching either checkout, so
/// callers can choose a full checkout before any workspace mutation.
pub(crate) fn unsupported_reason(source: &Path, destination_parent: &Path) -> Option<String> {
    imp::unsupported_reason(source, destination_parent)
}

/// Clone the selected entries of `source` into the existing `destination`.
///
/// Symlinks are recreated, never followed. Selected paths missing from the source
/// are skipped, leaving JJ to check them out, as are entries that are neither
/// regular files, symlinks, nor directories. Existing destination entries, such as
/// the new workspace's `.jj`, are never selected.
pub(crate) fn clone_into(
    source: &Path,
    selection: CloneSelection<'_>,
    destination: &Path,
) -> Result<()> {
    imp::clone_into(source, selection, destination)
}

#[cfg(unix)]
mod imp {
    use super::CloneSelection;
    use anyhow::{Context, Result};
    use std::collections::HashSet;
    use std::fs::{self, FileType, Metadata};
    use std::io::ErrorKind;
    use std::os::unix::fs::{MetadataExt, symlink};
    use std::path::{Path, PathBuf};

    /// Never cloned in either mode: the new workspace has its own JJ state, and
    /// copy-on-write workspaces are JJ-only.
    const METADATA_DIRS: [&str; 2] = [".jj", ".git"];

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

        // Exclusive creation and mode 0700 keep both files private. TempDir only
        // cleans up the directory it created, never a caller's existing paths.
        let directory = match tempfile::Builder::new()
            .prefix(".jw-cow-probe-")
            .tempdir_in(destination_parent)
        {
            Ok(directory) => directory,
            Err(error) => return Some(format!("cannot create private clone probe: {error}")),
        };
        let probe = directory.path().join("source");
        let probe_clone = directory.path().join("clone");
        let result =
            fs::write(&probe, b"probe").and_then(|()| reflink_copy::reflink(&probe, &probe_clone));
        result.err().map(|error| {
            format!(
                "the filesystem at {} cannot clone files: {error}",
                destination_parent.display()
            )
        })
    }

    pub(super) fn clone_into(
        source: &Path,
        selection: CloneSelection<'_>,
        destination: &Path,
    ) -> Result<()> {
        let selector = Selector::new(selection);
        let (_, ops) = selector.plan(source, Path::new(""))?;
        let mut created_directories = HashSet::new();
        for op in ops {
            let relative = op.path();
            let from = source.join(relative);
            let to = destination.join(relative);
            if let Some(parent) = to.parent()
                && created_directories.insert(parent.to_path_buf())
            {
                fs::create_dir_all(parent)
                    .with_context(|| format!("failed to create {}", parent.display()))?;
            }
            match op {
                Op::Symlink(_) => copy_symlink(&from, &to)?,
                Op::File(_) => clone_file(&from, &to)?,
                Op::Directory(_) => {
                    clone_directory(&from, &to)?;
                    selector.prune(destination, relative)?;
                }
            }
        }
        Ok(())
    }

    #[derive(Debug, PartialEq, Eq)]
    enum Op {
        File(PathBuf),
        Symlink(PathBuf),
        /// Every entry below is selected, so the whole subtree is cloned at once.
        Directory(PathBuf),
    }

    impl Op {
        fn path(&self) -> &Path {
            match self {
                Op::File(path) | Op::Symlink(path) | Op::Directory(path) => path,
            }
        }
    }

    enum Decision {
        Take,
        Skip,
        Descend,
    }

    struct Selector {
        all: bool,
        /// Tracked mode: selected files. All mode: unused.
        files: HashSet<PathBuf>,
        /// Paths never cloned.
        excluded: HashSet<PathBuf>,
        /// Directories that must be walked rather than taken or skipped whole:
        /// ancestors of tracked files, or ancestors of excluded paths.
        walk: HashSet<PathBuf>,
    }

    impl Selector {
        fn new(selection: CloneSelection<'_>) -> Self {
            let mut excluded = METADATA_DIRS
                .iter()
                .map(PathBuf::from)
                .collect::<HashSet<_>>();
            let (all, files) = match selection {
                CloneSelection::Tracked(files) => (false, files.iter().cloned().collect()),
                CloneSelection::All { excluded: extra } => {
                    excluded.extend(extra.iter().cloned());
                    (true, HashSet::new())
                }
            };
            let mut walk = HashSet::new();
            let anchors: Box<dyn Iterator<Item = &PathBuf>> = if all {
                Box::new(excluded.iter())
            } else {
                Box::new(files.iter())
            };
            for path in anchors {
                for ancestor in path.ancestors().skip(1) {
                    if ancestor.as_os_str().is_empty() || !walk.insert(ancestor.to_path_buf()) {
                        break;
                    }
                }
            }
            Self {
                all,
                files,
                excluded,
                walk,
            }
        }

        fn decide(&self, relative: &Path, file_type: FileType) -> Decision {
            if self.excluded.contains(relative) {
                Decision::Skip
            } else if file_type.is_dir() {
                if self.walk.contains(relative) {
                    Decision::Descend
                } else if self.all {
                    Decision::Take
                } else {
                    Decision::Skip
                }
            } else if (file_type.is_file() || file_type.is_symlink())
                && (self.all || self.files.contains(relative))
            {
                Decision::Take
            } else {
                Decision::Skip
            }
        }

        /// Plan the clone of `relative`. Returns whether every entry below it is
        /// selected, in which case the caller clones the directory whole and the
        /// returned operations are discarded.
        fn plan(&self, source: &Path, relative: &Path) -> Result<(bool, Vec<Op>)> {
            let directory = source.join(relative);
            let entries = fs::read_dir(&directory)
                .with_context(|| format!("failed to read {}", directory.display()))?;
            let mut complete = true;
            let mut ops = Vec::new();
            for entry in entries {
                let entry =
                    entry.with_context(|| format!("failed to read {}", directory.display()))?;
                let child = relative.join(entry.file_name());
                let file_type = entry.file_type().with_context(|| {
                    format!("failed to inspect {}", source.join(&child).display())
                })?;
                match self.decide(&child, file_type) {
                    Decision::Skip => complete = false,
                    Decision::Take if file_type.is_symlink() => ops.push(Op::Symlink(child)),
                    Decision::Take if file_type.is_dir() => ops.push(Op::Directory(child)),
                    Decision::Take => ops.push(Op::File(child)),
                    Decision::Descend => match self.plan(source, &child)? {
                        (true, _) => ops.push(Op::Directory(child)),
                        (false, nested) => {
                            complete = false;
                            ops.extend(nested);
                        }
                    },
                }
            }
            Ok((complete, ops))
        }
    }

    impl Selector {
        /// Remove what a whole-directory clone of `relative` picked up beyond the
        /// selection.
        ///
        /// Planning saw every entry below `relative` selected, but the source can
        /// change before the clone runs. An ignored file created in between would
        /// otherwise survive JJ's reconciliation. Only the new workspace is touched.
        fn prune(&self, destination: &Path, relative: &Path) -> Result<()> {
            let directory = destination.join(relative);
            let entries = fs::read_dir(&directory)
                .with_context(|| format!("failed to read {}", directory.display()))?;
            for entry in entries {
                let entry =
                    entry.with_context(|| format!("failed to read {}", directory.display()))?;
                let child = relative.join(entry.file_name());
                let path = destination.join(&child);
                let file_type = entry
                    .file_type()
                    .with_context(|| format!("failed to inspect {}", path.display()))?;
                match self.decide(&child, file_type) {
                    Decision::Take => {}
                    Decision::Descend => self.prune(destination, &child)?,
                    Decision::Skip if file_type.is_dir() => fs::remove_dir_all(&path)
                        .with_context(|| format!("failed to remove {}", path.display()))?,
                    Decision::Skip => fs::remove_file(&path)
                        .with_context(|| format!("failed to remove {}", path.display()))?,
                }
            }
            Ok(())
        }
    }

    #[cfg(test)]
    pub(super) fn prune_for_test(
        selection: CloneSelection<'_>,
        destination: &Path,
        relative: &Path,
    ) {
        Selector::new(selection)
            .prune(destination, relative)
            .expect("prune");
    }

    fn copy_symlink(from: &Path, to: &Path) -> Result<()> {
        let link =
            fs::read_link(from).with_context(|| format!("failed to read {}", from.display()))?;
        symlink(&link, to).with_context(|| format!("failed to create {}", to.display()))
    }

    fn clone_file(from: &Path, to: &Path) -> Result<()> {
        let metadata = match fs::symlink_metadata(from) {
            Ok(metadata) => metadata,
            // Raced with an edit in the source; JJ checks the path out instead.
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(error).with_context(|| format!("failed to inspect {}", from.display()));
            }
        };
        reflink_copy::reflink(from, to)
            .with_context(|| format!("failed to clone {} to {}", from.display(), to.display()))?;
        preserve_modified_time(to, &metadata)
    }

    /// `clonefile(2)` clones a whole directory hierarchy, including symlinks as
    /// symlinks, with one system call.
    #[cfg(target_os = "macos")]
    fn clone_directory(from: &Path, to: &Path) -> Result<()> {
        reflink_copy::reflink(from, to)
            .with_context(|| format!("failed to clone {} to {}", from.display(), to.display()))
    }

    /// Reflinks only apply to regular files, so recreate the hierarchy entry by entry.
    #[cfg(not(target_os = "macos"))]
    fn clone_directory(from: &Path, to: &Path) -> Result<()> {
        fs::create_dir(to).with_context(|| format!("failed to create {}", to.display()))?;
        let entries =
            fs::read_dir(from).with_context(|| format!("failed to read {}", from.display()))?;
        for entry in entries {
            let entry = entry.with_context(|| format!("failed to read {}", from.display()))?;
            let (from, to) = (entry.path(), to.join(entry.file_name()));
            let file_type = entry
                .file_type()
                .with_context(|| format!("failed to inspect {}", from.display()))?;
            if file_type.is_symlink() {
                copy_symlink(&from, &to)?;
            } else if file_type.is_dir() {
                clone_directory(&from, &to)?;
            } else if file_type.is_file() {
                clone_file(&from, &to)?;
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
    use super::CloneSelection;
    use anyhow::{Result, bail};
    use std::path::Path;

    pub(super) fn unsupported_reason(_source: &Path, _destination_parent: &Path) -> Option<String> {
        Some("copy-on-write cloning is supported on macOS and Linux only".to_owned())
    }

    pub(super) fn clone_into(
        _source: &Path,
        _selection: CloneSelection<'_>,
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
            assert!(
                std::env::var_os("JW_TEST_REQUIRE_COW").is_none_or(|value| value.is_empty()),
                "real cloning required: {reason}"
            );
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

        clone_into(&source, CloneSelection::Tracked(&files), &destination).expect("clone files");

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
        // Sharing storage must never couple later edits or share an inode.
        fs::write(&tool, "destination edit\n").unwrap();
        assert_eq!(
            fs::read_to_string(source.join("nested/deeper/tool.sh")).unwrap(),
            "#!/bin/sh\n"
        );
        fs::write(source.join("nested/deeper/tool.sh"), "source edit\n").unwrap();
        assert_eq!(fs::read_to_string(&tool).unwrap(), "destination edit\n");
    }

    /// Return a scratch directory, or `None` when this filesystem cannot clone.
    fn clone_scratch() -> Option<tempfile::TempDir> {
        let temp = tempfile::tempdir().expect("tempdir");
        if let Some(reason) = unsupported_reason(temp.path(), temp.path()) {
            assert!(
                std::env::var_os("JW_TEST_REQUIRE_COW").is_none_or(|value| value.is_empty()),
                "real cloning required: {reason}"
            );
            eprintln!("skipping clone test: {reason}");
            return None;
        }
        Some(temp)
    }

    fn write(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    #[test]
    fn tracked_clone_keeps_untracked_entries_out_of_mixed_directories() {
        let Some(temp) = clone_scratch() else { return };
        let (source, destination) = (temp.path().join("source"), temp.path().join("dest"));
        write(&source.join("src/pure/a.rs"), "a");
        write(&source.join("src/pure/deep/b.rs"), "b");
        write(&source.join("src/mixed/c.rs"), "c");
        write(&source.join("src/mixed/generated.js"), "untracked");
        write(&source.join("node_modules/pkg/index.js"), "ignored");
        write(&source.join(".jj/repo"), "source metadata");
        fs::create_dir_all(destination.join(".jj")).unwrap();
        let files = ["src/pure/a.rs", "src/pure/deep/b.rs", "src/mixed/c.rs"].map(PathBuf::from);

        clone_into(&source, CloneSelection::Tracked(&files), &destination).unwrap();

        for file in &files {
            assert!(destination.join(file).is_file(), "{}", file.display());
        }
        assert!(!destination.join("src/mixed/generated.js").exists());
        assert!(!destination.join("node_modules").exists());
        assert!(!destination.join(".jj/repo").exists());
    }

    #[test]
    fn whole_checkout_clone_skips_metadata_and_excluded_paths() {
        let Some(temp) = clone_scratch() else { return };
        let (source, destination) = (temp.path().join("source"), temp.path().join("dest"));
        write(&source.join("src/lib.rs"), "tracked");
        write(&source.join("node_modules/pkg/index.js"), "ignored");
        write(&source.join(".env"), "SECRET=1");
        write(&source.join("config/shared/linked.toml"), "link source");
        write(&source.join("config/local.toml"), "kept");
        write(&source.join(".jj/repo"), "source metadata");
        write(&source.join(".git"), "gitdir: elsewhere");
        symlink("src/lib.rs", source.join("lib-link")).unwrap();
        fs::create_dir_all(destination.join(".jj")).unwrap();
        let excluded = [PathBuf::from("config/shared")];

        clone_into(
            &source,
            CloneSelection::All {
                excluded: &excluded,
            },
            &destination,
        )
        .unwrap();

        assert_eq!(
            fs::read_to_string(destination.join("node_modules/pkg/index.js")).unwrap(),
            "ignored"
        );
        assert_eq!(
            fs::read_to_string(destination.join(".env")).unwrap(),
            "SECRET=1"
        );
        assert_eq!(
            fs::read_to_string(destination.join("config/local.toml")).unwrap(),
            "kept"
        );
        assert_eq!(
            fs::read_link(destination.join("lib-link")).unwrap(),
            Path::new("src/lib.rs")
        );
        assert!(!destination.join("config/shared").exists());
        assert!(!destination.join(".jj/repo").exists());
        assert!(!destination.join(".git").exists());
        // Whole-directory clones must stay independent copies.
        fs::write(destination.join("node_modules/pkg/index.js"), "edited").unwrap();
        assert_eq!(
            fs::read_to_string(source.join("node_modules/pkg/index.js")).unwrap(),
            "ignored"
        );
    }

    #[test]
    fn whole_directory_clone_prunes_entries_outside_the_tracked_selection() {
        // Simulates an ignored file created after planning chose to clone `src`
        // whole: the clone contains it, and pruning must take it back out.
        let temp = tempfile::tempdir().expect("tempdir");
        let destination = temp.path().join("dest");
        write(&destination.join("src/lib.rs"), "tracked");
        write(&destination.join("src/nested/mod.rs"), "tracked");
        write(
            &destination.join("src/late.log"),
            "ignored, created after planning",
        );
        write(&destination.join("src/cache/blob"), "ignored directory");
        let files = ["src/lib.rs", "src/nested/mod.rs"].map(PathBuf::from);

        imp::prune_for_test(
            CloneSelection::Tracked(&files),
            &destination,
            Path::new("src"),
        );

        assert!(destination.join("src/lib.rs").is_file());
        assert!(destination.join("src/nested/mod.rs").is_file());
        assert!(!destination.join("src/late.log").exists());
        assert!(!destination.join("src/cache").exists());
    }

    #[test]
    fn probe_preserves_existing_files_symlinks_and_directories() {
        let temp = tempfile::tempdir().unwrap();
        let victim = temp.path().join("victim");
        fs::write(&victim, "untouched").unwrap();
        let legacy = temp
            .path()
            .join(format!(".jw-cow-probe-{}", std::process::id()));
        let legacy_clone = legacy.with_extension("clone");
        symlink(&victim, &legacy).unwrap();
        fs::write(&legacy_clone, "caller-owned").unwrap();
        let other = temp.path().join(".jw-cow-probe-existing");
        fs::create_dir(&other).unwrap();
        fs::write(other.join("source"), "also caller-owned").unwrap();
        let before = fs::read_dir(temp.path()).unwrap().count();
        unsupported_reason(temp.path(), temp.path());
        assert_eq!(fs::read_to_string(&victim).unwrap(), "untouched");
        assert_eq!(fs::read_link(&legacy).unwrap(), victim);
        assert_eq!(fs::read_to_string(&legacy_clone).unwrap(), "caller-owned");
        assert_eq!(
            fs::read_to_string(other.join("source")).unwrap(),
            "also caller-owned"
        );
        assert_eq!(fs::read_dir(temp.path()).unwrap().count(), before);
    }
}
