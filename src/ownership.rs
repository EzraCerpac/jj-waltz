//! Checkout lifecycle belongs to the application that created a linked Git worktree.
//! A JJ workspace registration does not transfer that ownership to jw.
use crate::jj::JjClient;
use crate::metadata::{ManagedWorkspaceMetadata, WorkspaceMetadataStore};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalOwner {
    pub checkout_root: PathBuf,
    pub git_dir: PathBuf,
    pub common_dir: PathBuf,
}

/// Provenance is minted only in the creation path, with a reciprocal marker in
/// the newly allocated administrative directory. Path reuse cannot transfer ownership.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OwnedGitWorktree {
    pub topology: ExternalOwner,
    pub ownership_token: String,
}

impl std::ops::Deref for OwnedGitWorktree {
    type Target = ExternalOwner;
    fn deref(&self) -> &Self::Target {
        &self.topology
    }
}

pub fn registration_paths(client: &JjClient) -> Result<Vec<PathBuf>> {
    let backend = client.run(["--ignore-working-copy", "git", "root"])?;
    let directory = PathBuf::from(backend.trimmed_stdout()?)
        .canonicalize()?
        .join("worktrees");
    match fs::read_dir(directory) {
        Ok(entries) => entries.map(|entry| Ok(entry?.path())).collect(),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error).context("cannot snapshot Git registrations"),
    }
}

pub fn record_created(
    client: &JjClient,
    name: &str,
    path: &Path,
    previous: &[PathBuf],
) -> Result<OwnedGitWorktree> {
    use std::io::Write;
    let topology = detect(path)?.context("JJ did not create the requested linked Git worktree")?;
    if previous.contains(&topology.git_dir) {
        bail!("Git registration existed before creation; checkout retained for inspection")
    }
    let ownership_token = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos()
    );
    let owner = OwnedGitWorktree {
        topology,
        ownership_token,
    };
    // Validate topology before placing our marker in a newly allocated registration.
    validate_topology(client, name, Some(path), &owner.topology)?;
    let marker = owner.git_dir.join("jw-owner");
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(marker)?;
    file.write_all(owner.ownership_token.as_bytes())?;
    file.sync_all()?;
    validate_owned(client, name, Some(path), &owner)?;
    Ok(owner)
}

fn git(args: &[&str]) -> Result<String> {
    let mut command = Command::new("git");
    command.args(["--no-optional-locks"]);
    for name in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_CEILING_DIRECTORIES",
    ] {
        command.env_remove(name);
    }
    let output = command
        .args(args)
        .output()
        .context("cannot inspect Git ownership")?;
    if !output.status.success() {
        bail!(
            "cannot verify Git ownership: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    String::from_utf8(output.stdout).context("Git ownership paths are not UTF-8")
}

fn git_path(root: &Path, field: &str) -> Result<PathBuf> {
    let root = root.to_str().context("checkout path is not UTF-8")?;
    let output = git(&["-C", root, "rev-parse", "--path-format=absolute", field])?;
    let path = PathBuf::from(output.trim_end_matches(['\r', '\n']));
    path.canonicalize()
        .with_context(|| format!("cannot resolve Git ownership path {}", path.display()))
}

/// Inspect actual topology; ordinary JJ workspaces and primary Git checkouts are not linked.
pub fn detect(root: &Path) -> Result<Option<ExternalOwner>> {
    match fs::symlink_metadata(root.join(".git")) {
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("cannot inspect checkout .git metadata"),
        Ok(_) => {}
    }
    let git_dir = git_path(root, "--absolute-git-dir")?;
    let common_dir = git_path(root, "--git-common-dir")?;
    if git_dir == common_dir {
        return Ok(None);
    }
    let checkout_root = git_path(root, "--show-toplevel")?;
    if checkout_root != root.canonicalize()? {
        bail!("workspace is nested inside a linked Git checkout; ownership is ambiguous");
    }
    Ok(Some(ExternalOwner {
        checkout_root,
        git_dir,
        common_dir,
    }))
}

fn target_path(client: &JjClient, name: &str) -> Result<Option<PathBuf>> {
    let output =
        client.run_unchecked(["--ignore-working-copy", "workspace", "root", "--name", name])?;
    if output.success() {
        return Ok(Some(PathBuf::from(output.trimmed_stdout()?)));
    }
    let message = output.stderr();
    if message.contains("Workspace has no recorded path") {
        return Ok(None);
    }
    if let Some(path) = message.lines().find_map(|line| {
        line.trim()
            .strip_prefix("Error: ")
            .unwrap_or(line.trim())
            .strip_prefix("Cannot resolve absolute workspace path: ")
    }) {
        return Ok(Some(PathBuf::from(path)));
    }
    bail!("cannot verify workspace ownership for {name}: {message}")
}

// Canonicalize existing ancestors too: stale /tmp and /private/tmp spellings
// must compare equal after their checkout directory has disappeared.
fn normalized_path(path: &Path) -> Result<PathBuf> {
    if let Ok(path) = path.canonicalize() {
        return Ok(path);
    }
    let parent = path.parent().context("cannot normalize ownership path")?;
    let name = path.file_name().context("ownership path has no filename")?;
    Ok(normalized_path(parent)?.join(name))
}

fn same_path(left: &Path, right: &Path) -> Result<bool> {
    Ok(normalized_path(left)? == normalized_path(right)?)
}

fn registrations(client: &JjClient) -> Result<Vec<PathBuf>> {
    let output = client.run_unchecked(["--ignore-working-copy", "git", "root"])?;
    if !output.success() {
        let report = crate::context::discover(Some(client.cwd()));
        if report.jj.repository_path.as_ref().is_some_and(|repo| {
            fs::read_to_string(repo.join("store/type")).is_ok_and(|kind| kind.trim() == "local")
        }) {
            return Ok(Vec::new());
        }
        bail!("cannot verify Git registrations: {}", output.stderr());
    }
    let backend = output.trimmed_stdout()?;
    let output = git(&[
        "--git-dir",
        &backend,
        "worktree",
        "list",
        "--porcelain",
        "-z",
    ])?;
    // The first record is the primary repository, not a linked registration.
    Ok(output
        .split("\0\0")
        .skip(1)
        .filter_map(|record| {
            record
                .split('\0')
                .find_map(|field| field.strip_prefix("worktree ").map(PathBuf::from))
        })
        .collect())
}

pub fn ensure_removal_allowed(
    client: &JjClient,
    name: &str,
    path: Option<&Path>,
    metadata: Option<&ManagedWorkspaceMetadata>,
) -> Result<()> {
    if metadata
        .and_then(|record| record.external_owner.as_ref())
        .is_some()
    {
        bail!(
            "workspace {name} is externally owned; use the owning app to remove its checkout and Git registration, then `jw reconcile-external {name}`"
        );
    }
    if let Some(owner) = metadata.and_then(|record| record.owned_git_worktree.as_ref()) {
        validate_owned(client, name, path, owner)?;
        return Ok(());
    }
    let queried_path = target_path(client, name)?;
    if let (Some(expected), Some(current)) = (path, queried_path.as_deref())
        && !same_path(expected, current)?
    {
        bail!("workspace path changed during ownership validation; review it again");
    }
    let path = queried_path.as_deref().or(path);
    if let Some(path) = path {
        match fs::symlink_metadata(path) {
            Ok(_) => {
                if detect(path)?.is_some() {
                    bail!(
                        "workspace {name} is externally owned by a linked Git checkout; jw refuses to forget or remove it (including --keep-dir)"
                    );
                }
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("cannot verify checkout ownership"),
        }
        for registered in registrations(client)? {
            if same_path(&registered, path)? {
                bail!(
                    "workspace {name} still has an external Git worktree registration; use the owning app to remove it"
                );
            }
        }
    } else if !registrations(client)?.is_empty() {
        bail!(
            "workspace {name} has no recorded path and linked Git worktrees exist; cannot prove ownership safely"
        );
    }
    Ok(())
}

/// A stored marker alone never authorizes deleting replacement topology.
fn validate_owned(
    client: &JjClient,
    name: &str,
    path: Option<&Path>,
    owner: &OwnedGitWorktree,
) -> Result<()> {
    validate_topology(client, name, path, &owner.topology)?;
    validate_admin(owner)
}

fn validate_topology(
    client: &JjClient,
    name: &str,
    path: Option<&Path>,
    owner: &ExternalOwner,
) -> Result<()> {
    let queried = target_path(client, name)?.context("owned workspace has no recorded path")?;
    if !same_path(&queried, &owner.checkout_root)?
        || path.is_some_and(|path| same_path(path, &owner.checkout_root).ok() != Some(true))
    {
        bail!("owned workspace path changed; refusing cleanup")
    }
    let backend = client.run(["--ignore-working-copy", "git", "root"])?;
    if PathBuf::from(backend.trimmed_stdout()?).canonicalize()? != owner.common_dir {
        bail!("owned Git repository changed; refusing cleanup")
    }
    validate_admin_topology(owner)?;
    match fs::symlink_metadata(&owner.checkout_root) {
        Ok(metadata) => {
            if !metadata.is_dir() || detect(&owner.checkout_root)?.as_ref() != Some(owner) {
                bail!("owned Git topology changed; refusing cleanup")
            }
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("cannot inspect owned checkout"),
    }
    Ok(())
}

fn validate_admin(owner: &OwnedGitWorktree) -> Result<()> {
    validate_admin_topology(&owner.topology)?;
    let marker = owner.git_dir.join("jw-owner");
    if !fs::symlink_metadata(&marker)?.file_type().is_file()
        || fs::read_to_string(marker)? != owner.ownership_token
    {
        bail!("owned Git registration marker changed; refusing cleanup")
    }
    Ok(())
}

fn validate_admin_topology(owner: &ExternalOwner) -> Result<()> {
    if owner.git_dir.parent() != Some(owner.common_dir.join("worktrees").as_path()) {
        bail!("invalid owned Git registration path")
    }
    let metadata = fs::symlink_metadata(&owner.git_dir)
        .context("owned Git registration is missing; refusing cleanup")?;
    if !metadata.is_dir() || owner.git_dir.canonicalize()? != owner.git_dir {
        bail!("owned Git registration was replaced; refusing cleanup")
    }
    let back = fs::read_to_string(owner.git_dir.join("gitdir"))?;
    let common = fs::read_to_string(owner.git_dir.join("commondir"))?;
    if !same_path(
        &owner.git_dir.join(back.trim_end_matches(['\r', '\n'])),
        &owner.checkout_root.join(".git"),
    )? || owner
        .git_dir
        .join(common.trim_end_matches(['\r', '\n']))
        .canonicalize()?
        != owner.common_dir
    {
        bail!("owned Git registration topology changed; refusing cleanup")
    }
    if owner.git_dir.join("locked").exists() {
        bail!("owned Git worktree is locked; unlock it before cleanup")
    }
    Ok(())
}

/// All forget operations pass this boundary. JJ 0.46 unlinks `.git` then runs a
/// global worktree prune, so detach only our validated gitlink and delete only
/// our exact administrative directory. Never invoke global Git cleanup.
pub fn forget_workspace(
    client: &JjClient,
    name: &str,
    path: Option<&Path>,
    metadata: Option<&ManagedWorkspaceMetadata>,
) -> Result<()> {
    ensure_removal_allowed(client, name, path, metadata)?;
    let resolved = target_path(client, name)?;
    forget_validated(
        client,
        name,
        resolved.as_deref().or(path),
        metadata.and_then(|m| m.owned_git_worktree.as_ref()),
    )
}

/// In-memory provenance authorizes rollback before the lifecycle record is persisted.
pub fn forget_created(
    client: &JjClient,
    name: &str,
    path: &Path,
    owner: Option<&OwnedGitWorktree>,
) -> Result<()> {
    if let Some(owner) = owner {
        validate_owned(client, name, Some(path), owner)?;
    } else {
        ensure_removal_allowed(client, name, Some(path), None)?;
    }
    forget_validated(client, name, Some(path), owner)
}

fn forget_validated(
    client: &JjClient,
    name: &str,
    path: Option<&Path>,
    owner: Option<&OwnedGitWorktree>,
) -> Result<()> {
    let gitlink = path.map(|p| p.join(".git"));
    let backup = path.map(|p| p.join(".jj/jw-detached-git"));
    let mut detached = false;
    if let Some(link) = &gitlink {
        match fs::symlink_metadata(link) {
            Ok(metadata) if owner.is_none() => {
                // A directory .git is ignored by JJ unlink; unowned gitlinks are unsafe.
                if !metadata.file_type().is_dir() {
                    bail!("unowned Git link; refusing workspace forget")
                }
            }
            Ok(metadata) => {
                let owner = owner.unwrap();
                validate_owned(client, name, path, owner)?;
                if !metadata.file_type().is_file() {
                    bail!("owned Git link is not a regular file; refusing cleanup")
                }
                let backup = backup.as_ref().unwrap();
                ensure_absent(backup)?;
                let original = fs::read(link)?;
                let text = std::str::from_utf8(&original).context("Git link is not UTF-8")?;
                let target = text
                    .trim_end_matches(['\r', '\n'])
                    .strip_prefix("gitdir: ")
                    .context("invalid owned Git link")?;
                if path.unwrap().join(target).canonicalize()? != owner.git_dir
                    || !fs::symlink_metadata(path.unwrap().join(".jj"))?
                        .file_type()
                        .is_dir()
                {
                    bail!("owned checkout topology changed before detach; refusing cleanup")
                }
                rename_no_replace(link, backup).context("cannot detach owned Git link")?;
                detached = true;
                if fs::read(backup)? != original {
                    restore_gitlink(backup, link)?;
                    bail!("Git link changed during detach; cleanup refused")
                }
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("cannot inspect Git link before forget"),
        }
    }
    // Nonzero status need not mean no mutation. Query the result before restoring
    // the link or deleting anything, and treat a confirmed completed forget as success.
    let outcome = client.run(["workspace", "forget", name]);
    let remaining = match client.workspace_names() {
        Ok(names) => names.iter().any(|candidate| candidate == name),
        Err(error) => {
            let detail = if detached {
                format!(
                    "; detached Git link retained at {}",
                    backup.as_ref().unwrap().display()
                )
            } else {
                String::new()
            };
            return Err(error.context(format!(
                "cannot verify forget outcome; checkout, registration and metadata retained{detail}"
            )));
        }
    };
    if remaining {
        if detached {
            validate_admin(owner.unwrap()).context(
                "forget did not complete and Git registration changed; detached link retained",
            )?;
            restore_gitlink(backup.as_ref().unwrap(), gitlink.as_ref().unwrap())?;
        }
        return match outcome {
            Err(error) => Err(error),
            Ok(_) => Err(anyhow::anyhow!(
                "workspace forget did not remove {name}; checkout retained"
            )),
        };
    }
    if let Err(error) = outcome {
        eprintln!("Warning: {error}; verified workspace {name} was forgotten, completing cleanup");
    }
    if let Some(owner) = owner {
        validate_admin(owner)?;
        // Rename the exact registration out of its recorded name, revalidate it,
        // then delete it. Never chase a registration that replaced that name.
        let staged_path = owner
            .common_dir
            .join("worktrees")
            .join(format!(".jw-cleanup-{}", owner.ownership_token));
        ensure_absent(&staged_path)?;
        let identity = fs::symlink_metadata(&owner.git_dir)?;
        rename_no_replace(&owner.git_dir, &staged_path)
            .context("workspace forgotten but owned Git registration could not be detached")?;
        let mut staged = owner.clone();
        staged.topology.git_dir = staged_path.clone();
        if let Err(error) = validate_staged_admin(&staged, &identity) {
            rename_no_replace(&staged_path, &owner.git_dir).with_context(|| {
                format!(
                    "Git registration changed during detach; could not restore {}; retained at {}",
                    owner.git_dir.display(),
                    staged_path.display()
                )
            })?;
            return Err(error
                .context("Git registration changed during detach; restored without deleting it"));
        }
        ensure_absent(&owner.git_dir)
            .context("Git registration was replaced during cleanup; staged cleanup retained")?;
        fs::remove_dir_all(&staged_path)
            .context("workspace forgotten but staged Git registration cleanup failed")?;
        if detached {
            fs::remove_file(backup.as_ref().unwrap())
                .context("workspace forgotten but detached Git link cleanup failed")?;
        }
        if owner.git_dir.exists()
            || registrations(client)?
                .iter()
                .any(|p| same_path(p, &owner.checkout_root).ok() == Some(true))
        {
            bail!("workspace forgotten but owned Git registration remains")
        }
    }
    Ok(())
}

fn ensure_absent(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).context("cannot inspect cleanup destination"),
        Ok(_) => bail!("cleanup destination already exists: {}", path.display()),
    }
}

fn restore_gitlink(backup: &Path, link: &Path) -> Result<()> {
    ensure_absent(link).context("Git link was replaced; detached link retained for inspection")?;
    rename_no_replace(backup, link).context("Git link could not be restored")
}

fn validate_staged_admin(owner: &OwnedGitWorktree, original: &fs::Metadata) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let staged = fs::symlink_metadata(&owner.git_dir)?;
        if staged.dev() != original.dev() || staged.ino() != original.ino() {
            bail!("Git registration identity changed during detach")
        }
    }
    #[cfg(not(unix))]
    let _ = original;
    validate_admin(owner)
}

// Both detach and restore must atomically refuse an occupied destination. A
// prior existence check followed by std::fs::rename can overwrite replacements.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn rename_no_replace(source: &Path, destination: &Path) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let source = CString::new(source.as_os_str().as_bytes())?;
    let destination = CString::new(destination.as_os_str().as_bytes())?;
    #[cfg(target_os = "macos")]
    unsafe extern "C" {
        fn renamex_np(
            source: *const std::ffi::c_char,
            destination: *const std::ffi::c_char,
            flags: u32,
        ) -> i32;
    }
    #[cfg(target_os = "linux")]
    unsafe extern "C" {
        fn renameat2(
            source_fd: i32,
            source: *const std::ffi::c_char,
            destination_fd: i32,
            destination: *const std::ffi::c_char,
            flags: u32,
        ) -> i32;
    }
    // SAFETY: These C strings are NUL-terminated and live through the call.
    // macOS RENAME_EXCL=4; Linux AT_FDCWD=-100 and RENAME_NOREPLACE=1.
    #[cfg(target_os = "macos")]
    let result = unsafe { renamex_np(source.as_ptr(), destination.as_ptr(), 4) };
    #[cfg(target_os = "linux")]
    let result = unsafe { renameat2(-100, source.as_ptr(), -100, destination.as_ptr(), 1) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(windows)]
fn rename_no_replace(source: &Path, destination: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        #[link_name = "MoveFileExW"]
        fn move_file_ex_w(source: *const u16, destination: *const u16, flags: u32) -> i32;
    }
    let source: Vec<_> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let destination: Vec<_> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    // SAFETY: Both paths are NUL-terminated and remain alive for the call. Zero
    // flags disallow replacement of an existing file or directory.
    let result = unsafe { move_file_ex_w(source.as_ptr(), destination.as_ptr(), 0) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn rename_no_replace(_source: &Path, _destination: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "safe exclusive rename is unavailable on this platform",
    ))
}

/// Explicitly forget only a stale externally-owned JJ registration and lifecycle record.
/// Never remove files, Git registrations, commits, or bookmarks here.
pub fn reconcile(name: &str) -> Result<()> {
    if name.is_empty() || ["@", "-", "^"].contains(&name) {
        bail!("reconciliation requires a literal workspace name");
    }
    let client = JjClient::current()?;
    let store = WorkspaceMetadataStore::from_repo_config_path(client.repo_config_path()?)?;
    let metadata = store
        .get(name)?
        .context("no external ownership record; restore its metadata before reconciliation")?;
    let owner = metadata
        .external_owner
        .as_ref()
        .context("workspace has no external ownership record")?;
    validate_gone(&client, name, owner)?;
    if store.get(name)?.as_ref() != Some(&metadata) {
        bail!("workspace metadata changed during reconciliation");
    }
    // Recheck immediately before the destructive boundary.
    validate_gone(&client, name, owner)?;
    if client
        .workspace_names()?
        .iter()
        .any(|candidate| candidate == name)
    {
        forget_workspace(&client, name, Some(&owner.checkout_root), None)?;
    }
    if !store.remove_if_matches(&metadata)? {
        bail!("workspace metadata changed during reconciliation and was retained");
    }
    Ok(())
}

fn validate_gone(client: &JjClient, name: &str, owner: &ExternalOwner) -> Result<()> {
    for path in [&owner.checkout_root, &owner.git_dir] {
        match fs::symlink_metadata(path) {
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("cannot verify external checkout cleanup"),
            Ok(_) => bail!(
                "external checkout or Git registration still exists: {}",
                path.display()
            ),
        }
    }
    for path in registrations(client)? {
        if same_path(&path, &owner.checkout_root)? {
            bail!("external Git registration still exists for {name}");
        }
    }
    if client
        .workspace_names()?
        .iter()
        .any(|candidate| candidate == name)
    {
        match target_path(client, name)? {
            Some(path) if same_path(&path, &owner.checkout_root)? => {}
            _ => bail!("JJ workspace path changed; refusing external reconciliation"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod cleanup_tests {
    use super::*;

    #[test]
    fn exclusive_rename_preserves_occupied_files_and_directories() {
        let temp = tempfile::tempdir().unwrap();
        for directory in [false, true] {
            let source = temp
                .path()
                .join(if directory { "source-dir" } else { "source" });
            let destination = temp
                .path()
                .join(if directory { "dest-dir" } else { "dest" });
            if directory {
                fs::create_dir(&source).unwrap();
                fs::create_dir(&destination).unwrap();
            } else {
                fs::write(&source, "source").unwrap();
                fs::write(&destination, "destination").unwrap();
            }
            assert!(rename_no_replace(&source, &destination).is_err());
            assert!(source.exists());
            assert!(destination.exists());
            if !directory {
                assert_eq!(fs::read_to_string(&destination).unwrap(), "destination");
            }
            let unused = temp
                .path()
                .join(if directory { "unused-dir" } else { "unused" });
            rename_no_replace(&source, &unused).unwrap();
            assert!(!source.exists());
            assert!(unused.exists());
        }
    }

    #[cfg(unix)]
    #[test]
    fn staged_registration_rejects_replacement_identity() {
        let temp = tempfile::tempdir().unwrap();
        let original = temp.path().join("original");
        let replacement = temp.path().join("replacement");
        fs::create_dir(&original).unwrap();
        fs::create_dir(&replacement).unwrap();
        let identity = fs::symlink_metadata(&original).unwrap();
        let owner = OwnedGitWorktree {
            topology: ExternalOwner {
                checkout_root: temp.path().join("checkout"),
                git_dir: replacement,
                common_dir: temp.path().join("common"),
            },
            ownership_token: "test-token".into(),
        };
        assert!(
            validate_staged_admin(&owner, &identity)
                .unwrap_err()
                .to_string()
                .contains("identity changed")
        );
    }
}
