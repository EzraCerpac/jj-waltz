//! Reviewed, drift-resistant workspace removal for the interactive manager.

use super::status::{self, Integration, ManagerRow};
use crate::jj::{JjClient, RecordedRoot};
use crate::metadata::{ManagedWorkspaceMetadata, WorkspaceMetadataStore};
use crate::ownership::OwnershipView;
use crate::snapshot::{ManagementState, WorkingCopyStatus, WorkspaceSnapshot};
use crate::workspace;
use anyhow::{Context, Result, anyhow, bail};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

#[derive(Debug, Clone)]
pub struct BatchPlan {
    pub trunk: String,
    pub rows: Vec<RemovalRow>,
    pub prune: bool,
    frozen: BTreeMap<String, FrozenRow>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemovalRow {
    pub name: String,
    pub path: Option<PathBuf>,
    pub bookmark: Option<String>,
    pub warnings: Vec<String>,
    pub ignored: Vec<String>,
    pub blocked: Option<String>,
    pub risky: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchOutcome {
    pub name: String,
    pub state: OutcomeState,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutcomeState {
    Completed,
    Failed,
    Partial,
    Skipped,
}

impl BatchOutcome {
    pub fn success(&self) -> bool {
        self.state == OutcomeState::Completed
    }
}

#[derive(Debug, Clone)]
struct FrozenRow {
    status: ManagerRow,
    metadata: Option<ManagedWorkspaceMetadata>,
    bookmark_targets: Vec<String>,
    path_identity: Option<PathIdentity>,
    missing_path: Option<PathBuf>,
    ignored_inventory: Vec<IgnoredEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PathIdentity {
    canonical: PathBuf,
    modified_ns: Option<u128>,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct IgnoredEntry {
    path: PathBuf,
    kind: IgnoredKind,
    length: u64,
    modified_ns: Option<u128>,
    symlink_target: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IgnoredKind {
    Directory,
    File,
    Symlink,
    Other,
}

struct IgnoredScan {
    display: Vec<String>,
    inventory: Vec<IgnoredEntry>,
}

/// Refresh and review the requested workspaces without mutating their lifecycle state.
pub fn prepare(trunk: &str, names: &[String], prune: bool) -> Result<BatchPlan> {
    reject_duplicate_names(names)?;
    let captured = status::capture(trunk, names)?;
    let operation_id = captured.snapshot.repository.operation_id.clone();
    let client = JjClient::current()?;
    let store = metadata_store(&client)?;
    // Repository-wide facts are read once for the whole batch, however many rows it reviews.
    let all_bookmark_targets = client.local_bookmark_targets_at(&operation_id)?;
    let ownership = OwnershipView::live(&client).with_all_roots();
    let selected = names.iter().cloned().collect::<BTreeSet<_>>();
    let mut seen = BTreeSet::new();
    let mut rows = Vec::new();
    let mut frozen = BTreeMap::new();

    for status_row in captured.rows {
        let workspace = &status_row.workspace;
        if !selected.is_empty() && !selected.contains(&workspace.name) {
            continue;
        }
        if selected.is_empty() && !prune {
            continue;
        }
        if prune && selected.is_empty() && workspace.path.is_some() {
            continue;
        }
        seen.insert(workspace.name.clone());

        let metadata = store
            .get(&workspace.name)
            .with_context(|| format!("failed to read metadata for workspace {}", workspace.name))?;
        let bookmark_targets = bookmark_targets(&all_bookmark_targets, metadata.as_ref());
        let mut warnings = status_row.warnings.clone();
        let mut blocked = structural_block(&status_row, prune);
        let allowed = match &ownership {
            Ok(ownership) => ownership
                .ensure_removal_allowed(
                    &workspace.name,
                    workspace.path.as_deref(),
                    metadata.as_ref(),
                )
                .map(|_| ()),
            Err(error) => Err(anyhow!("cannot verify workspace ownership: {error:#}")),
        };
        if let Err(error) = allowed {
            set_blocked(&mut blocked, error.to_string());
        }
        let mut ignored = Vec::new();
        let mut ignored_inventory = Vec::new();
        let mut path_identity = None;
        let mut missing_path = None;

        if let Some(path) = workspace.path.as_deref() {
            match inspect_workspace_path(&client, path, &workspace.name) {
                Ok(identity) => path_identity = Some(identity),
                Err(error) => set_blocked(&mut blocked, error.to_string()),
            }
            match scan_ignored(path) {
                Ok(scan) => {
                    ignored = scan.display;
                    ignored_inventory = scan.inventory;
                }
                Err(error) => set_blocked(&mut blocked, error.to_string()),
            }
        } else if prune {
            let inspected = ownership
                .as_ref()
                .map_err(|error| anyhow!("{error:#}"))
                .and_then(|ownership| inspect_missing_workspace(ownership, &workspace.name));
            match inspected {
                Ok(path) => missing_path = Some(path),
                Err(error) => set_blocked(&mut blocked, error.to_string()),
            }
        }

        if metadata_identity(&metadata) != workspace_metadata_identity(workspace) {
            set_blocked(
                &mut blocked,
                "workspace metadata changed while the removal preview was being built",
            );
        }

        let risky = is_risky(&status_row);
        append_review_warnings(&status_row, &mut warnings);
        if !ignored.is_empty() {
            warnings.push(format!(
                "{} path(s) not recorded by JJ are present",
                ignored.len()
            ));
        }

        let public = RemovalRow {
            name: workspace.name.clone(),
            path: workspace.path.clone().or_else(|| missing_path.clone()),
            bookmark: (!bookmark_targets.is_empty())
                .then(|| managed_bookmark(workspace, metadata.as_ref()))
                .flatten(),
            warnings,
            ignored,
            blocked,
            risky,
        };
        frozen.insert(
            public.name.clone(),
            FrozenRow {
                status: status_row,
                metadata,
                bookmark_targets,
                path_identity,
                missing_path,
                ignored_inventory,
            },
        );
        rows.push(public);
    }

    if !selected.is_empty() {
        let missing = selected.difference(&seen).cloned().collect::<Vec<_>>();
        if !missing.is_empty() {
            bail!(
                "requested workspace(s) were not returned by the refreshed snapshot: {}",
                missing.join(", ")
            )
        }
    }

    Ok(BatchPlan {
        trunk: trunk.to_owned(),
        rows,
        prune,
        frozen,
    })
}

/// Execute reviewed removals. One capture revalidates every runnable row against its preview;
/// each row's live ownership, path identity, and content checks still run immediately before that
/// row's mutation. Missing workspaces in prune mode are forgotten together with one JJ operation.
pub fn execute(
    plan: BatchPlan,
    delete_bookmarks: bool,
    include_risky: bool,
    acknowledge_ignored: bool,
) -> Vec<BatchOutcome> {
    let BatchPlan {
        trunk,
        rows,
        prune,
        mut frozen,
    } = plan;

    let mut outcomes = Vec::with_capacity(rows.len());
    let mut runnable = Vec::new();
    for row in rows {
        let Some(expected) = frozen.remove(&row.name) else {
            outcomes.push(Some(failed(
                &row.name,
                "internal removal plan is incomplete",
            )));
            continue;
        };
        let skip = if let Some(reason) = &row.blocked {
            Some(format!("blocked: {reason}"))
        } else if row.risky && !include_risky {
            Some("excluded because it needs the risky-removal override".to_owned())
        } else if !row.ignored.is_empty() && !acknowledge_ignored {
            Some(
                "skipped because files not recorded by JJ were not approved for deletion"
                    .to_owned(),
            )
        } else {
            None
        };
        match skip {
            Some(reason) => outcomes.push(Some(skipped(&row.name, reason))),
            None => {
                runnable.push((outcomes.len(), row.name, expected));
                outcomes.push(None);
            }
        }
    }
    if runnable.is_empty() {
        return outcomes.into_iter().flatten().collect();
    }

    let names = runnable
        .iter()
        .map(|(_, name, _)| name.clone())
        .collect::<Vec<_>>();
    let results = match Revalidation::capture(&trunk, &names) {
        Ok(revalidation) if prune => execute_prune_batch(&revalidation, runnable, delete_bookmarks),
        Ok(revalidation) => runnable
            .into_iter()
            .map(|(index, name, expected)| {
                let result = execute_removal(&revalidation, &name, expected, delete_bookmarks);
                (index, name, result)
            })
            .collect(),
        // A row that vanished since the preview fails the batch capture. Revalidate rows alone
        // so independent removals still proceed.
        Err(_) => runnable
            .into_iter()
            .map(|(index, name, expected)| {
                let result = Revalidation::capture(&trunk, std::slice::from_ref(&name))
                    .with_context(|| format!("failed to revalidate workspace {name}"))
                    .and_then(|revalidation| {
                        if prune {
                            execute_prune_batch(
                                &revalidation,
                                vec![(index, name.clone(), expected)],
                                delete_bookmarks,
                            )
                            .pop()
                            .expect("one prune result per row")
                            .2
                        } else {
                            execute_removal(&revalidation, &name, expected, delete_bookmarks)
                        }
                    });
                (index, name, result)
            })
            .collect(),
    };
    for (index, name, result) in results {
        outcomes[index] = Some(match result {
            Ok(message) => BatchOutcome {
                name,
                state: OutcomeState::Completed,
                message,
            },
            Err(error) => failed(&name, format!("{error:#}")),
        });
    }
    outcomes.into_iter().flatten().collect()
}

/// One fresh capture of the rows about to be removed, plus the repository facts their
/// revalidation compares against the preview.
struct Revalidation {
    rows: BTreeMap<String, ManagerRow>,
    bookmark_targets: BTreeMap<String, Vec<String>>,
    client: JjClient,
    store: WorkspaceMetadataStore,
}

impl Revalidation {
    fn capture(trunk: &str, names: &[String]) -> Result<Self> {
        let captured = status::capture(trunk, names)?;
        let client = JjClient::current()?;
        let bookmark_targets =
            client.local_bookmark_targets_at(&captured.snapshot.repository.operation_id)?;
        let store = metadata_store(&client)?;
        Ok(Self {
            rows: captured
                .rows
                .into_iter()
                .map(|row| (row.workspace.name.clone(), row))
                .collect(),
            bookmark_targets,
            client,
            store,
        })
    }

    /// Checks that hold for both removal and pruning. Ownership is judged on the current
    /// metadata before it is compared, so a newly recorded owner is reported as such.
    fn revalidate(
        &self,
        ownership: &OwnershipView<'_>,
        name: &str,
        expected: &FrozenRow,
    ) -> Result<&ManagerRow> {
        let current = self
            .rows
            .get(name)
            .ok_or_else(|| anyhow!("workspace disappeared during revalidation"))?;
        ensure_status_unchanged(&expected.status, current)?;
        let current_metadata = self.store.get(name)?;
        ownership.ensure_removal_allowed(
            name,
            current.workspace.path.as_deref(),
            current_metadata.as_ref(),
        )?;
        if current_metadata != expected.metadata {
            bail!("workspace metadata changed since the removal preview; review it again")
        }
        if bookmark_targets(&self.bookmark_targets, expected.metadata.as_ref())
            != expected.bookmark_targets
        {
            bail!("associated bookmark moved since the removal preview; review it again")
        }
        Ok(current)
    }
}

fn execute_removal(
    revalidation: &Revalidation,
    name: &str,
    expected: FrozenRow,
    delete_bookmarks: bool,
) -> Result<String> {
    let client = &revalidation.client;
    let store = &revalidation.store;
    let current = revalidation.revalidate(&OwnershipView::live(client), name, &expected)?;
    let Some(path) = current.workspace.path.as_deref() else {
        if expected.path_identity.is_some() {
            bail!("workspace path disappeared since the removal preview; review it again")
        }
        bail!("workspace path is missing; use prune instead")
    };
    let identity = inspect_workspace_path(client, path, name)?;
    if Some(identity) != expected.path_identity {
        bail!("workspace path identity changed since the removal preview; review it again")
    }
    let scan = scan_ignored(path)?;
    if scan.inventory != expected.ignored_inventory {
        bail!("workspace content changed since the removal preview; review it again")
    }

    let inventory = workspace::WorkspaceInventory::load()?;
    let mut low_level = workspace::plan_remove_workspace(&inventory, Some(name), true)?;
    // The general-purpose lifecycle command can infer bookmark ownership for unmanaged
    // workspaces. The manager has no authoritative association for those rows, so its
    // explicit removal approval covers only the workspace and directory.
    if expected.metadata.is_none() {
        low_level.bookmarks.clear();
    }
    if store.get(name)? != expected.metadata {
        bail!("workspace metadata changed during final removal planning; review it again")
    }
    let expected_bookmarks = expected
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.associated_bookmark.as_ref())
        .filter(|_| !expected.bookmark_targets.is_empty())
        .cloned()
        .into_iter()
        .collect::<Vec<_>>();
    if low_level.bookmarks != expected_bookmarks {
        bail!("associated bookmark changed during final removal planning; review it again")
    }
    let planned_path = canonicalize(&low_level.path)?;
    let expected_path = expected
        .path_identity
        .as_ref()
        .ok_or_else(|| anyhow!("workspace path is missing; use prune instead"))?;
    if planned_path != expected_path.canonical {
        bail!("workspace path changed during final removal planning; review it again")
    }
    let registered = client.workspace_names()?.into_iter().collect();
    let shared = shared_bookmark(store, &registered, name, expected.metadata.as_ref())?;
    let result = workspace::execute_remove_workspace(
        low_level,
        delete_bookmarks && !shared && expected.metadata.is_some(),
    )?;
    let action = if result.deleted_dir {
        "Removed workspace and directory"
    } else {
        "Forgot workspace"
    };
    Ok(success_message(
        action,
        name,
        expected.metadata.as_ref(),
        delete_bookmarks,
        shared,
    ))
}

type RowResult = (usize, String, Result<String>);

/// Revalidate every missing row, forget the valid ones with one JJ operation, then delete their
/// unshared bookmarks with one more and retire their metadata.
fn execute_prune_batch(
    revalidation: &Revalidation,
    rows: Vec<(usize, String, FrozenRow)>,
    delete_bookmarks: bool,
) -> Vec<RowResult> {
    let client = &revalidation.client;
    let store = &revalidation.store;
    let ownership = OwnershipView::live(client).with_all_roots();
    let mut results = Vec::new();
    let mut valid = Vec::new();
    for (index, name, expected) in rows {
        let checked = ownership
            .as_ref()
            .map_err(|error| anyhow!("cannot verify workspace ownership: {error:#}"))
            .and_then(|ownership| {
                let current = revalidation.revalidate(ownership, &name, &expected)?;
                if current.workspace.path.is_some() {
                    bail!("workspace path reappeared since the removal preview; review it again")
                }
                if Some(inspect_missing_workspace(ownership, &name)?) != expected.missing_path {
                    bail!(
                        "missing workspace path changed since the removal preview; review it again"
                    )
                }
                Ok(())
            });
        match checked {
            Ok(()) => valid.push((index, name, expected)),
            Err(error) => results.push((index, name, Err(error))),
        }
    }
    if valid.is_empty() {
        return results;
    }

    let targets = valid
        .iter()
        .map(|(_, name, expected)| crate::ownership::MissingWorkspace {
            name,
            metadata: expected.metadata.as_ref(),
        })
        .collect::<Vec<_>>();
    if let Err(error) = crate::ownership::forget_missing(client, &targets) {
        let message = format!("{error:#}");
        results.extend(
            valid
                .into_iter()
                .map(|(index, name, _)| (index, name, Err(anyhow!("{message}")))),
        );
        return results;
    }

    // Shared means another workspace that survives this batch still records the bookmark.
    let survivors = client.workspace_names().map(BTreeSet::from_iter);
    let mut sharing = BTreeMap::new();
    for (_, name, expected) in &valid {
        let shared = survivors
            .as_ref()
            .map_err(|error| anyhow!("{error:#}"))
            .and_then(|survivors| {
                shared_bookmark(store, survivors, name, expected.metadata.as_ref())
            });
        sharing.insert(name.clone(), shared);
    }
    let doomed = valid
        .iter()
        .filter(|(_, name, _)| delete_bookmarks && matches!(sharing.get(name), Some(Ok(false))))
        .filter_map(|(_, _, expected)| managed_association(expected.metadata.as_ref()))
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    let deleted = delete_local_bookmarks(client, &doomed);

    for (index, name, expected) in valid {
        let bookmark = managed_association(expected.metadata.as_ref());
        let result = (|| -> Result<String> {
            let mut progress = format!("partial removal: workspace {name} was forgotten");
            let shared = sharing
                .remove(&name)
                .expect("sharing computed for every row")
                .with_context(|| progress.clone())?;
            if let Some(bookmark) = bookmark.filter(|bookmark| doomed.contains(*bookmark)) {
                match &deleted {
                    Ok(deleted) if deleted.contains(bookmark) => {
                        progress.push_str(&format!(" and bookmark {bookmark} was deleted"));
                    }
                    Ok(_) => {}
                    Err(error) => bail!("{progress}, but bookmark {bookmark} remains: {error}"),
                }
            }
            if let Some(metadata) = &expected.metadata {
                let removed = store.remove_if_matches(metadata).with_context(|| {
                    format!("{progress}; managed workspace metadata cleanup failed")
                })?;
                if !removed {
                    bail!(
                        "{progress}; workspace metadata changed during removal and was retained: {name}"
                    )
                }
            }
            Ok(success_message(
                "Pruned missing workspace",
                &name,
                expected.metadata.as_ref(),
                delete_bookmarks,
                shared,
            ))
        })();
        results.push((index, name, result));
    }
    results
}

fn managed_association(metadata: Option<&ManagedWorkspaceMetadata>) -> Option<&str> {
    metadata.and_then(|entry| entry.associated_bookmark.as_deref())
}

/// Delete whichever of the given bookmarks still exist locally, with one JJ operation.
/// Returns the bookmarks that were deleted.
fn delete_local_bookmarks(
    client: &JjClient,
    bookmarks: &BTreeSet<String>,
) -> std::result::Result<BTreeSet<String>, String> {
    if bookmarks.is_empty() {
        return Ok(BTreeSet::new());
    }
    let existing = client
        .operation_id()
        .and_then(|operation_id| client.local_bookmark_names_at(&operation_id))
        .map_err(|error| format!("{error:#}"))?;
    let present = bookmarks
        .iter()
        .filter(|bookmark| existing.contains(*bookmark))
        .cloned()
        .collect::<BTreeSet<_>>();
    if present.is_empty() {
        return Ok(present);
    }
    let mut args = vec!["bookmark".to_owned(), "delete".to_owned()];
    args.extend(present.iter().cloned());
    client
        .run(&args)
        .map(|_| present)
        .map_err(|error| format!("{error:#}"))
}

fn success_message(
    action: &str,
    name: &str,
    metadata: Option<&ManagedWorkspaceMetadata>,
    delete_bookmarks: bool,
    shared: bool,
) -> String {
    let mut message = format!("{action}: {name}");
    if let Some(bookmark) = metadata.and_then(|entry| entry.associated_bookmark.as_deref()) {
        if delete_bookmarks && shared {
            message.push_str(&format!("; preserved shared bookmark {bookmark}"));
        } else if delete_bookmarks {
            message.push_str(&format!(
                "; deleted bookmark {bookmark} if it existed locally"
            ));
        } else {
            message.push_str(&format!("; preserved bookmark {bookmark}"));
        }
    }
    message
}

fn structural_block(row: &ManagerRow, prune: bool) -> Option<String> {
    let workspace = &row.workspace;
    if workspace.role.default {
        return Some("the default workspace cannot be removed".to_owned());
    }
    if workspace.role.current {
        return Some("the current workspace cannot be removed; switch away first".to_owned());
    }
    match (&workspace.path, prune) {
        (None, false) => Some("workspace path is missing; review it in prune mode".to_owned()),
        (Some(_), true) => {
            Some("workspace path still exists and is not a prune candidate".to_owned())
        }
        (Some(_), false) if !workspace.working_copy_refreshed => {
            Some("working copy could not be refreshed safely".to_owned())
        }
        _ => match workspace.working_copy {
            WorkingCopyStatus::Stale => Some("working copy is stale".to_owned()),
            WorkingCopyStatus::Unknown if workspace.path.is_some() => {
                Some("working-copy state is unknown".to_owned())
            }
            _ => None,
        },
    }
}

fn is_risky(row: &ManagerRow) -> bool {
    row.workspace.management == ManagementState::Unmanaged
        || matches!(
            row.work_integration,
            Integration::OutsideTrunk
                | Integration::Unknown
                | Integration::Unassociated
                | Integration::Conflicted
        )
        || matches!(
            row.bookmark_integration,
            Integration::OutsideTrunk | Integration::Unknown | Integration::Conflicted
        )
}

fn append_review_warnings(row: &ManagerRow, warnings: &mut Vec<String>) {
    if row.workspace.management == ManagementState::Unmanaged {
        warnings.push("workspace is unmanaged; no bookmark will be inferred or deleted".to_owned());
    }
    if matches!(row.work_integration, Integration::OutsideTrunk) {
        warnings.push("workspace contains work outside trunk".to_owned());
    }
    if matches!(row.work_integration, Integration::Conflicted) {
        warnings.push("workspace work is conflicted".to_owned());
    }
    if matches!(
        row.work_integration,
        Integration::Unknown | Integration::Unassociated
    ) {
        warnings.push("workspace integration could not be established".to_owned());
    }
    match row.bookmark_integration {
        Integration::OutsideTrunk => {
            warnings.push("associated bookmark points outside trunk".to_owned())
        }
        Integration::Conflicted => {
            warnings.push("associated bookmark has conflicting targets".to_owned())
        }
        Integration::Unknown => {
            warnings.push("associated bookmark integration could not be established".to_owned())
        }
        _ => {}
    }
}

fn managed_bookmark(
    workspace: &WorkspaceSnapshot,
    metadata: Option<&ManagedWorkspaceMetadata>,
) -> Option<String> {
    (workspace.management == ManagementState::Managed)
        .then(|| metadata.and_then(|entry| entry.associated_bookmark.clone()))
        .flatten()
}

/// Whether another registered, managed workspace records the same bookmark association.
fn shared_bookmark(
    store: &WorkspaceMetadataStore,
    registered: &BTreeSet<String>,
    removed_name: &str,
    removed_metadata: Option<&ManagedWorkspaceMetadata>,
) -> Result<bool> {
    let Some(bookmark) = managed_association(removed_metadata) else {
        return Ok(false);
    };
    Ok(store.list()?.into_iter().any(|entry| {
        entry.workspace_name != removed_name
            && registered.contains(&entry.workspace_name)
            && entry.associated_bookmark.as_deref() == Some(bookmark)
    }))
}

/// The sorted targets of a row's associated bookmark, from one frozen listing of all bookmarks.
fn bookmark_targets(
    all_targets: &BTreeMap<String, Vec<String>>,
    metadata: Option<&ManagedWorkspaceMetadata>,
) -> Vec<String> {
    managed_association(metadata)
        .and_then(|bookmark| all_targets.get(bookmark))
        .cloned()
        .unwrap_or_default()
}

fn ensure_status_unchanged(expected: &ManagerRow, current: &ManagerRow) -> Result<()> {
    let expected_workspace = &expected.workspace;
    let current_workspace = &current.workspace;
    let unchanged = expected_workspace.name == current_workspace.name
        && expected_workspace.path == current_workspace.path
        && expected_workspace.role == current_workspace.role
        && expected_workspace.management == current_workspace.management
        && expected_workspace.working_copy == current_workspace.working_copy
        && expected_workspace.working_copy_refreshed == current_workspace.working_copy_refreshed
        && expected_workspace.change_id == current_workspace.change_id
        && expected_workspace.commit_id == current_workspace.commit_id
        && expected_workspace.associated_bookmark == current_workspace.associated_bookmark
        && expected_workspace.created_at_unix_ms == current_workspace.created_at_unix_ms
        && expected_workspace.creation_operation_id == current_workspace.creation_operation_id
        && expected_workspace.creation_base_commit_id == current_workspace.creation_base_commit_id
        && expected_workspace.intended_remote == current_workspace.intended_remote
        && expected.bookmark_integration == current.bookmark_integration
        && expected.work_integration == current.work_integration;
    if unchanged {
        Ok(())
    } else {
        bail!("workspace state changed since the removal preview; review it again")
    }
}

fn inspect_workspace_path(
    repository_client: &JjClient,
    path: &Path,
    workspace_name: &str,
) -> Result<PathIdentity> {
    if !path.is_absolute() {
        bail!("workspace path is not absolute: {}", path.display())
    }
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("workspace path is unreadable: {}", path.display()))?;
    if metadata.file_type().is_symlink() {
        bail!("workspace root is a symlink: {}", path.display())
    }
    if !metadata.is_dir() {
        bail!("workspace path is not a directory: {}", path.display())
    }
    let canonical = canonicalize(path)?;
    if canonical.parent().is_none() {
        bail!("refusing unsafe workspace root: {}", canonical.display())
    }

    let jj_metadata = fs::symlink_metadata(path.join(".jj"))
        .context("workspace root has no readable .jj metadata")?;
    if !jj_metadata.is_dir() || jj_metadata.file_type().is_symlink() {
        bail!("workspace root has unsafe .jj metadata")
    }
    let repo_pointer = fs::symlink_metadata(path.join(".jj/repo"))
        .context("workspace root has no readable .jj repository pointer")?;
    if repo_pointer.is_dir() {
        bail!("workspace root contains the repository storage")
    }
    if fs::symlink_metadata(path.join(".git")).is_ok() {
        bail!("workspace root contains Git repository metadata")
    }
    let current_dir = fs::canonicalize(std::env::current_dir()?)?;
    if path_contains(&canonical, &current_dir) {
        bail!("workspace directory contains the manager's current directory")
    }

    let target_client = JjClient::new(path);
    let root = target_client
        .run(["--ignore-working-copy", "workspace", "root"])
        .context("workspace directory does not contain readable JJ metadata")?
        .trimmed_stdout()?;
    if canonicalize(Path::new(&root))? != canonical {
        bail!("workspace directory resolves to a different JJ workspace root")
    }
    if repository_identity(repository_client)? != repository_identity(&target_client)? {
        bail!("workspace directory belongs to a different JJ repository")
    }
    let current_targets = target_client.current_workspace_target_names()?;
    if current_targets.as_slice() != [workspace_name] {
        bail!(
            "workspace directory identity changed: expected {workspace_name}, found {}",
            if current_targets.is_empty() {
                "no current workspace target".to_owned()
            } else {
                current_targets.join(", ")
            }
        )
    }
    ensure_no_nested_workspace(&target_client, workspace_name, &canonical)?;
    Ok(PathIdentity {
        canonical,
        modified_ns: modified_ns(&metadata),
        #[cfg(unix)]
        device: metadata.dev(),
        #[cfg(unix)]
        inode: metadata.ino(),
    })
}

fn inspect_missing_workspace(
    ownership: &OwnershipView<'_>,
    workspace_name: &str,
) -> Result<PathBuf> {
    let path = ownership.recorded_root(workspace_name)?.ok_or_else(|| {
        anyhow!(
            "workspace path is unknown rather than confirmed missing: JJ has no recorded path for {workspace_name}"
        )
    })?;
    match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(path),
        Err(error) => Err(error).with_context(|| {
            format!(
                "workspace path could not be checked safely: {}",
                path.display()
            )
        }),
        Ok(_) => bail!(
            "workspace path is not a confirmed-missing directory: {}",
            path.display()
        ),
    }
}

/// Refuse to delete a directory that contains another registered checkout, using one listing of
/// every recorded root. A recorded checkout that no longer exists cannot be harmed and is skipped.
fn ensure_no_nested_workspace(client: &JjClient, own_name: &str, root: &Path) -> Result<()> {
    for (name, recorded) in client.recorded_workspace_roots(None)? {
        if name == own_name {
            continue;
        }
        let RecordedRoot::Path(path) = recorded else {
            bail!(
                "could not verify registered workspace {name} before removal: it has no recorded path"
            )
        };
        let nested = match fs::canonicalize(&path) {
            Ok(nested) => nested,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("could not verify registered workspace {name} before removal")
                });
            }
        };
        if nested != root && nested.starts_with(root) {
            bail!(
                "workspace contains registered workspace {name} at {}",
                nested.display()
            )
        }
    }
    Ok(())
}

fn repository_identity(client: &JjClient) -> Result<PathBuf> {
    let config = client.repo_config_path()?;
    let directory = config.parent().ok_or_else(|| {
        anyhow!(
            "JJ repository config path has no parent: {}",
            config.display()
        )
    })?;
    canonicalize(directory)
}

fn path_contains(parent: &Path, child: &Path) -> bool {
    child.starts_with(parent)
}

fn scan_ignored(root: &Path) -> Result<IgnoredScan> {
    let output = JjClient::new(root).run([
        "--ignore-working-copy",
        "file",
        "list",
        "-r",
        "@",
        "--template",
        "path ++ \"\\0\"",
    ])?;
    let tracked = output
        .stdout()?
        .split_terminator('\0')
        .map(PathBuf::from)
        .collect::<BTreeSet<_>>();
    let tracked_parents = tracked
        .iter()
        .flat_map(|path| path.ancestors().skip(1).map(Path::to_path_buf))
        .filter(|path| !path.as_os_str().is_empty())
        .collect::<BTreeSet<_>>();
    let mut display = Vec::new();
    let mut inventory = Vec::new();
    scan_directory(
        root,
        root,
        &tracked,
        &tracked_parents,
        false,
        &mut display,
        &mut inventory,
    )?;
    display.sort();
    inventory.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(IgnoredScan { display, inventory })
}

fn scan_directory(
    root: &Path,
    directory: &Path,
    tracked: &BTreeSet<PathBuf>,
    tracked_parents: &BTreeSet<PathBuf>,
    parent_listed: bool,
    display: &mut Vec<String>,
    inventory: &mut Vec<IgnoredEntry>,
) -> Result<()> {
    let mut entries = fs::read_dir(directory)
        .with_context(|| format!("workspace directory is unreadable: {}", directory.display()))?
        .collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        let path = entry.path();
        let relative = path
            .strip_prefix(root)
            .expect("directory walk stays below its root")
            .to_path_buf();
        if relative.components().count() == 1 && matches!(relative.to_str(), Some(".jj" | ".git")) {
            continue;
        }
        if matches!(
            path.file_name().and_then(|name| name.to_str()),
            Some(".jj" | ".git")
        ) {
            bail!(
                "workspace contains nested repository metadata at {}",
                path.display()
            )
        }

        let metadata = fs::symlink_metadata(&path)
            .with_context(|| format!("failed to inspect {}", path.display()))?;
        let has_tracked_descendant = tracked_parents.contains(&relative);
        let is_tracked = tracked.contains(&relative);
        let is_unrecorded = !is_tracked && !has_tracked_descendant;
        let kind = if metadata.file_type().is_symlink() {
            IgnoredKind::Symlink
        } else if metadata.is_dir() {
            IgnoredKind::Directory
        } else if metadata.is_file() {
            IgnoredKind::File
        } else {
            IgnoredKind::Other
        };

        if is_unrecorded {
            inventory.push(IgnoredEntry {
                path: relative.clone(),
                kind,
                length: metadata.len(),
                modified_ns: modified_ns(&metadata),
                symlink_target: (kind == IgnoredKind::Symlink)
                    .then(|| fs::read_link(&path))
                    .transpose()
                    .with_context(|| format!("failed to read symlink {}", path.display()))?,
            });
            if !parent_listed {
                let suffix = if kind == IgnoredKind::Directory {
                    "/"
                } else {
                    ""
                };
                display.push(format!("{}{suffix}", relative.display()));
            }
        }

        if kind == IgnoredKind::Directory {
            scan_directory(
                root,
                &path,
                tracked,
                tracked_parents,
                parent_listed || is_unrecorded,
                display,
                inventory,
            )?;
        }
    }
    Ok(())
}

fn metadata_store(client: &JjClient) -> Result<WorkspaceMetadataStore> {
    WorkspaceMetadataStore::from_repo_config_path(client.repo_config_path()?)
}

type MetadataIdentity<'a> = (
    &'a str,
    u64,
    &'a str,
    &'a str,
    Option<&'a str>,
    Option<&'a str>,
);

fn metadata_identity(metadata: &Option<ManagedWorkspaceMetadata>) -> Option<MetadataIdentity<'_>> {
    metadata.as_ref().map(|entry| {
        (
            entry.workspace_name.as_str(),
            entry.created_at_unix_ms,
            entry.creation_operation_id.as_str(),
            entry.creation_base_commit_id.as_str(),
            entry.associated_bookmark.as_deref(),
            entry.intended_remote.as_deref(),
        )
    })
}

fn workspace_metadata_identity(workspace: &WorkspaceSnapshot) -> Option<MetadataIdentity<'_>> {
    if workspace.management != ManagementState::Managed {
        return None;
    }
    Some((
        workspace.name.as_str(),
        workspace.created_at_unix_ms.unwrap_or_default(),
        workspace
            .creation_operation_id
            .as_deref()
            .unwrap_or_default(),
        workspace
            .creation_base_commit_id
            .as_deref()
            .unwrap_or_default(),
        workspace.associated_bookmark.as_deref(),
        workspace.intended_remote.as_deref(),
    ))
}

fn modified_ns(metadata: &fs::Metadata) -> Option<u128> {
    metadata
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|value| value.as_nanos())
}

fn canonicalize(path: &Path) -> Result<PathBuf> {
    fs::canonicalize(path).with_context(|| format!("failed to resolve {}", path.display()))
}

fn reject_duplicate_names(names: &[String]) -> Result<()> {
    let mut seen = BTreeSet::new();
    for name in names {
        if !seen.insert(name) {
            bail!("workspace listed more than once: {name}")
        }
    }
    Ok(())
}

fn set_blocked(blocked: &mut Option<String>, message: impl Into<String>) {
    if blocked.is_none() {
        *blocked = Some(message.into());
    }
}

fn failed(name: &str, message: impl Into<String>) -> BatchOutcome {
    let message = message.into();
    BatchOutcome {
        name: name.to_owned(),
        state: if message.contains("partial removal:") {
            OutcomeState::Partial
        } else {
            OutcomeState::Failed
        },
        message,
    }
}

fn skipped(name: &str, message: impl Into<String>) -> BatchOutcome {
    BatchOutcome {
        name: name.to_owned(),
        state: OutcomeState::Skipped,
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use tempfile::TempDir;

    const SCENARIO_ENV: &str = "JW_REMOVAL_TEST_SCENARIO";

    struct Repo {
        root: TempDir,
        repo: PathBuf,
        config: PathBuf,
    }

    impl Repo {
        fn new() -> Self {
            let root = TempDir::new().unwrap();
            let repo = root.path().join("repo");
            let config = root.path().join("config");
            fs::create_dir(&config).unwrap();
            run_with_config(
                root.path(),
                &config,
                &["git", "init", repo.to_str().unwrap()],
            );
            fs::write(repo.join("tracked.txt"), "tracked\n").unwrap();
            fs::write(repo.join(".gitignore"), "*.tmp\n").unwrap();
            run_with_config(&repo, &config, &["describe", "-m", "base"]);
            Self { root, repo, config }
        }

        fn path(&self) -> &Path {
            &self.repo
        }

        fn workspace_path(&self, name: &str) -> PathBuf {
            self.root.path().join(name)
        }

        fn add_workspace(&self, name: &str) -> PathBuf {
            let path = self.workspace_path(name);
            run_with_config(
                self.path(),
                &self.config,
                &[
                    "workspace",
                    "add",
                    "--config",
                    "git.colocate=false",
                    path.to_str().unwrap(),
                    "--name",
                    name,
                    "-r",
                    "default@",
                ],
            );
            path
        }

        fn workspace_names(&self) -> Vec<String> {
            self.lines(&[
                "--ignore-working-copy",
                "workspace",
                "list",
                "-T",
                "name ++ \"\\n\"",
            ])
        }

        fn bookmark_names(&self) -> Vec<String> {
            self.lines(&[
                "--ignore-working-copy",
                "bookmark",
                "list",
                "-T",
                "if(remote, \"\", name ++ \"\\n\")",
            ])
        }

        fn lines(&self, args: &[&str]) -> Vec<String> {
            let output = command_output(self.path(), &self.config, args);
            String::from_utf8(output.stdout)
                .unwrap()
                .lines()
                .map(ToOwned::to_owned)
                .collect()
        }
    }

    fn command_output(cwd: &Path, config: &Path, args: &[&str]) -> std::process::Output {
        let output = Command::new("jj")
            .current_dir(cwd)
            .env("XDG_CONFIG_HOME", config)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "jj {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    fn run_with_config(cwd: &Path, config: &Path, args: &[&str]) {
        command_output(cwd, config, args);
    }

    fn run_scenario(repo: &Repo, scenario: &str) {
        let output = Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("manager::removal::tests::removal_subprocess")
            .arg("--nocapture")
            .env(SCENARIO_ENV, scenario)
            .env("XDG_CONFIG_HOME", &repo.config)
            .current_dir(repo.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "removal child failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn manage_here(name: &str, bookmark: Option<&str>) {
        let client = JjClient::current().unwrap();
        let operation_id = client.operation_id().unwrap();
        let commit_id = client.resolve_one("default@").unwrap().commit_id;
        metadata_store(&client)
            .unwrap()
            .insert(&ManagedWorkspaceMetadata {
                workspace_name: name.to_owned(),
                created_at_unix_ms: 1,
                creation_operation_id: operation_id,
                creation_base_commit_id: commit_id,
                associated_bookmark: bookmark.map(ToOwned::to_owned),
                intended_remote: None,
                external_owner: None,
                owned_git_worktree: None,
            })
            .unwrap();
    }

    #[test]
    fn ignored_scan_lists_directory_once_and_does_not_follow_symlinks() {
        let repo = Repo::new();
        let ignored = repo.path().join("ignored");
        fs::create_dir(&ignored).unwrap();
        fs::write(ignored.join("secret.txt"), "secret").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(repo.path(), ignored.join("loop")).unwrap();

        run_scenario(&repo, "scan-symlink");
    }

    #[test]
    fn ignored_inventory_detects_content_metadata_drift() {
        let repo = Repo::new();
        let path = repo.path().join("ignored.bin");
        fs::write(&path, b"one").unwrap();
        run_scenario(&repo, "inventory-drift");
    }

    #[test]
    fn primary_repository_storage_is_an_unoverrideable_path_blocker() {
        let repo = Repo::new();
        run_scenario(&repo, "primary-block");
    }

    #[cfg(unix)]
    #[test]
    fn symlink_workspace_root_is_an_unoverrideable_path_blocker() {
        let repo = Repo::new();
        let workspace = repo.add_workspace("feature");
        let link = repo.workspace_path("feature-link");
        std::os::unix::fs::symlink(&workspace, &link).unwrap();
        run_scenario(&repo, "symlink-block");
    }

    #[test]
    fn shared_managed_bookmark_is_preserved_for_surviving_workspace() {
        let repo = Repo::new();
        repo.add_workspace("one");
        repo.add_workspace("two");
        run_scenario(&repo, "shared");
    }

    #[test]
    fn approved_unmanaged_removal_preserves_an_inferred_bookmark() {
        let repo = Repo::new();
        let workspace = repo.add_workspace("unmanaged");
        run_with_config(
            repo.path(),
            &repo.config,
            &["bookmark", "create", "wip/unmanaged", "-r", "unmanaged@"],
        );
        run_scenario(&repo, "unmanaged-inferred-bookmark");

        assert!(!workspace.exists());
        assert!(
            !repo
                .workspace_names()
                .iter()
                .any(|name| name == "unmanaged")
        );
        assert!(
            repo.bookmark_names()
                .iter()
                .any(|name| name == "wip/unmanaged")
        );
    }

    #[test]
    fn mixed_batch_removes_safe_row_and_excludes_risky_row() {
        let repo = Repo::new();
        repo.add_workspace("safe");
        repo.add_workspace("risky");
        run_scenario(&repo, "mixed");

        let names = repo.workspace_names();
        assert!(!names.iter().any(|name| name == "safe"));
        assert!(names.iter().any(|name| name == "risky"));
        assert!(!repo.workspace_path("safe").exists());
        assert!(repo.workspace_path("risky").exists());
    }

    #[test]
    fn bookmark_choice_is_respected_for_keep_and_delete() {
        for (scenario, name, bookmark, should_exist) in [
            ("keep", "keep", "wip/keep", true),
            ("delete", "delete", "wip/delete", false),
        ] {
            let repo = Repo::new();
            repo.add_workspace(name);
            run_scenario(&repo, scenario);

            assert_eq!(
                repo.bookmark_names().iter().any(|name| name == bookmark),
                should_exist
            );
        }
    }

    #[test]
    fn drift_failure_does_not_stop_independent_removal() {
        let repo = Repo::new();
        repo.add_workspace("drift");
        repo.add_workspace("independent");
        run_scenario(&repo, "drift");

        let names = repo.workspace_names();
        assert!(names.iter().any(|name| name == "drift"));
        assert!(!names.iter().any(|name| name == "independent"));
    }

    #[cfg(unix)]
    #[test]
    fn partial_removal_does_not_stop_independent_removal() {
        let repo = Repo::new();
        let locked = repo.root.path().join("locked");
        fs::create_dir(&locked).unwrap();
        run_with_config(
            repo.path(),
            &repo.config,
            &[
                "workspace",
                "add",
                "--config",
                "git.colocate=false",
                locked.join("a-partial").to_str().unwrap(),
                "--name",
                "a-partial",
                "-r",
                "default@",
            ],
        );
        repo.add_workspace("independent");
        run_scenario(&repo, "partial");
        assert!(locked.join("a-partial").is_dir());
        assert!(!repo.workspace_path("independent").exists());
        assert_eq!(repo.workspace_names(), ["default"]);
    }

    #[test]
    fn prune_forgets_only_a_confirmed_missing_workspace() {
        let repo = Repo::new();
        let stale = repo.add_workspace("stale");
        fs::remove_dir_all(stale).unwrap();

        run_scenario(&repo, "prune");

        let names = repo.workspace_names();
        assert!(!names.iter().any(|name| name == "stale"));
        assert!(names.iter().any(|name| name == "default"));
    }

    #[test]
    fn ignored_content_requires_one_batch_acknowledgement() {
        let repo = Repo::new();
        let workspace = repo.add_workspace("ignored");
        fs::write(workspace.join("ignored.tmp"), "keep me visible").unwrap();

        run_scenario(&repo, "ignored");

        let names = repo.workspace_names();
        assert!(!names.iter().any(|name| name == "ignored"));
    }

    #[test]
    fn external_owner_blocks_ui_and_revalidates_a_frozen_removal_plan() {
        let repo = Repo::new();
        repo.add_workspace("owned");
        run_scenario(&repo, "ownership-drift");
        assert!(repo.workspace_names().iter().any(|name| name == "owned"));
    }

    #[test]
    fn removal_subprocess() {
        let Ok(scenario) = std::env::var(SCENARIO_ENV) else {
            return;
        };
        match scenario.as_str() {
            "ownership-drift" => {
                manage_here("owned", None);
                let inventory = workspace::WorkspaceInventory::load().unwrap();
                let low_level =
                    workspace::plan_remove_workspace(&inventory, Some("owned"), true).unwrap();
                let preview = prepare("default@", &["owned".to_owned()], false).unwrap();
                let client = JjClient::current().unwrap();
                let store = metadata_store(&client).unwrap();
                let original = store.get("owned").unwrap().unwrap();
                let mut owned = original.clone();
                owned.external_owner = Some(crate::ownership::ExternalOwner {
                    checkout_root: inventory.root("owned").unwrap(),
                    git_dir: inventory.current_root().join(".git/worktrees/owned"),
                    common_dir: inventory.current_root().join(".git"),
                });
                assert!(store.replace_if_matches(&original, &owned).unwrap());
                let error = workspace::execute_remove_workspace(low_level, true).unwrap_err();
                assert!(error.to_string().contains("externally owned"));
                let outcomes = execute(preview, true, true, true);
                assert!(!outcomes[0].success());
                assert!(outcomes[0].message.contains("externally owned"));
                let blocked = prepare("default@", &["owned".to_owned()], false).unwrap();
                assert!(
                    blocked.rows[0]
                        .blocked
                        .as_ref()
                        .unwrap()
                        .contains("externally owned")
                );
                assert!(inventory.root("owned").unwrap().exists());
                fs::remove_dir_all(inventory.root("owned").unwrap()).unwrap();
                let prune = prepare("default@", &["owned".to_owned()], true).unwrap();
                assert!(
                    prune.rows[0]
                        .blocked
                        .as_ref()
                        .unwrap()
                        .contains("externally owned")
                );
                assert!(store.get("owned").unwrap().is_some());
            }
            "scan-symlink" => {
                let scan = scan_ignored(Path::new(".")).unwrap();
                assert!(scan.display.iter().any(|path| path == "ignored/"));
                assert!(
                    !scan
                        .display
                        .iter()
                        .any(|path| path.contains("loop/ignored"))
                );
                #[cfg(unix)]
                assert!(
                    scan.inventory
                        .iter()
                        .any(|entry| entry.kind == IgnoredKind::Symlink)
                );
            }
            "inventory-drift" => {
                let before = scan_ignored(Path::new(".")).unwrap().inventory;
                fs::write("ignored.bin", b"changed-size").unwrap();
                let after = scan_ignored(Path::new(".")).unwrap().inventory;
                assert_ne!(before, after);
            }
            "primary-block" => {
                let client = JjClient::current().unwrap();
                let error = inspect_workspace_path(&client, client.cwd(), "default").unwrap_err();
                assert!(error.to_string().contains("repository storage"));
            }
            "symlink-block" => {
                let client = JjClient::current().unwrap();
                let link = std::env::current_dir().unwrap().join("../feature-link");
                let error = inspect_workspace_path(&client, &link, "feature").unwrap_err();
                assert!(error.to_string().contains("symlink"));
            }
            "shared" => {
                JjClient::current()
                    .unwrap()
                    .run(["bookmark", "create", "wip/shared", "-r", "default@"])
                    .unwrap();
                manage_here("one", Some("wip/shared"));
                manage_here("two", Some("wip/shared"));
                let client = JjClient::current().unwrap();
                let store = metadata_store(&client).unwrap();
                let metadata = store.get("one").unwrap().unwrap();
                let registered = client.workspace_names().unwrap().into_iter().collect();
                assert!(shared_bookmark(&store, &registered, "one", Some(&metadata)).unwrap());
                let plan = prepare("default@", &["one".into()], false).unwrap();
                let outcomes = execute(plan, true, false, true);
                assert!(outcomes[0].success(), "{}", outcomes[0].message);
                assert!(outcomes[0].message.contains("preserved shared bookmark"));
                assert!(client.resolve_one("wip/shared").is_ok());
                assert!(client.workspace_names().unwrap().contains(&"two".into()));
            }
            "unmanaged-inferred-bookmark" => {
                let inventory = workspace::WorkspaceInventory::load().unwrap();
                let lifecycle_plan =
                    workspace::plan_remove_workspace(&inventory, Some("unmanaged"), true).unwrap();
                assert!(lifecycle_plan.bookmarks.contains(&"wip/unmanaged".into()));

                let store = metadata_store(&JjClient::current().unwrap()).unwrap();
                assert!(store.get("unmanaged").unwrap().is_none());
                let manager_plan = prepare("default@", &["unmanaged".to_owned()], false).unwrap();
                let row = &manager_plan.rows[0];
                assert!(
                    row.risky,
                    "the operator must explicitly approve unmanaged removal"
                );
                assert_eq!(
                    row.bookmark, None,
                    "unmanaged bookmark is not an authoritative association"
                );
                assert!(
                    row.warnings
                        .iter()
                        .any(|warning| warning.contains("no bookmark will be inferred"))
                );
                let outcomes = execute(manager_plan, true, true, true);
                assert!(outcomes[0].success(), "{}", outcomes[0].message);
                assert!(
                    JjClient::current()
                        .unwrap()
                        .resolve_one("wip/unmanaged")
                        .is_ok(),
                    "approved removal must preserve the unmanaged bookmark"
                );
            }
            "mixed" => {
                manage_here("safe", None);
                let plan =
                    prepare("default@", &["safe".to_owned(), "risky".to_owned()], false).unwrap();
                assert!(
                    !plan
                        .rows
                        .iter()
                        .find(|row| row.name == "safe")
                        .unwrap()
                        .risky
                );
                assert!(
                    plan.rows
                        .iter()
                        .find(|row| row.name == "risky")
                        .unwrap()
                        .risky
                );
                let outcomes = execute(plan, false, false, true);
                assert!(
                    outcomes
                        .iter()
                        .find(|row| row.name == "safe")
                        .unwrap()
                        .success()
                );
                assert!(
                    !outcomes
                        .iter()
                        .find(|row| row.name == "risky")
                        .unwrap()
                        .success()
                );
            }
            "keep" | "delete" => {
                let name = scenario.as_str();
                let bookmark = format!("wip/{name}");
                JjClient::current()
                    .unwrap()
                    .run(["bookmark", "create", &bookmark, "-r", "default@"])
                    .unwrap();
                manage_here(name, Some(&bookmark));
                let plan = prepare("default@", &[name.to_owned()], false).unwrap();
                let outcomes = execute(plan, scenario == "delete", false, true);
                assert!(outcomes[0].success(), "{}", outcomes[0].message);
            }
            "drift" => {
                manage_here("drift", None);
                manage_here("independent", None);
                let plan = prepare(
                    "default@",
                    &["drift".to_owned(), "independent".to_owned()],
                    false,
                )
                .unwrap();
                fs::write("../drift/late.tmp", "new after review").unwrap();
                let outcomes = execute(plan, false, false, true);
                assert!(
                    !outcomes
                        .iter()
                        .find(|row| row.name == "drift")
                        .unwrap()
                        .success()
                );
                assert!(
                    outcomes
                        .iter()
                        .find(|row| row.name == "independent")
                        .unwrap()
                        .success()
                );
            }
            "prune" => {
                manage_here("stale", None);
                let plan = prepare("default@", &[], true).unwrap();
                assert_eq!(plan.rows.len(), 1);
                assert_eq!(plan.rows[0].name, "stale");
                assert_eq!(
                    plan.rows[0].path.as_ref().unwrap().file_name().unwrap(),
                    "stale"
                );
                assert!(plan.rows[0].blocked.is_none());
                let outcomes = execute(plan, false, false, true);
                assert!(outcomes[0].success(), "{}", outcomes[0].message);
            }
            #[cfg(unix)]
            "partial" => {
                use std::os::unix::fs::PermissionsExt;
                manage_here("a-partial", None);
                manage_here("independent", None);
                let plan = prepare(
                    "default@",
                    &["a-partial".into(), "independent".into()],
                    false,
                )
                .unwrap();
                let parent = Path::new("../locked");
                let permissions = fs::metadata(parent).unwrap().permissions();
                fs::set_permissions(parent, fs::Permissions::from_mode(0o500)).unwrap();
                let outcomes = execute(plan, false, false, true);
                fs::set_permissions(parent, permissions).unwrap();
                assert_eq!(
                    outcomes[0].state,
                    OutcomeState::Partial,
                    "{}",
                    outcomes[0].message
                );
                assert_eq!(
                    outcomes[1].state,
                    OutcomeState::Completed,
                    "{}",
                    outcomes[1].message
                );
            }
            "ignored" => {
                manage_here("ignored", None);
                let plan = prepare("default@", &["ignored".to_owned()], false).unwrap();
                assert_eq!(plan.rows[0].ignored, ["ignored.tmp"]);
                let outcomes = execute(plan, false, false, false);
                assert_eq!(outcomes[0].state, OutcomeState::Skipped);
                assert!(outcomes[0].message.contains("not approved for deletion"));

                let plan = prepare("default@", &["ignored".to_owned()], false).unwrap();
                let outcomes = execute(plan, false, false, true);
                assert!(outcomes[0].success(), "{}", outcomes[0].message);
            }
            other => panic!("unknown removal test scenario: {other}"),
        }
    }

    #[test]
    fn duplicate_batch_names_are_rejected_before_capture() {
        let names = vec!["a".to_owned(), "a".to_owned()];
        assert!(reject_duplicate_names(&names).is_err());
    }

    #[test]
    fn lifecycle_partial_progress_is_distinct_from_failure() {
        assert_eq!(
            failed("one", "identity changed").state,
            OutcomeState::Failed
        );
        assert_eq!(
            failed(
                "one",
                "partial removal: workspace one was forgotten; directory remains"
            )
            .state,
            OutcomeState::Partial
        );
    }
}
