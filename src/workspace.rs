use anyhow::{Context, Result, anyhow, bail};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const PREVIOUS_WORKSPACE_FILE: &str = "jw-prev-workspace";
const WORKSPACE_BOOKMARK_FILE: &str = "jw-bookmark";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceEntry {
    pub name: String,
    pub root: Option<PathBuf>,
    pub is_current: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwitchResult {
    pub workspace: String,
    pub path: PathBuf,
    pub created: bool,
    pub bookmark: Option<String>,
    pub relative_subdir: Option<PathBuf>,
    from_workspace: String,
    from_path: PathBuf,
}

#[derive(Debug, Clone, Default)]
pub struct SwitchOptions {
    pub at_revset: Option<String>,
    pub bookmark: Option<String>,
    pub preserve_subdir: bool,
}

#[derive(Debug, Clone, Default)]
pub struct AddOptions {
    pub at_revset: Option<String>,
    pub bookmark: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddResult {
    pub workspace: String,
    pub path: PathBuf,
    pub bookmark: Option<String>,
}

#[derive(Debug, Clone)]
pub struct WorkspaceInventory {
    entries: Vec<WorkspaceEntry>,
    current: String,
    previous: Option<String>,
    previous_error: Option<String>,
    default: Option<String>,
    current_root: PathBuf,
}

impl WorkspaceInventory {
    pub fn load() -> Result<Self> {
        let current_root = canonicalize_dir(&workspace_root_current()?)?;
        let names = workspace_names()?;
        let mut entries = names
            .iter()
            .map(|name| {
                Ok(WorkspaceEntry {
                    name: name.clone(),
                    root: workspace_root_by_name_direct(name)?,
                    is_current: false,
                })
            })
            .collect::<Result<Vec<_>>>()?;

        let matches = entries
            .iter()
            .filter_map(|entry| match entry.root.as_deref().map(canonicalize_dir) {
                Some(Ok(root)) if root == current_root => Some(entry.name.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        let current = match matches.as_slice() {
            [] => {
                let target_candidates = current_workspace_names_by_target()?;
                let candidates = entries
                    .iter()
                    .filter(|entry| entry.root.is_none() && target_candidates.contains(&entry.name))
                    .map(|entry| entry.name.clone())
                    .collect::<Vec<_>>();
                match candidates.as_slice() {
                    [name] => {
                        entries
                            .iter_mut()
                            .find(|entry| entry.name == *name)
                            .expect("candidate came from entries")
                            .root = Some(current_root.clone());
                        name.clone()
                    }
                    _ => bail!(
                        "could not determine current workspace for root {}",
                        current_root.display()
                    ),
                }
            }
            [name] => name.clone(),
            _ => bail!(
                "multiple workspaces match current root {}: {}",
                current_root.display(),
                matches.join(", ")
            ),
        };

        for entry in &mut entries {
            entry.is_current = entry.name == current;
        }

        let default_root = workspace_base_root(&current_root, &current)?;
        if let Some(entry) = entries
            .iter_mut()
            .find(|entry| entry.name == "default" && entry.root.is_none())
            && default_root.is_dir()
        {
            entry.root = Some(default_root.clone());
        }
        let default = if names.iter().any(|name| name == "default") {
            Some("default".to_owned())
        } else if canonicalize_dir(&default_root).ok().as_ref() == Some(&current_root) {
            Some(current.clone())
        } else {
            None
        };

        let (previous, previous_error) = read_previous_workspace(&current_root, &names)?;

        Ok(Self {
            entries,
            current,
            previous,
            previous_error,
            default,
            current_root,
        })
    }

    pub fn entries(&self) -> &[WorkspaceEntry] {
        &self.entries
    }

    pub fn current_name(&self) -> &str {
        &self.current
    }

    pub fn current_root(&self) -> &Path {
        &self.current_root
    }

    pub fn previous_name(&self) -> Result<&str> {
        self.previous.as_deref().ok_or_else(|| {
            anyhow!(
                "{}",
                self.previous_error
                    .as_deref()
                    .unwrap_or("no previous workspace recorded")
            )
        })
    }

    pub fn default_name(&self) -> Result<&str> {
        self.default
            .as_deref()
            .ok_or_else(|| anyhow!("could not determine default workspace"))
    }

    pub fn contains(&self, name: &str) -> bool {
        self.entries.iter().any(|entry| entry.name == name)
    }

    pub fn resolve(&self, token: &str) -> Result<String> {
        match token {
            "@" => Ok(self.current.clone()),
            "-" => Ok(self.previous_name()?.to_owned()),
            "^" | "default" => Ok(self.default_name()?.to_owned()),
            other => Ok(other.to_owned()),
        }
    }

    pub fn root(&self, name: &str) -> Result<PathBuf> {
        self.entries
            .iter()
            .find(|entry| entry.name == name)
            .and_then(|entry| entry.root.clone())
            .ok_or_else(|| anyhow!("workspace not found: {name}"))
    }

    pub fn marker(&self, name: &str) -> char {
        if self.current == name {
            '@'
        } else if self.previous.as_deref() == Some(name) {
            '-'
        } else if self.default.as_deref() == Some(name) {
            '^'
        } else {
            ' '
        }
    }

    pub fn record_created(&mut self, result: &AddResult) {
        self.entries.push(WorkspaceEntry {
            name: result.workspace.clone(),
            root: Some(result.path.clone()),
            is_current: false,
        });
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemovalPlan {
    pub workspace: String,
    pub path: PathBuf,
    pub delete_dir: bool,
    pub bookmarks: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemovalResult {
    pub workspace: String,
    pub path: PathBuf,
    pub deleted_dir: bool,
    pub deleted_bookmarks: Vec<String>,
}

pub fn current_workspace_name() -> Result<String> {
    Ok(WorkspaceInventory::load()?.current)
}

fn workspace_root_by_name_direct(name: &str) -> Result<Option<PathBuf>> {
    let output = Command::new("jj")
        .args(["workspace", "root", "--name", name])
        .output()
        .with_context(|| "failed to execute `jj workspace root`".to_string())?;

    if output.status.success() {
        return Ok(Some(PathBuf::from(trimmed_stdout(output)?)));
    }
    let message = stderr_message(output, "workspace root lookup failed");
    let missing_checkout = message.contains("Cannot resolve absolute workspace path")
        && (message.contains("No such file or directory")
            || message.contains("os error 2")
            || message.contains("os error 3"))
        || message.contains("Workspace has no recorded path");
    if missing_checkout {
        Ok(None)
    } else {
        bail!("failed to resolve workspace root for {name}: {message}")
    }
}

pub fn workspace_root_current() -> Result<PathBuf> {
    let output = run_jj(&["workspace", "root"])?;
    Ok(PathBuf::from(trimmed_stdout(output)?))
}

pub fn switch_workspace(
    inventory: &WorkspaceInventory,
    target: &str,
    options: &SwitchOptions,
) -> Result<SwitchResult> {
    let current_name = inventory.current_name().to_owned();
    let current_root = inventory.current_root().to_path_buf();
    let current_dir = env::current_dir().context("failed to determine current directory")?;
    let relative_subdir = if options.preserve_subdir {
        current_dir
            .strip_prefix(&current_root)
            .ok()
            .map(Path::to_path_buf)
    } else {
        None
    };

    let resolved_name = inventory.resolve(target)?;
    let (target_path, created, bookmark) = if inventory.contains(&resolved_name) {
        (inventory.root(&resolved_name)?, false, None)
    } else {
        let result = add_workspace_by_name_with_inventory(
            &resolved_name,
            &AddOptions {
                at_revset: options.at_revset.clone(),
                bookmark: options.bookmark.clone(),
            },
            inventory,
        )?;
        (result.path, true, result.bookmark)
    };

    Ok(SwitchResult {
        workspace: resolved_name,
        path: target_path,
        created,
        bookmark,
        relative_subdir,
        from_workspace: current_name,
        from_path: current_root,
    })
}

pub fn record_switch(result: &SwitchResult) -> Result<()> {
    remember_previous_workspace(
        &result.from_workspace,
        &result.from_path,
        &result.workspace,
        &result.path,
    )
}

pub fn add_workspace(
    inventory: &WorkspaceInventory,
    target: &str,
    options: &AddOptions,
) -> Result<AddResult> {
    let name = inventory.resolve(target)?;
    if inventory.contains(&name) {
        bail!("workspace already exists: {name}")
    }
    add_workspace_by_name_with_inventory(&name, options, inventory)
}

pub fn path_for_workspace(token: &str) -> Result<PathBuf> {
    let inventory = WorkspaceInventory::load()?;
    let name = inventory.resolve(token)?;
    inventory.root(&name)
}

pub fn plan_remove_workspace(
    inventory: &WorkspaceInventory,
    token: Option<&str>,
    delete_dir: bool,
) -> Result<RemovalPlan> {
    let name = match token {
        Some(value) => inventory.resolve(value)?,
        None => inventory.current_name().to_owned(),
    };

    if name == inventory.default_name()? {
        bail!("refusing to remove the default workspace")
    }

    let path = inventory.root(&name)?;
    if delete_dir && name == inventory.current_name() {
        bail!("cannot delete the current workspace directory; switch away first")
    }

    Ok(RemovalPlan {
        bookmarks: bookmarks_for_workspace(&name, &path)?,
        workspace: name,
        path,
        delete_dir,
    })
}

pub fn execute_remove_workspace(
    plan: RemovalPlan,
    delete_bookmarks: bool,
) -> Result<RemovalResult> {
    run_jj(&["workspace", "forget", &plan.workspace])?;

    let mut deleted_bookmarks = Vec::new();
    if delete_bookmarks && !plan.bookmarks.is_empty() {
        let mut args = vec!["bookmark".to_owned(), "delete".to_owned()];
        args.extend(plan.bookmarks.iter().cloned());
        run_jj_owned(&args)?;
        deleted_bookmarks.clone_from(&plan.bookmarks);
    }

    let deleted_dir = plan.delete_dir && plan.path.is_dir();
    if deleted_dir {
        fs::remove_dir_all(&plan.path).with_context(|| {
            format!(
                "failed to delete workspace directory {}",
                plan.path.display()
            )
        })?;
    }

    Ok(RemovalResult {
        workspace: plan.workspace,
        path: plan.path,
        deleted_dir,
        deleted_bookmarks,
    })
}

fn add_workspace_by_name_with_inventory(
    name: &str,
    options: &AddOptions,
    inventory: &WorkspaceInventory,
) -> Result<AddResult> {
    validate_workspace_name(name)?;
    let path = workspace_dir_for_name(name, inventory)?;
    if path.exists() {
        bail!("directory already exists: {}", path.display());
    }

    let mut args = vec![
        "workspace".to_owned(),
        "add".to_owned(),
        "--name".to_owned(),
        name.to_owned(),
    ];

    if let Some(revset) = &options.at_revset {
        args.push("--revision".to_owned());
        args.push(revset.clone());
    }

    args.push(path.display().to_string());
    run_jj_owned(&args)?;

    if let Some(bookmark) = &options.bookmark {
        let output = Command::new("jj")
            .current_dir(&path)
            .args(["bookmark", "create", bookmark, "-r", "@"])
            .output()
            .with_context(|| "failed to create bookmark".to_string())?;
        if !output.status.success() {
            let error = stderr_message(output, "failed to create bookmark");
            let cleanup = rollback_workspace_parts(name, &path, None).err();
            if let Some(cleanup) = cleanup {
                bail!("{error}; cleanup also failed: {cleanup}")
            }
            bail!(error)
        }
        if let Err(error) = fs::write(workspace_bookmark_file(&path), format!("{bookmark}\n")) {
            let cleanup = rollback_workspace_parts(name, &path, Some(bookmark)).err();
            if let Some(cleanup) = cleanup {
                bail!(
                    "failed to record workspace bookmark: {error}; cleanup also failed: {cleanup}"
                )
            }
            bail!("failed to record workspace bookmark: {error}")
        }
    }

    Ok(AddResult {
        workspace: name.to_owned(),
        path,
        bookmark: options.bookmark.clone(),
    })
}

pub fn rollback_added_workspace(result: &AddResult) -> Result<()> {
    rollback_workspace_parts(&result.workspace, &result.path, result.bookmark.as_deref())
}

fn rollback_workspace_parts(name: &str, path: &Path, bookmark: Option<&str>) -> Result<()> {
    let mut errors = Vec::new();
    if let Err(error) = run_jj(&["workspace", "forget", name]) {
        errors.push(format!("forget workspace: {error}"));
    }
    if let Some(bookmark) = bookmark
        && let Err(error) = run_jj(&["bookmark", "delete", bookmark])
    {
        errors.push(format!("delete bookmark {bookmark}: {error}"));
    }
    if path.is_dir()
        && let Err(error) = fs::remove_dir_all(path)
    {
        errors.push(format!("delete directory {}: {error}", path.display()));
    }
    if errors.is_empty() {
        Ok(())
    } else {
        bail!(errors.join("; "))
    }
}

pub fn prune_missing_workspaces() -> Result<Vec<String>> {
    let mut removed = Vec::new();

    for entry in WorkspaceInventory::load()?.entries {
        match &entry.root {
            Some(path) if path.is_dir() => {}
            _ => {
                run_jj(&["workspace", "forget", &entry.name])?;
                removed.push(entry.name);
            }
        }
    }

    Ok(removed)
}

pub fn completion_workspace_candidates() -> Result<Vec<(String, String)>> {
    let inventory = WorkspaceInventory::load()?;

    let mut candidates = Vec::new();

    for entry in inventory.entries() {
        let description = if inventory.current_name() == entry.name {
            "Existing workspace (current)"
        } else if inventory.previous.as_deref() == Some(entry.name.as_str()) {
            "Existing workspace (previous)"
        } else if inventory.default.as_deref() == Some(entry.name.as_str()) {
            "Existing workspace (default)"
        } else {
            "Existing workspace"
        };
        candidates.push((entry.name.clone(), description.to_owned()));
    }

    candidates.push(("@".to_owned(), "Current workspace".to_owned()));
    candidates.push(("-".to_owned(), "Previous workspace".to_owned()));
    candidates.push(("^".to_owned(), "Default workspace".to_owned()));

    Ok(candidates)
}

fn remember_previous_workspace(
    from_name: &str,
    from_root: &Path,
    to_name: &str,
    to_root: &Path,
) -> Result<()> {
    if from_name == to_name {
        return Ok(());
    }

    let mut updates = vec![(
        workspace_state_file(from_root),
        format!("{to_name}\n").into_bytes(),
    )];
    let to_state_dir = to_root.join(".jj");
    if to_state_dir.is_dir() {
        updates.push((
            workspace_state_file(to_root),
            format!("{from_name}\n").into_bytes(),
        ));
    }

    let originals = updates
        .iter()
        .map(|(path, _)| read_optional_file(path))
        .collect::<Result<Vec<_>>>()?;
    for (index, (path, contents)) in updates.iter().enumerate() {
        if let Err(error) = fs::write(path, contents) {
            let rollback_errors = updates[..=index]
                .iter()
                .zip(&originals[..=index])
                .rev()
                .filter_map(|((path, _), original)| restore_file(path, original.as_deref()).err())
                .map(|error| error.to_string())
                .collect::<Vec<_>>();
            if rollback_errors.is_empty() {
                return Err(error).with_context(|| {
                    format!("failed to record workspace state at {}", path.display())
                });
            }
            bail!(
                "failed to record workspace state at {}: {error}; state rollback also failed: {}",
                path.display(),
                rollback_errors.join("; ")
            )
        }
    }

    Ok(())
}

fn read_optional_file(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(contents) => Ok(Some(contents)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error)
            .with_context(|| format!("failed to inspect workspace state {}", path.display())),
    }
}

fn restore_file(path: &Path, contents: Option<&[u8]>) -> Result<()> {
    match contents {
        Some(contents) => fs::write(path, contents)
            .with_context(|| format!("failed to restore workspace state {}", path.display())),
        None => match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error)
                .with_context(|| format!("failed to remove workspace state {}", path.display())),
        },
    }
}

fn workspace_state_file(root: &Path) -> PathBuf {
    root.join(".jj").join(PREVIOUS_WORKSPACE_FILE)
}

fn workspace_bookmark_file(root: &Path) -> PathBuf {
    root.join(".jj").join(WORKSPACE_BOOKMARK_FILE)
}

fn workspace_dir_for_name(name: &str, inventory: &WorkspaceInventory) -> Result<PathBuf> {
    let default_root = workspace_base_root(inventory.current_root(), inventory.current_name())?;
    if name == "default" {
        Ok(default_root)
    } else {
        let parent = default_root
            .parent()
            .ok_or_else(|| anyhow!("workspace root has no parent directory"))?;
        let base_name = default_root
            .file_name()
            .ok_or_else(|| anyhow!("workspace root has no valid basename"))?;
        Ok(parent.join(format!("{}.{name}", base_name.to_string_lossy())))
    }
}

fn workspace_base_root(current_root: &Path, current_name: &str) -> Result<PathBuf> {
    let parent = current_root
        .parent()
        .ok_or_else(|| anyhow!("workspace root has no parent directory"))?;
    let mut base = current_root
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow!("workspace root has no valid basename"))?
        .to_owned();

    let suffix = format!(".{current_name}");
    if current_name != "default" && base.ends_with(&suffix) {
        let new_len = base.len() - suffix.len();
        base.truncate(new_len);
    } else if current_name != "default" && base == current_name && base.contains('.') {
        if let Some((prefix, _)) = base.rsplit_once('.') {
            base = prefix.to_owned();
        }
    }

    Ok(parent.join(base))
}

fn read_previous_workspace(
    root: &Path,
    names: &[String],
) -> Result<(Option<String>, Option<String>)> {
    let state_path = workspace_state_file(root);
    let contents = match fs::read_to_string(&state_path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((None, Some("no previous workspace recorded".to_owned())));
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!("failed to read workspace state {}", state_path.display())
            });
        }
    };
    let recorded = contents
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    if recorded.is_empty() {
        return Ok((None, Some("no previous workspace recorded".to_owned())));
    }
    if recorded.len() != 1 {
        return Ok((
            None,
            Some(format!(
                "previous workspace record is invalid: {}",
                state_path.display()
            )),
        ));
    }
    if names.iter().any(|name| name == recorded[0]) {
        Ok((Some(recorded[0].to_owned()), None))
    } else {
        Ok((None, Some("no previous workspace recorded".to_owned())))
    }
}

fn validate_workspace_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("workspace name cannot be empty")
    }
    if name.contains('\n') {
        bail!("workspace name cannot contain newlines")
    }
    if name.contains("..") {
        bail!("workspace name cannot contain '..'")
    }
    if name.starts_with('/') {
        bail!("workspace name cannot start with '/'")
    }
    if name.starts_with('-') {
        bail!("workspace name cannot start with '-'")
    }
    if name.contains(':') {
        bail!("workspace name cannot contain ':'")
    }
    Ok(())
}

fn workspace_names() -> Result<Vec<String>> {
    let output = run_jj(&[
        "workspace",
        "list",
        "-T",
        "name ++ \"\\n\"",
        "--color=never",
    ])?;

    Ok(trimmed_stdout(output)?
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .collect())
}

fn current_workspace_names_by_target() -> Result<Vec<String>> {
    let output = run_jj(&[
        "workspace",
        "list",
        "-T",
        "if(target.current_working_copy(), name ++ \"\\n\", \"\")",
        "--color=never",
    ])?;
    let stdout = trimmed_stdout(output)?;
    let names = stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();

    Ok(names.into_iter().map(ToOwned::to_owned).collect())
}

fn bookmarks_for_workspace(name: &str, root: &Path) -> Result<Vec<String>> {
    let bookmark_path = workspace_bookmark_file(root);
    let recorded = match fs::read_to_string(&bookmark_path) {
        Ok(contents) => {
            let names = contents
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .collect::<Vec<_>>();
            if names.len() != 1 {
                bail!(
                    "workspace bookmark record is invalid: {}",
                    bookmark_path.display()
                )
            }
            Some(names[0].to_owned())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "failed to read workspace bookmark {}",
                    bookmark_path.display()
                )
            });
        }
    };
    let mut args = vec!["bookmark".to_owned(), "list".to_owned()];
    if recorded.is_none() {
        args.push("-r".to_owned());
        args.push(format!("{name}@"));
    }
    args.extend([
        "-T".to_owned(),
        "name ++ \"\\n\"".to_owned(),
        "--color=never".to_owned(),
    ]);
    let output = run_jj_owned(&args)?;
    let candidates = trimmed_stdout(output)?
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    if let Some(recorded) = recorded {
        return Ok(candidates
            .into_iter()
            .filter(|candidate| candidate == &recorded)
            .collect());
    }
    let suffix = format!("/{name}");
    Ok(candidates
        .into_iter()
        .filter(|candidate| candidate == name || candidate.ends_with(&suffix))
        .collect())
}

fn canonicalize_dir(path: &Path) -> Result<PathBuf> {
    path.canonicalize()
        .with_context(|| format!("failed to resolve {}", path.display()))
}

fn run_jj(args: &[&str]) -> Result<std::process::Output> {
    let output = Command::new("jj")
        .args(args)
        .output()
        .with_context(|| format!("failed to execute `jj {}`", args.join(" ")))?;

    if output.status.success() {
        Ok(output)
    } else {
        bail!(stderr_message(output, "jj command failed"))
    }
}

fn run_jj_owned(args: &[String]) -> Result<std::process::Output> {
    let output = Command::new("jj")
        .args(args)
        .output()
        .with_context(|| format!("failed to execute `jj {}`", args.join(" ")))?;

    if output.status.success() {
        Ok(output)
    } else {
        bail!(stderr_message(output, "jj command failed"))
    }
}

fn trimmed_stdout(output: std::process::Output) -> Result<String> {
    String::from_utf8(output.stdout)
        .context("jj output was not valid UTF-8")
        .map(|value| value.trim().to_owned())
}

fn stderr_message(output: std::process::Output, fallback: &str) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    if stderr.is_empty() {
        fallback.to_owned()
    } else {
        stderr
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_workspace_names() {
        assert!(validate_workspace_name("feature").is_ok());
        assert!(validate_workspace_name("").is_err());
        assert!(validate_workspace_name("../bad").is_err());
        assert!(validate_workspace_name("-bad").is_err());
        assert!(validate_workspace_name("bad:name").is_err());
    }
}
