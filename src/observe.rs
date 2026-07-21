use crate::jj::{JjClient, JjCommandError, JjErrorKind};
use crate::metadata::{ManagedWorkspaceMetadata, WorkspaceMetadataStore};
use crate::snapshot::{
    Hazard, HazardId, ManagementState, RepositorySnapshot, ResolvedTrunk, SnapshotCommand,
    SnapshotEnvelope, WorkingCopyStatus, WorkspaceRole, WorkspaceSnapshot,
};
use anyhow::{Context, Result, anyhow, bail};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const CAPTURE_ATTEMPTS: usize = 2;
const PREVIOUS_WORKSPACE_FILE: &str = "jw-prev-workspace";

// Keep every field JSON encoded. Tabs and newlines inside names and descriptions therefore cannot
// corrupt the record boundary. This avoids WorkspaceRef.root(), which is absent from older JJ
// releases in the supported compatibility window.
const WORKSPACE_NAMES_TEMPLATE: &str = r#"json(name) ++ "\n""#;
const CURRENT_WORKSPACE_NAMES_TEMPLATE: &str =
    r#"if(target.current_working_copy(), json(name) ++ "\n", "")"#;
const WORKSPACE_FACTS_TEMPLATE: &str = r#"json(name) ++ "\t" ++ json(target.change_id()) ++ "\t" ++ json(target.commit_id()) ++ "\t" ++ json(target.description().first_line()) ++ "\t" ++ json(target.current_working_copy()) ++ "\t" ++ json(target.empty()) ++ "\t" ++ json(target.conflict()) ++ "\t" ++ json(target.divergent()) ++ "\t" ++ json(target.diff().files().len()) ++ "\t" ++ json(if(target.conflict(), 0, target.diff().stat().total_added())) ++ "\t" ++ json(if(target.conflict(), 0, target.diff().stat().total_removed())) ++ "\t" ++ json(target.conflicted_files().len()) ++ "\n""#;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RefreshMode {
    None,
    #[default]
    Current,
    All,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedWorkspace {
    pub name: String,
    pub path: Option<PathBuf>,
}

/// Resolve a workspace routing token without snapshotting any working copy or resolving trunk.
pub fn resolve_workspace_token(client: &JjClient, token: &str) -> Result<ResolvedWorkspace> {
    let inventory = PreliminaryInventory::discover(client)?;
    resolve_workspace_token_in(&inventory, token)
}

/// Captures one consistent view of repository and workspace state.
///
/// Construction discovers JJ's repository configuration path. On an older repository, JJ may
/// initialize its empty secure-config directory while answering that documented query; it does not
/// change the repository operation or working copy. During capture, only the explicitly selected
/// working copies may run normal JJ commands. Every JJ query after the final operation ID is
/// captured uses `--at-operation` and `--ignore-working-copy` through `JjClient`.
#[derive(Debug, Clone)]
pub struct ObservationEngine {
    client: JjClient,
    trunk_revset: String,
    metadata_store: WorkspaceMetadataStore,
}

impl ObservationEngine {
    pub fn new(client: JjClient, trunk_revset: impl Into<String>) -> Result<Self> {
        let metadata_store =
            WorkspaceMetadataStore::from_repo_config_path(client.repo_config_path()?)?;
        Ok(Self::with_metadata_store(
            client,
            trunk_revset,
            metadata_store,
        ))
    }

    pub(crate) fn with_metadata_store(
        client: JjClient,
        trunk_revset: impl Into<String>,
        metadata_store: WorkspaceMetadataStore,
    ) -> Self {
        Self {
            client,
            trunk_revset: trunk_revset.into(),
            metadata_store,
        }
    }

    pub fn capture_list(&self, refresh: RefreshMode) -> Result<SnapshotEnvelope> {
        self.capture(CaptureSelection::List, refresh)
    }

    /// Capture one workspace. `workspace` accepts `@` for current, `-` for previous, and `^` or
    /// `default` for default. Any other value is treated as a literal workspace name.
    pub fn capture_status(
        &self,
        workspace: &str,
        refresh: RefreshMode,
    ) -> Result<SnapshotEnvelope> {
        self.capture(CaptureSelection::Status(workspace.to_owned()), refresh)
    }

    fn capture(
        &self,
        selection: CaptureSelection,
        refresh: RefreshMode,
    ) -> Result<SnapshotEnvelope> {
        let mut last_drift = None;

        for attempt in 0..CAPTURE_ATTEMPTS {
            let inventory = PreliminaryInventory::discover(&self.client)?;
            let resolved_selection = selection.resolve(&inventory)?;

            let refresh_names = resolved_selection.refresh_names(&inventory, refresh);
            let refresh_states = self.refresh_sequentially(&inventory, &refresh_names)?;

            let operation_id = self.client.operation_id()?;
            let captured_at_unix_ms = unix_time_ms()?;
            let final_facts = self.workspace_facts_at(&operation_id)?;

            let preliminary_names = inventory.names();
            let final_names = final_facts.keys().cloned().collect::<BTreeSet<_>>();
            if preliminary_names != final_names {
                last_drift = Some((preliminary_names, final_names));
                if attempt + 1 < CAPTURE_ATTEMPTS {
                    continue;
                }
                break;
            }
            if !final_facts
                .get(&inventory.current)
                .is_some_and(|facts| facts.current_working_copy)
            {
                bail!(
                    "frozen workspace inventory did not mark `{}` as the current working-copy target",
                    inventory.current
                )
            }

            // Trunk cardinality is a capture error. Callers must not render a partial envelope.
            let trunk_revision = self
                .client
                .resolve_one_at(&operation_id, &self.trunk_revset)
                .with_context(|| {
                    format!(
                        "failed to resolve trunk revset `{}` to exactly one revision",
                        self.trunk_revset
                    )
                })?;

            let final_paths =
                self.workspace_paths_at(&operation_id, final_facts.keys(), &inventory.current)?;

            // One metadata read per successful capture. All joins and status derivation after this
            // point happen in memory; creation-base existence is queried at the frozen operation.
            let metadata = self
                .metadata_store
                .list()?
                .into_iter()
                .map(|entry| (entry.workspace_name.clone(), entry))
                .collect::<BTreeMap<_, _>>();
            let missing_creation_bases =
                self.missing_creation_bases_at(&operation_id, &metadata, final_facts.keys());

            let workspaces = resolved_selection
                .selected_facts(&final_facts)?
                .into_iter()
                .map(|facts| {
                    derive_workspace_snapshot(DerivationInput {
                        facts,
                        path: final_paths.get(&facts.name).cloned().flatten(),
                        role: WorkspaceRole {
                            current: facts.name == inventory.current,
                            previous: inventory.previous.as_deref() == Some(facts.name.as_str()),
                            default: inventory.default.as_deref() == Some(facts.name.as_str()),
                        },
                        refresh_state: refresh_states.get(&facts.name).copied(),
                        metadata: metadata.get(&facts.name),
                        creation_base_missing: missing_creation_bases.contains(&facts.name),
                    })
                })
                .collect();

            return Ok(SnapshotEnvelope::new(
                resolved_selection.command(),
                RepositorySnapshot {
                    captured_at_unix_ms,
                    repository_id: self.metadata_store.repository_id().to_owned(),
                    operation_id,
                    trunk: ResolvedTrunk {
                        revset: self.trunk_revset.clone(),
                        change_id: trunk_revision.change_id,
                        commit_id: trunk_revision.commit_id,
                        description: trunk_revision.description,
                    },
                },
                workspaces,
                Vec::new(),
            ));
        }

        let (preliminary, final_names) = last_drift.expect("capture loop records workspace drift");
        bail!(
            "workspace inventory changed during snapshot capture (before: {}; after: {}); retry the command",
            display_names(&preliminary),
            display_names(&final_names)
        )
    }

    fn refresh_sequentially(
        &self,
        inventory: &PreliminaryInventory,
        selected: &BTreeSet<String>,
    ) -> Result<BTreeMap<String, RefreshState>> {
        let mut states = BTreeMap::new();
        for name in selected {
            let Some(root) = inventory.roots.get(name).and_then(Option::as_deref) else {
                continue;
            };
            let workspace_client = JjClient::new(root);
            match workspace_client.run(["status"]) {
                Ok(_) => {
                    states.insert(name.clone(), RefreshState::Refreshed);
                }
                Err(error) if is_stale_working_copy_error(&error) => {
                    states.insert(name.clone(), RefreshState::Stale);
                }
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("failed to refresh workspace `{name}` at {}", root.display())
                    });
                }
            }
        }
        Ok(states)
    }

    fn workspace_facts_at(&self, operation_id: &str) -> Result<BTreeMap<String, TargetFacts>> {
        let output = self.client.run_at(
            operation_id,
            ["workspace", "list", "-T", WORKSPACE_FACTS_TEMPLATE],
        )?;
        parse_workspace_facts(output.stdout()?)
    }

    fn workspace_paths_at<'a>(
        &self,
        operation_id: &str,
        names: impl Iterator<Item = &'a String>,
        current_name: &str,
    ) -> Result<BTreeMap<String, Option<PathBuf>>> {
        let frozen_current_root = query_workspace_root_at(&self.client, operation_id, None)?;
        let mut paths = BTreeMap::new();
        for name in names {
            let mut path = query_workspace_root_at(&self.client, operation_id, Some(name))?;
            if path.is_none() && name == current_name {
                path = frozen_current_root.clone();
            }
            paths.insert(name.clone(), path.filter(|path| path.is_dir()));
        }
        Ok(paths)
    }

    fn missing_creation_bases_at<'a>(
        &self,
        operation_id: &str,
        metadata: &BTreeMap<String, ManagedWorkspaceMetadata>,
        workspace_names: impl Iterator<Item = &'a String>,
    ) -> BTreeSet<String> {
        workspace_names
            .filter_map(|name| {
                let metadata = metadata.get(name)?;
                self.client
                    .resolve_one_at(operation_id, &metadata.creation_base_commit_id)
                    .is_err()
                    .then(|| name.clone())
            })
            .collect()
    }
}

#[derive(Debug, Clone)]
enum CaptureSelection {
    List,
    Status(String),
}

impl CaptureSelection {
    fn command(&self) -> SnapshotCommand {
        match self {
            Self::List => SnapshotCommand::List,
            Self::Status(_) => SnapshotCommand::Status,
        }
    }

    fn resolve(&self, inventory: &PreliminaryInventory) -> Result<Self> {
        let Self::Status(token) = self else {
            return Ok(Self::List);
        };
        Ok(Self::Status(
            resolve_workspace_token_in(inventory, token)?.name,
        ))
    }

    fn refresh_names(
        &self,
        inventory: &PreliminaryInventory,
        refresh: RefreshMode,
    ) -> BTreeSet<String> {
        match refresh {
            RefreshMode::None => BTreeSet::new(),
            RefreshMode::All => inventory.names(),
            RefreshMode::Current => match self {
                Self::List => BTreeSet::from([inventory.current.clone()]),
                Self::Status(name) => BTreeSet::from([name.clone()]),
            },
        }
    }

    fn selected_facts<'a>(
        &self,
        facts: &'a BTreeMap<String, TargetFacts>,
    ) -> Result<Vec<&'a TargetFacts>> {
        match self {
            Self::List => Ok(facts.values().collect()),
            Self::Status(name) => {
                Ok(vec![facts.get(name).ok_or_else(|| {
                    anyhow!("workspace not found in frozen snapshot: {name}")
                })?])
            }
        }
    }
}

fn resolve_workspace_token_in(
    inventory: &PreliminaryInventory,
    token: &str,
) -> Result<ResolvedWorkspace> {
    let name = match token {
        "@" => inventory.current.clone(),
        "-" => inventory
            .previous
            .clone()
            .ok_or_else(|| anyhow!("no previous workspace recorded"))?,
        "^" | "default" => inventory
            .default
            .clone()
            .ok_or_else(|| anyhow!("could not determine default workspace"))?,
        name => name.to_owned(),
    };
    let path = inventory
        .roots
        .get(&name)
        .ok_or_else(|| anyhow!("workspace not found: {name}"))?
        .clone();
    Ok(ResolvedWorkspace { name, path })
}

#[derive(Debug, Clone)]
struct PreliminaryInventory {
    roots: BTreeMap<String, Option<PathBuf>>,
    current: String,
    previous: Option<String>,
    default: Option<String>,
}

impl PreliminaryInventory {
    fn discover(client: &JjClient) -> Result<Self> {
        let output = client.run([
            "--ignore-working-copy",
            "workspace",
            "list",
            "-T",
            WORKSPACE_NAMES_TEMPLATE,
        ])?;
        let names = parse_json_lines(output.stdout()?, "workspace name")?;
        if names.is_empty() {
            bail!("JJ workspace inventory was empty")
        }

        let current_root = query_workspace_root(client, None)?
            .ok_or_else(|| anyhow!("current JJ workspace path is missing"))?;
        let current_root = canonicalize_existing(&current_root)?;

        let mut roots = BTreeMap::new();
        for name in &names {
            let root = query_workspace_root(client, Some(name))?
                .filter(|path| path.is_dir())
                .map(|path| canonicalize_existing(&path))
                .transpose()?;
            roots.insert(name.clone(), root);
        }

        let path_matches = roots
            .iter()
            .filter_map(|(name, root)| {
                (root.as_deref() == Some(current_root.as_path())).then_some(name)
            })
            .cloned()
            .collect::<Vec<_>>();
        let current = match path_matches.as_slice() {
            [name] => name.clone(),
            [] => {
                let output = client.run([
                    "--ignore-working-copy",
                    "workspace",
                    "list",
                    "-T",
                    CURRENT_WORKSPACE_NAMES_TEMPLATE,
                ])?;
                let target_matches = parse_json_lines(output.stdout()?, "current workspace name")?;
                let candidates = target_matches
                    .into_iter()
                    .filter(|name| roots.get(name).is_some_and(Option::is_none))
                    .collect::<Vec<_>>();
                match candidates.as_slice() {
                    [name] => {
                        roots.insert(name.clone(), Some(current_root.clone()));
                        name.clone()
                    }
                    _ => bail!(
                        "could not determine current workspace for root {}",
                        current_root.display()
                    ),
                }
            }
            _ => bail!(
                "multiple workspaces match current root {}: {}",
                current_root.display(),
                path_matches.join(", ")
            ),
        };

        let default = derive_default_workspace(&names, &current, &current_root)?;
        let previous = read_previous_workspace(&current_root, &names)?;

        Ok(Self {
            roots,
            current,
            previous,
            default,
        })
    }

    fn names(&self) -> BTreeSet<String> {
        self.roots.keys().cloned().collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TargetFacts {
    name: String,
    change_id: String,
    commit_id: String,
    description: String,
    current_working_copy: bool,
    empty: bool,
    conflicted: bool,
    divergent: bool,
    files: u32,
    added: u32,
    removed: u32,
    conflicts: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RefreshState {
    Refreshed,
    Stale,
}

struct DerivationInput<'a> {
    facts: &'a TargetFacts,
    path: Option<PathBuf>,
    role: WorkspaceRole,
    refresh_state: Option<RefreshState>,
    metadata: Option<&'a ManagedWorkspaceMetadata>,
    creation_base_missing: bool,
}

fn derive_workspace_snapshot(input: DerivationInput<'_>) -> WorkspaceSnapshot {
    let DerivationInput {
        facts,
        path,
        role,
        refresh_state,
        metadata,
        creation_base_missing,
    } = input;

    let working_copy = match refresh_state {
        Some(RefreshState::Stale) => WorkingCopyStatus::Stale,
        None => WorkingCopyStatus::Unknown,
        Some(RefreshState::Refreshed) if facts.conflicted => WorkingCopyStatus::Conflicted {
            conflicts: facts.conflicts,
        },
        Some(RefreshState::Refreshed) if facts.empty => WorkingCopyStatus::Empty,
        Some(RefreshState::Refreshed) => WorkingCopyStatus::Modified {
            files: facts.files,
            added: facts.added,
            removed: facts.removed,
        },
    };

    let mut hazards = Vec::new();
    if refresh_state == Some(RefreshState::Stale) {
        hazards.push(Hazard::new(
            HazardId::StaleWorkingCopy,
            "working copy is stale; run `jj workspace update-stale` in that workspace",
        ));
    }
    if facts.conflicted {
        hazards.push(Hazard::new(
            HazardId::Conflicted,
            format!(
                "working-copy revision contains {} conflicted files",
                facts.conflicts
            ),
        ));
    }
    if facts.divergent {
        hazards.push(Hazard::new(
            HazardId::DivergentChange,
            "working-copy change ID has multiple visible commits",
        ));
    }
    if metadata.is_none() {
        hazards.push(Hazard::new(
            HazardId::UnmanagedWorkspace,
            "workspace has no jj-waltz lifecycle metadata",
        ));
    }
    if metadata.is_some() && creation_base_missing {
        hazards.push(Hazard::new(
            HazardId::MissingCreationBase,
            "managed workspace creation base cannot be resolved at the captured operation",
        ));
    }
    if path.is_none() {
        hazards.push(Hazard::new(
            HazardId::MissingWorkspacePath,
            "JJ workspace has no existing working-copy path",
        ));
    }

    WorkspaceSnapshot {
        name: facts.name.clone(),
        path,
        role,
        management: if metadata.is_some() {
            ManagementState::Managed
        } else {
            ManagementState::Unmanaged
        },
        working_copy,
        working_copy_refreshed: refresh_state == Some(RefreshState::Refreshed),
        change_id: facts.change_id.clone(),
        commit_id: facts.commit_id.clone(),
        description: facts.description.clone(),
        associated_bookmark: metadata.and_then(|entry| entry.associated_bookmark.clone()),
        created_at_unix_ms: metadata.map(|entry| entry.created_at_unix_ms),
        creation_operation_id: metadata.map(|entry| entry.creation_operation_id.clone()),
        creation_base_commit_id: metadata.map(|entry| entry.creation_base_commit_id.clone()),
        intended_remote: metadata.and_then(|entry| entry.intended_remote.clone()),
        hazards,
    }
}

fn parse_workspace_facts(output: &str) -> Result<BTreeMap<String, TargetFacts>> {
    let mut facts = BTreeMap::new();
    for (index, line) in output.lines().enumerate() {
        if line.is_empty() {
            continue;
        }
        let fields = line.split('\t').collect::<Vec<_>>();
        let [
            name,
            change_id,
            commit_id,
            description,
            current_working_copy,
            empty,
            conflicted,
            divergent,
            files,
            added,
            removed,
            conflicts,
        ] = fields.as_slice()
        else {
            bail!(
                "JJ workspace query returned malformed record {} with {} fields; expected 12",
                index + 1,
                fields.len()
            )
        };

        let entry = TargetFacts {
            name: parse_json_field(name, index, "name")?,
            change_id: parse_json_field(change_id, index, "change ID")?,
            commit_id: parse_json_field(commit_id, index, "commit ID")?,
            description: parse_json_field(description, index, "description")?,
            current_working_copy: parse_json_field(
                current_working_copy,
                index,
                "current working copy",
            )?,
            empty: parse_json_field(empty, index, "empty")?,
            conflicted: parse_json_field(conflicted, index, "conflicted")?,
            divergent: parse_json_field(divergent, index, "divergent")?,
            files: parse_count(files, index, "files")?,
            added: parse_count(added, index, "added")?,
            removed: parse_count(removed, index, "removed")?,
            conflicts: parse_count(conflicts, index, "conflicts")?,
        };
        let name = entry.name.clone();
        if facts.insert(name.clone(), entry).is_some() {
            bail!("JJ workspace query returned duplicate workspace `{name}`")
        }
    }
    if facts.is_empty() {
        bail!("JJ workspace query returned no workspaces")
    }
    Ok(facts)
}

fn parse_json_lines(output: &str, field: &str) -> Result<Vec<String>> {
    output
        .lines()
        .filter(|line| !line.is_empty())
        .enumerate()
        .map(|(index, line)| parse_json_field(line, index, field))
        .collect()
}

fn parse_json_field<T>(value: &str, record_index: usize, field: &str) -> Result<T>
where
    T: serde::de::DeserializeOwned,
{
    serde_json::from_str(value).with_context(|| {
        format!(
            "JJ workspace query returned invalid {field} JSON in record {}",
            record_index + 1
        )
    })
}

fn parse_count(value: &str, record_index: usize, field: &str) -> Result<u32> {
    let value: u64 = parse_json_field(value, record_index, field)?;
    value.try_into().with_context(|| {
        format!(
            "JJ workspace query returned {field} count outside u32 range in record {}",
            record_index + 1
        )
    })
}

fn query_workspace_root(client: &JjClient, name: Option<&str>) -> Result<Option<PathBuf>> {
    let mut args = vec!["--ignore-working-copy", "workspace", "root"];
    if let Some(name) = name {
        args.extend(["--name", name]);
    }
    let output = client.run_unchecked(args)?;
    if output.success() {
        return parse_workspace_root(output.stdout()?).map(Some);
    }
    let message = output.stderr();
    if name.is_some() && is_missing_workspace_path_error(&message) {
        Ok(None)
    } else if let Some(name) = name {
        bail!("failed to resolve workspace root for {name}: {message}")
    } else {
        bail!("failed to resolve current workspace root: {message}")
    }
}

fn query_workspace_root_at(
    client: &JjClient,
    operation_id: &str,
    name: Option<&str>,
) -> Result<Option<PathBuf>> {
    let mut args = vec!["workspace", "root"];
    if let Some(name) = name {
        args.extend(["--name", name]);
    }
    let output = client.run_at_unchecked(operation_id, args)?;
    if output.success() {
        return parse_workspace_root(output.stdout()?).map(Some);
    }
    let message = output.stderr();
    if name.is_some() && is_missing_workspace_path_error(&message) {
        Ok(None)
    } else if let Some(name) = name {
        bail!("failed to resolve workspace root for {name}: {message}")
    } else {
        bail!("failed to resolve current workspace root: {message}")
    }
}

fn is_missing_workspace_path_error(message: &str) -> bool {
    message.contains("Cannot resolve absolute workspace path")
        && (message.contains("No such file or directory")
            || message.contains("os error 2")
            || message.contains("os error 3"))
        || message.contains("Workspace has no recorded path")
}

fn parse_workspace_root(output: &str) -> Result<PathBuf> {
    let paths = output
        .lines()
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    match paths.as_slice() {
        [path] => Ok(PathBuf::from(path)),
        _ => bail!(
            "JJ workspace root query returned {} paths; expected exactly one",
            paths.len()
        ),
    }
}

fn canonicalize_existing(path: &Path) -> Result<PathBuf> {
    fs::canonicalize(path)
        .with_context(|| format!("failed to resolve workspace path {}", path.display()))
}

fn derive_default_workspace(
    names: &[String],
    current: &str,
    current_root: &Path,
) -> Result<Option<String>> {
    if names.iter().any(|name| name == "default") {
        return Ok(Some("default".to_owned()));
    }

    let parent = current_root
        .parent()
        .ok_or_else(|| anyhow!("workspace root has no parent directory"))?;
    let mut base = current_root
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow!("workspace root has no valid basename"))?
        .to_owned();
    let suffix = format!(".{current}");
    if current != "default" && base.ends_with(&suffix) {
        base.truncate(base.len() - suffix.len());
    } else if current != "default" && base == current && base.contains('.') {
        if let Some((prefix, _)) = base.rsplit_once('.') {
            base = prefix.to_owned();
        }
    }
    let base_root = parent.join(base);
    Ok(
        (canonicalize_existing(&base_root).ok().as_deref() == Some(current_root))
            .then(|| current.to_owned()),
    )
}

fn read_previous_workspace(current_root: &Path, names: &[String]) -> Result<Option<String>> {
    let state_path = current_root.join(".jj").join(PREVIOUS_WORKSPACE_FILE);
    let contents = match fs::read_to_string(&state_path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
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
    match recorded.as_slice() {
        [name] if names.iter().any(|candidate| candidate == name) => Ok(Some((*name).to_owned())),
        _ => Ok(None),
    }
}

fn is_stale_working_copy_error(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<JjCommandError>()
        .is_some_and(|error| error.kind() == JjErrorKind::StaleWorkingCopy)
}

fn unix_time_ms() -> Result<u64> {
    let milliseconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?
        .as_millis();
    milliseconds
        .try_into()
        .context("current Unix timestamp does not fit in u64 milliseconds")
}

fn display_names(names: &BTreeSet<String>) -> String {
    if names.is_empty() {
        "<none>".to_owned()
    } else {
        names.iter().cloned().collect::<Vec<_>>().join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn facts() -> TargetFacts {
        TargetFacts {
            name: "solver".to_owned(),
            change_id: "change-1".to_owned(),
            commit_id: "commit-1".to_owned(),
            description: "Improve solver".to_owned(),
            current_working_copy: true,
            empty: false,
            conflicted: false,
            divergent: false,
            files: 3,
            added: 18,
            removed: 4,
            conflicts: 0,
        }
    }

    #[test]
    fn parses_json_tab_records_without_delimiter_ambiguity() {
        let output = [
            r#""solver\tui""#,
            r#""change-1""#,
            r#""commit-1""#,
            r#""line one\nline two""#,
            "true",
            "false",
            "true",
            "false",
            "3",
            "18",
            "4",
            "2",
        ]
        .join("\t")
            + "\n";
        let parsed = parse_workspace_facts(&output).unwrap();
        let facts = &parsed["solver\tui"];
        assert_eq!(facts.description, "line one\nline two");
        assert_eq!(facts.files, 3);
        assert_eq!(facts.conflicts, 2);
    }

    #[test]
    fn rejects_malformed_and_duplicate_workspace_records() {
        assert!(parse_workspace_facts("\"name\"\t\"too-few\"\n").is_err());
        let line = "\"same\"\t\"change\"\t\"commit\"\t\"description\"\ttrue\ttrue\tfalse\tfalse\t0\t0\t0\t0\n";
        assert!(parse_workspace_facts(&format!("{line}{line}")).is_err());
    }

    #[test]
    fn unrefreshed_state_is_unknown_and_roles_can_overlap() {
        let snapshot = derive_workspace_snapshot(DerivationInput {
            facts: &facts(),
            path: None,
            role: WorkspaceRole {
                current: true,
                previous: true,
                default: true,
            },
            refresh_state: None,
            metadata: None,
            creation_base_missing: false,
        });

        assert_eq!(snapshot.working_copy, WorkingCopyStatus::Unknown);
        assert!(!snapshot.working_copy_refreshed);
        assert!(snapshot.role.current && snapshot.role.previous && snapshot.role.default);
        assert_eq!(snapshot.management, ManagementState::Unmanaged);
        assert!(
            snapshot
                .hazards
                .iter()
                .any(|hazard| hazard.id == HazardId::UnmanagedWorkspace)
        );
        assert!(
            snapshot
                .hazards
                .iter()
                .any(|hazard| hazard.id == HazardId::MissingWorkspacePath)
        );
    }

    #[test]
    fn refreshed_derivation_counts_changes_conflicts_and_staleness() {
        let modified = facts();
        let snapshot = derive_workspace_snapshot(DerivationInput {
            facts: &modified,
            path: Some(PathBuf::from("/workspace")),
            role: WorkspaceRole::default(),
            refresh_state: Some(RefreshState::Refreshed),
            metadata: None,
            creation_base_missing: false,
        });
        assert_eq!(
            snapshot.working_copy,
            WorkingCopyStatus::Modified {
                files: 3,
                added: 18,
                removed: 4,
            }
        );

        let mut conflicted = facts();
        conflicted.conflicted = true;
        conflicted.conflicts = 2;
        let snapshot = derive_workspace_snapshot(DerivationInput {
            facts: &conflicted,
            path: Some(PathBuf::from("/workspace")),
            role: WorkspaceRole::default(),
            refresh_state: Some(RefreshState::Refreshed),
            metadata: None,
            creation_base_missing: false,
        });
        assert_eq!(
            snapshot.working_copy,
            WorkingCopyStatus::Conflicted { conflicts: 2 }
        );

        let snapshot = derive_workspace_snapshot(DerivationInput {
            facts: &modified,
            path: Some(PathBuf::from("/workspace")),
            role: WorkspaceRole::default(),
            refresh_state: Some(RefreshState::Stale),
            metadata: None,
            creation_base_missing: false,
        });
        assert_eq!(snapshot.working_copy, WorkingCopyStatus::Stale);
        assert!(!snapshot.working_copy_refreshed);
        assert!(
            snapshot
                .hazards
                .iter()
                .any(|hazard| hazard.id == HazardId::StaleWorkingCopy)
        );
    }

    #[test]
    fn managed_metadata_and_missing_base_are_derived_in_memory() {
        let metadata = ManagedWorkspaceMetadata {
            workspace_name: "solver".to_owned(),
            created_at_unix_ms: 1_750_000_000_123,
            creation_operation_id: "operation-1".to_owned(),
            creation_base_commit_id: "missing-base".to_owned(),
            associated_bookmark: Some("wip/solver".to_owned()),
            intended_remote: Some("origin".to_owned()),
        };
        let snapshot = derive_workspace_snapshot(DerivationInput {
            facts: &facts(),
            path: Some(PathBuf::from("/workspace")),
            role: WorkspaceRole::default(),
            refresh_state: None,
            metadata: Some(&metadata),
            creation_base_missing: true,
        });

        assert_eq!(snapshot.management, ManagementState::Managed);
        assert_eq!(snapshot.created_at_unix_ms, Some(1_750_000_000_123));
        assert_eq!(
            snapshot.creation_operation_id.as_deref(),
            Some("operation-1")
        );
        assert_eq!(snapshot.intended_remote.as_deref(), Some("origin"));
        assert!(
            snapshot
                .hazards
                .iter()
                .any(|hazard| hazard.id == HazardId::MissingCreationBase)
        );
    }

    #[test]
    fn only_documented_missing_workspace_path_errors_are_hidden() {
        assert!(is_missing_workspace_path_error(
            "Cannot resolve absolute workspace path: No such file or directory (os error 2)"
        ));
        assert!(is_missing_workspace_path_error(
            "Workspace has no recorded path"
        ));
        assert!(!is_missing_workspace_path_error(
            "workspace root failed because repository metadata is corrupt"
        ));
    }

    #[test]
    fn resolves_workspace_aliases_without_collapsing_roles() {
        let inventory = PreliminaryInventory {
            roots: BTreeMap::from([
                ("default".to_owned(), Some(PathBuf::from("/repo"))),
                ("docs".to_owned(), Some(PathBuf::from("/repo.docs"))),
                ("solver".to_owned(), Some(PathBuf::from("/repo.solver"))),
            ]),
            current: "solver".to_owned(),
            previous: Some("docs".to_owned()),
            default: Some("default".to_owned()),
        };

        assert_eq!(
            resolve_workspace_token_in(&inventory, "@").unwrap().name,
            "solver"
        );
        assert_eq!(
            resolve_workspace_token_in(&inventory, "-").unwrap().name,
            "docs"
        );
        assert_eq!(
            resolve_workspace_token_in(&inventory, "^").unwrap().name,
            "default"
        );
        assert_eq!(
            resolve_workspace_token_in(&inventory, "default")
                .unwrap()
                .name,
            "default"
        );
        assert_eq!(
            resolve_workspace_token_in(&inventory, "solver")
                .unwrap()
                .path,
            Some(PathBuf::from("/repo.solver"))
        );
        assert!(resolve_workspace_token_in(&inventory, "missing").is_err());
    }

    #[test]
    fn refresh_none_does_not_snapshot_modified_disk() {
        if Command::new("jj").arg("--version").output().is_err() {
            return;
        }

        let directory = tempfile::tempdir().unwrap();
        let init = Command::new("jj")
            .args(["git", "init"])
            .arg(directory.path())
            .output()
            .unwrap();
        assert!(
            init.status.success(),
            "{}",
            String::from_utf8_lossy(&init.stderr)
        );

        let client = JjClient::new(directory.path());
        let metadata_store = WorkspaceMetadataStore::from_repo_config_path(
            directory.path().join("repo-config.toml"),
        )
        .unwrap();
        let engine =
            ObservationEngine::with_metadata_store(client.clone(), "root()", metadata_store);
        let before = client.operation_id().unwrap();
        let resolved = resolve_workspace_token(&client, "@").unwrap();
        assert_eq!(resolved.name, "default");
        assert_eq!(
            resolved.path,
            Some(directory.path().canonicalize().unwrap())
        );
        assert_eq!(client.operation_id().unwrap(), before);
        fs::write(directory.path().join("dirty.txt"), "dirty\n").unwrap();

        let envelope = engine.capture_status("@", RefreshMode::None).unwrap();
        let after = client.operation_id().unwrap();

        assert_eq!(before, after);
        assert_eq!(envelope.repository.operation_id, before);
        assert_eq!(envelope.workspaces.len(), 1);
        assert_eq!(
            envelope.workspaces[0].working_copy,
            WorkingCopyStatus::Unknown
        );
        assert!(!envelope.workspaces[0].working_copy_refreshed);

        let refreshed = engine.capture_list(RefreshMode::All).unwrap();
        assert_eq!(refreshed.workspaces.len(), 1);
        assert_eq!(
            refreshed.workspaces[0].working_copy,
            WorkingCopyStatus::Modified {
                files: 1,
                added: 1,
                removed: 0,
            }
        );
        assert!(refreshed.workspaces[0].working_copy_refreshed);
    }
}
