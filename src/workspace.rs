use crate::jj::JjClient;
use crate::metadata::{ManagedWorkspaceMetadata, WorkspaceMetadataStore};
use anyhow::{Context, Result, anyhow, bail};
use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

const PREVIOUS_WORKSPACE_FILE: &str = "jw-prev-workspace";
const WORKSPACE_BOOKMARK_FILE: &str = "jw-bookmark";
const REMOVE_DIRECTORY_ATTEMPTS: usize = 4;
const REMOVE_DIRECTORY_RETRY_DELAY: Duration = Duration::from_millis(25);

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
    pub preserve_subdir: bool,
}

#[derive(Debug, Clone)]
pub struct AddOptions {
    /// Full commit ID resolved before any workspace mutation.
    pub base_commit_id: String,
    pub bookmark: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddResult {
    pub workspace: String,
    pub path: PathBuf,
    pub bookmark: Option<String>,
    pub creation_operation_id: String,
    pub creation_base_commit_id: String,
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

#[derive(Debug, Clone)]
pub struct RemovalPlan {
    pub workspace: String,
    pub path: PathBuf,
    pub delete_dir: bool,
    pub bookmarks: Vec<String>,
    managed_metadata: Option<ManagedWorkspaceMetadata>,
    managed_store: Option<WorkspaceMetadataStore>,
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
    let output = JjClient::current()?.run_unchecked(["workspace", "root", "--name", name])?;

    if output.success() {
        return Ok(Some(PathBuf::from(output.trimmed_stdout()?)));
    }
    let message = output.stderr();
    let message = if message.is_empty() {
        "workspace root lookup failed".to_owned()
    } else {
        message
    };
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
    let output = JjClient::current()?.run(["workspace", "root"])?;
    Ok(PathBuf::from(output.trimmed_stdout()?))
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
    if !inventory.contains(&resolved_name) {
        bail!("workspace does not exist: {resolved_name}")
    }
    let target_path = inventory.root(&resolved_name)?;

    Ok(SwitchResult {
        workspace: resolved_name,
        path: target_path,
        created: false,
        bookmark: None,
        relative_subdir,
        from_workspace: current_name,
        from_path: current_root,
    })
}

pub fn switch_to_created_workspace(
    inventory: &WorkspaceInventory,
    created: &AddResult,
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

    Ok(SwitchResult {
        workspace: created.workspace.clone(),
        path: created.path.clone(),
        created: true,
        bookmark: created.bookmark.clone(),
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

    let store = metadata_store()?;
    let managed_metadata = store.get(&name)?;
    let bookmarks = match &managed_metadata {
        Some(metadata) => bookmarks_for_managed_workspace(metadata)?,
        None => bookmarks_for_workspace(&name, &path)?,
    };

    Ok(RemovalPlan {
        bookmarks,
        workspace: name,
        path,
        delete_dir,
        managed_store: managed_metadata.as_ref().map(|_| store),
        managed_metadata,
    })
}

pub fn execute_remove_workspace(
    plan: RemovalPlan,
    delete_bookmarks: bool,
) -> Result<RemovalResult> {
    JjClient::current()?.run(["workspace", "forget", &plan.workspace])?;

    let mut deleted_bookmarks = Vec::new();
    if delete_bookmarks && !plan.bookmarks.is_empty() {
        let mut args = vec!["bookmark".to_owned(), "delete".to_owned()];
        args.extend(plan.bookmarks.iter().cloned());
        JjClient::current()?.run(&args).with_context(|| {
            format!(
                "partial removal: workspace {} was forgotten, but associated bookmark deletion failed",
                plan.workspace
            )
        })?;
        deleted_bookmarks.clone_from(&plan.bookmarks);
    }

    if let Some(metadata) = &plan.managed_metadata {
        let store = plan
            .managed_store
            .as_ref()
            .expect("managed metadata plan retains its store");
        let removed = store.remove_if_matches(metadata).with_context(|| {
            format!(
                "{}; managed workspace metadata cleanup failed",
                partial_removal_progress(&plan.workspace, &deleted_bookmarks)
            )
        })?;
        if !removed {
            bail!(
                "{}; workspace metadata changed during removal and was retained: {}",
                partial_removal_progress(&plan.workspace, &deleted_bookmarks),
                metadata.workspace_name
            )
        }
    }

    let deleted_dir = plan.delete_dir && plan.path.is_dir();
    if deleted_dir {
        remove_workspace_directory(&plan.path).with_context(|| {
            format!(
                "{}; workspace directory remains at {}",
                partial_removal_progress(&plan.workspace, &deleted_bookmarks),
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

fn partial_removal_progress(workspace: &str, deleted_bookmarks: &[String]) -> String {
    let mut progress = format!("partial removal: workspace {workspace} was forgotten");
    match deleted_bookmarks {
        [] => {}
        [bookmark] => progress.push_str(&format!(" and bookmark {bookmark} was deleted")),
        bookmarks => progress.push_str(&format!(
            " and bookmarks {} were deleted",
            bookmarks.join(", ")
        )),
    }
    progress
}

fn remove_workspace_directory(path: &Path) -> io::Result<()> {
    remove_workspace_directory_with(path, |path| fs::remove_dir_all(path))
}

fn remove_workspace_directory_with(
    path: &Path,
    mut remove: impl FnMut(&Path) -> io::Result<()>,
) -> io::Result<()> {
    for attempt in 1..=REMOVE_DIRECTORY_ATTEMPTS {
        match remove(path) {
            Ok(()) => return Ok(()),
            Err(error)
                if error.kind() == io::ErrorKind::DirectoryNotEmpty
                    && attempt < REMOVE_DIRECTORY_ATTEMPTS =>
            {
                thread::sleep(REMOVE_DIRECTORY_RETRY_DELAY);
            }
            Err(error) => return Err(error),
        }
    }
    unreachable!("directory removal loop always returns")
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

    args.push("--revision".to_owned());
    args.push(options.base_commit_id.clone());

    args.push(path.display().to_string());
    let client = JjClient::current()?;
    client.run(&args)?;

    // Capture provenance before bookmark creation records another JJ operation.
    let creation_operation_id = match client.operation_id() {
        Ok(operation_id) => operation_id,
        Err(error) => {
            let cleanup = rollback_workspace_parts(name, &path, None).err();
            if let Some(cleanup) = cleanup {
                bail!("{error}; cleanup also failed: {cleanup}")
            }
            bail!(error)
        }
    };

    if let Some(bookmark) = &options.bookmark {
        if let Err(error) = JjClient::new(&path).run(["bookmark", "create", bookmark, "-r", "@"]) {
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
        creation_operation_id,
        creation_base_commit_id: options.base_commit_id.clone(),
    })
}

pub(crate) fn preflight_add_workspace(inventory: &WorkspaceInventory, name: &str) -> Result<()> {
    validate_workspace_name(name)?;
    if inventory.contains(name) {
        bail!("workspace already exists: {name}")
    }
    let path = workspace_dir_for_name(name, inventory)?;
    if path.exists() {
        bail!("directory already exists: {}", path.display())
    }
    Ok(())
}

pub fn rollback_added_workspace(result: &AddResult) -> Result<()> {
    rollback_workspace_parts(&result.workspace, &result.path, result.bookmark.as_deref())
}

fn rollback_workspace_parts(name: &str, path: &Path, bookmark: Option<&str>) -> Result<()> {
    let mut errors = Vec::new();
    if let Err(error) = JjClient::current()?.run(["workspace", "forget", name]) {
        errors.push(format!("forget workspace: {error}"));
    }
    if let Some(bookmark) = bookmark
        && let Err(error) = JjClient::current()?.run(["bookmark", "delete", bookmark])
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
    let store = metadata_store()?;

    for entry in WorkspaceInventory::load()?.entries {
        match &entry.root {
            Some(path) if path.is_dir() => {}
            _ => {
                let metadata = store.get(&entry.name)?;
                JjClient::current()?.run(["workspace", "forget", &entry.name])?;
                if let Some(metadata) = metadata
                    && !store.remove_if_matches(&metadata)?
                {
                    bail!(
                        "workspace metadata changed during prune and was retained: {}",
                        metadata.workspace_name
                    )
                }
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

pub(crate) fn legacy_workspace_bookmark(root: &Path) -> Result<Option<String>> {
    let path = workspace_bookmark_file(root);
    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to read workspace bookmark {}", path.display()));
        }
    };
    let names = contents
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    match names.as_slice() {
        [name] => Ok(Some((*name).to_owned())),
        _ => bail!("workspace bookmark record is invalid: {}", path.display()),
    }
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

pub(crate) fn workspace_base_root(current_root: &Path, current_name: &str) -> Result<PathBuf> {
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
    JjClient::current()?.workspace_names()
}

fn current_workspace_names_by_target() -> Result<Vec<String>> {
    JjClient::current()?.current_workspace_target_names()
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
    let client = JjClient::current()?;
    let operation_id = client.operation_id()?;
    let candidates = match &recorded {
        Some(_) => client.local_bookmark_names_at(&operation_id)?,
        None => client.local_bookmark_names_for_workspace_at(&operation_id, name)?,
    };
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

fn bookmarks_for_managed_workspace(metadata: &ManagedWorkspaceMetadata) -> Result<Vec<String>> {
    let Some(recorded) = &metadata.associated_bookmark else {
        return Ok(Vec::new());
    };
    let client = JjClient::current()?;
    let operation_id = client.operation_id()?;
    Ok(client
        .local_bookmark_names_at(&operation_id)?
        .into_iter()
        .filter(|candidate| candidate == recorded)
        .collect())
}

fn metadata_store() -> Result<WorkspaceMetadataStore> {
    let client = JjClient::current()?;
    WorkspaceMetadataStore::from_repo_config_path(client.repo_config_path()?)
}

fn canonicalize_dir(path: &Path) -> Result<PathBuf> {
    path.canonicalize()
        .with_context(|| format!("failed to resolve {}", path.display()))
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

    #[test]
    fn retries_directory_not_empty_during_workspace_cleanup() {
        let tempdir = tempfile::tempdir().expect("create temporary directory");
        let workspace = tempdir.path().join("workspace");
        fs::create_dir(&workspace).expect("create workspace directory");
        let mut attempts = 0;

        remove_workspace_directory_with(&workspace, |path| {
            attempts += 1;
            if attempts == 1 {
                Err(io::Error::from(io::ErrorKind::DirectoryNotEmpty))
            } else {
                fs::remove_dir_all(path)
            }
        })
        .expect("retry directory removal");

        assert_eq!(attempts, 2);
        assert!(!workspace.exists());
    }
}
