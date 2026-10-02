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
        client.run(["workspace", "forget", name])?;
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
