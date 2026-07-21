use crate::config::{self, Config};
use crate::jj::{JjClient, ResolvedRevision};
use crate::links::{self, LinkApplication, LinkApplyReport};
use crate::metadata::{ManagedWorkspaceMetadata, WorkspaceMetadataStore};
use crate::workspace::{self, AddOptions, AddResult, SwitchOptions, SwitchResult};
use anyhow::{Context, Error, Result, anyhow, bail};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone)]
pub struct CreationPolicy {
    at_revset: Option<String>,
    explicit_bookmark: Option<String>,
    no_bookmark: bool,
    apply_links: bool,
    config: Config,
}

impl CreationPolicy {
    pub fn load(
        at_revset: Option<String>,
        explicit_bookmark: Option<String>,
        no_bookmark: bool,
        no_links: bool,
        workspace_count: usize,
    ) -> Result<Self> {
        if explicit_bookmark.is_some() && no_bookmark {
            bail!("--bookmark and --no-bookmark cannot be used together")
        }
        if explicit_bookmark.is_some() && workspace_count > 1 {
            bail!("--bookmark can only be used with a single workspace")
        }

        Ok(Self {
            at_revset,
            explicit_bookmark,
            no_bookmark,
            apply_links: !no_links,
            config: Config::load()?,
        })
    }

    fn bookmark_for(&self, workspace: &str, allow_explicit: bool) -> Option<String> {
        if self.no_bookmark {
            return None;
        }
        if allow_explicit && let Some(bookmark) = &self.explicit_bookmark {
            return Some(bookmark.clone());
        }
        self.config.workspace.create_bookmark.then(|| {
            config::bookmark_from_template(&self.config.workspace.bookmark_template, workspace)
        })
    }
}

#[derive(Debug, Clone)]
pub struct CreatedWorkspace {
    pub result: AddResult,
    pub links: Option<LinkApplyReport>,
}

#[derive(Debug, Clone)]
pub struct SwitchOutcome {
    pub intermediate: Vec<CreatedWorkspace>,
    pub result: SwitchResult,
    pub links: Option<LinkApplyReport>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdoptionRequest {
    pub workspace_name: String,
    pub workspace_root: PathBuf,
    pub base_revset: String,
    pub bookmark: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdoptionResult {
    pub metadata: ManagedWorkspaceMetadata,
    pub current_revision: ResolvedRevision,
}

#[derive(Debug, Clone)]
struct PlannedWorkspace {
    name: String,
    bookmark: Option<String>,
    created_at_unix_ms: u64,
}

#[derive(Debug)]
struct PendingWorkspace {
    result: AddResult,
    metadata: ManagedWorkspaceMetadata,
    links: Option<LinkApplication>,
}

impl PendingWorkspace {
    fn finish(self) -> CreatedWorkspace {
        CreatedWorkspace {
            result: self.result,
            links: self.links.map(LinkApplication::into_report),
        }
    }
}

pub fn add_workspaces(names: &[String], policy: &CreationPolicy) -> Result<Vec<CreatedWorkspace>> {
    let mut inventory = workspace::WorkspaceInventory::load()?;
    let resolved_names = names
        .iter()
        .map(|name| inventory.resolve(name))
        .collect::<Result<Vec<_>>>()?;
    let plans = resolved_names
        .into_iter()
        .map(|name| {
            Ok(PlannedWorkspace {
                bookmark: policy.bookmark_for(&name, true),
                name,
                created_at_unix_ms: unix_timestamp_ms()?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    if plans.is_empty() {
        return Ok(Vec::new());
    }
    let client = JjClient::current()?;
    let base = resolve_creation_base(&client, policy.at_revset.as_deref())?;
    let store = metadata_store(&client)?;
    preflight_creations(&inventory, &store, &plans)?;
    let config_root = policy
        .apply_links
        .then(|| inventory.root(inventory.default_name()?))
        .transpose()?;

    let mut pending = Vec::new();
    for plan in plans {
        match create_workspace(&plan, &base, &inventory, &store, config_root.as_deref()) {
            Ok(created) => {
                inventory.record_created(&created.result);
                pending.push(created);
            }
            Err(error) => return Err(rollback_after(error, Some(&store), &mut pending)),
        }
    }
    Ok(pending.into_iter().map(PendingWorkspace::finish).collect())
}

pub fn switch_workspaces(names: &[String], policy: &CreationPolicy) -> Result<SwitchOutcome> {
    let (final_name, intermediate_names) = names
        .split_last()
        .ok_or_else(|| anyhow!("at least one workspace name is required"))?;
    let mut inventory = workspace::WorkspaceInventory::load()?;

    // Resolve every token and validate every creation before mutating repository state.
    let resolved_intermediate = intermediate_names
        .iter()
        .map(|name| inventory.resolve(name))
        .collect::<Result<Vec<_>>>()?;
    let resolved_final = inventory.resolve(final_name)?;
    let existing = inventory
        .entries()
        .iter()
        .map(|entry| entry.name.clone())
        .collect::<HashSet<_>>();
    let mut available = existing;
    let mut intermediate_plans = Vec::new();
    for name in &resolved_intermediate {
        if available.insert(name.clone()) {
            intermediate_plans.push(PlannedWorkspace {
                bookmark: policy.bookmark_for(name, false),
                name: name.clone(),
                created_at_unix_ms: unix_timestamp_ms()?,
            });
        }
    }
    let final_plan = if available.insert(resolved_final.clone()) {
        Some(PlannedWorkspace {
            bookmark: policy.bookmark_for(&resolved_final, true),
            name: resolved_final.clone(),
            created_at_unix_ms: unix_timestamp_ms()?,
        })
    } else {
        None
    };
    let all_plans = intermediate_plans
        .iter()
        .chain(final_plan.iter())
        .cloned()
        .collect::<Vec<_>>();
    let client = JjClient::current()?;
    let base = if all_plans.is_empty() {
        None
    } else {
        Some(resolve_creation_base(&client, policy.at_revset.as_deref())?)
    };
    let store = if all_plans.is_empty() {
        None
    } else {
        let store = metadata_store(&client)?;
        preflight_creations(&inventory, &store, &all_plans)?;
        Some(store)
    };
    let config_root = policy
        .apply_links
        .then(|| inventory.root(inventory.default_name()?))
        .transpose()?;

    let mut intermediate = Vec::new();
    for plan in intermediate_plans {
        let base = base.as_ref().expect("creation plan has resolved base");
        let store = store.as_ref().expect("creation plan has metadata store");
        match create_workspace(&plan, base, &inventory, store, config_root.as_deref()) {
            Ok(created) => {
                inventory.record_created(&created.result);
                intermediate.push(created);
            }
            Err(error) => return Err(rollback_after(error, Some(store), &mut intermediate)),
        }
    }

    let mut final_created = if let Some(plan) = final_plan {
        let base = base.as_ref().expect("creation plan has resolved base");
        let store = store.as_ref().expect("creation plan has metadata store");
        match create_workspace(&plan, base, &inventory, store, config_root.as_deref()) {
            Ok(created) => Some(created),
            Err(error) => return Err(rollback_after(error, Some(store), &mut intermediate)),
        }
    } else {
        None
    };

    let result = if let Some(created) = &final_created {
        match workspace::switch_to_created_workspace(
            &inventory,
            &created.result,
            &SwitchOptions {
                preserve_subdir: true,
            },
        ) {
            Ok(result) => result,
            Err(error) => {
                let created = final_created.take().expect("created workspace exists");
                intermediate.push(created);
                return Err(rollback_after(error, store.as_ref(), &mut intermediate));
            }
        }
    } else {
        match workspace::switch_workspace(
            &inventory,
            &resolved_final,
            &SwitchOptions {
                preserve_subdir: true,
            },
        ) {
            Ok(result) => result,
            Err(error) => return Err(rollback_after(error, store.as_ref(), &mut intermediate)),
        }
    };

    let mut existing_links = if final_created.is_none() {
        match apply_links(config_root.as_deref(), &result.path) {
            Ok(application) => application,
            Err(error) => return Err(rollback_after(error, store.as_ref(), &mut intermediate)),
        }
    } else {
        None
    };

    if let Err(error) = workspace::record_switch(&result) {
        if let Some(created) = final_created.take() {
            intermediate.push(created);
        } else {
            let error = rollback_links_after(error, existing_links.take());
            return Err(rollback_after(error, store.as_ref(), &mut intermediate));
        }
        return Err(rollback_after(error, store.as_ref(), &mut intermediate));
    }

    let links = match final_created {
        Some(created) => created.links.map(LinkApplication::into_report),
        None => existing_links.map(LinkApplication::into_report),
    };
    Ok(SwitchOutcome {
        intermediate: intermediate
            .into_iter()
            .map(PendingWorkspace::finish)
            .collect(),
        result,
        links,
    })
}

/// Record an existing workspace without changing JJ graph or bookmarks.
#[allow(dead_code)] // CLI subcommand is wired by the integration lane.
pub fn adopt_workspace(request: &AdoptionRequest) -> Result<AdoptionResult> {
    if request.base_revset.trim().is_empty() {
        bail!("adoption base revision cannot be empty")
    }
    let client = JjClient::new(&request.workspace_root);
    let store = metadata_store(&client)?;
    adopt_workspace_with_store(request, &client, &store)
}

fn adopt_workspace_with_store(
    request: &AdoptionRequest,
    client: &JjClient,
    store: &WorkspaceMetadataStore,
) -> Result<AdoptionResult> {
    if store.get(&request.workspace_name)?.is_some() {
        bail!("workspace is already managed: {}", request.workspace_name)
    }

    let operation_id = client.operation_id()?;
    let current_revision = client
        .resolve_one_at(&operation_id, "@")
        .context("failed to resolve the workspace working-copy revision during adoption")?;
    let base = client
        .resolve_one_at(&operation_id, &request.base_revset)
        .with_context(|| {
            format!(
                "adoption base {:?} must resolve to exactly one revision",
                request.base_revset
            )
        })?;
    let bookmark = match &request.bookmark {
        Some(bookmark) => Some(bookmark.clone()),
        None => workspace::legacy_workspace_bookmark(&request.workspace_root)?,
    };
    let metadata = ManagedWorkspaceMetadata {
        workspace_name: request.workspace_name.clone(),
        created_at_unix_ms: unix_timestamp_ms()?,
        creation_operation_id: operation_id,
        creation_base_commit_id: base.commit_id,
        associated_bookmark: bookmark,
        intended_remote: None,
    };
    store.insert(&metadata)?;
    Ok(AdoptionResult {
        metadata,
        current_revision,
    })
}

fn preflight_creations(
    inventory: &workspace::WorkspaceInventory,
    store: &WorkspaceMetadataStore,
    plans: &[PlannedWorkspace],
) -> Result<()> {
    let mut names = HashSet::new();
    for plan in plans {
        if !names.insert(plan.name.as_str()) {
            bail!("workspace listed more than once: {}", plan.name)
        }
        workspace::preflight_add_workspace(inventory, &plan.name)?;
        if store.get(&plan.name)?.is_some() {
            bail!("workspace is already managed: {}", plan.name)
        }
    }
    Ok(())
}

fn resolve_creation_base(client: &JjClient, at_revset: Option<&str>) -> Result<ResolvedRevision> {
    let operation_id = client.operation_id()?;
    match at_revset {
        Some(revset) => client
            .resolve_one_at(&operation_id, revset)
            .with_context(|| format!("--at {revset:?} must resolve to exactly one revision")),
        None => client
            .resolve_one_at(&operation_id, "parents(@)")
            .map_err(|error| {
                anyhow!(
                    "cannot choose implicit creation base from parents(@): {error}; use --at @ to create on the current working-copy commit"
                )
            }),
    }
}

fn create_workspace(
    plan: &PlannedWorkspace,
    base: &ResolvedRevision,
    inventory: &workspace::WorkspaceInventory,
    store: &WorkspaceMetadataStore,
    config_root: Option<&Path>,
) -> Result<PendingWorkspace> {
    let result = workspace::add_workspace(
        inventory,
        &plan.name,
        &AddOptions {
            base_commit_id: base.commit_id.clone(),
            bookmark: plan.bookmark.clone(),
        },
    )
    .with_context(|| format!("failed to add workspace {}", plan.name))?;
    let metadata = ManagedWorkspaceMetadata {
        workspace_name: result.workspace.clone(),
        created_at_unix_ms: plan.created_at_unix_ms,
        creation_operation_id: result.creation_operation_id.clone(),
        creation_base_commit_id: result.creation_base_commit_id.clone(),
        associated_bookmark: result.bookmark.clone(),
        intended_remote: None,
    };

    if let Err(error) = store.insert(&metadata) {
        let cleanup = workspace::rollback_added_workspace(&result).err();
        return Err(with_cleanup_error(error, cleanup));
    }

    match apply_links(config_root, &result.path) {
        Ok(links) => Ok(PendingWorkspace {
            result,
            metadata,
            links,
        }),
        Err(error) => {
            let pending = PendingWorkspace {
                result,
                metadata,
                links: None,
            };
            Err(with_cleanup_error(
                error,
                rollback_pending(store, pending).err(),
            ))
        }
    }
}

fn metadata_store(client: &JjClient) -> Result<WorkspaceMetadataStore> {
    WorkspaceMetadataStore::from_repo_config_path(client.repo_config_path()?)
}

fn unix_timestamp_ms() -> Result<u64> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?
        .as_millis();
    u64::try_from(millis).context("system clock timestamp does not fit metadata format")
}

fn apply_links(config_root: Option<&Path>, path: &Path) -> Result<Option<LinkApplication>> {
    let Some(config_root) = config_root else {
        return Ok(None);
    };
    links::apply_workspace_links_reversible(config_root, path).map(Some)
}

fn rollback_links_after(error: Error, application: Option<LinkApplication>) -> Error {
    let Some(application) = application else {
        return error;
    };
    match application.rollback() {
        Ok(()) => error,
        Err(rollback_error) => error.context(format!(
            "workspace link cleanup also failed: {rollback_error:#}"
        )),
    }
}

fn rollback_pending(store: &WorkspaceMetadataStore, pending: PendingWorkspace) -> Result<()> {
    let mut errors = Vec::new();
    if let Some(links) = pending.links
        && let Err(error) = links.rollback()
    {
        errors.push(format!("remove workspace links: {error:#}"));
    }

    match workspace::rollback_added_workspace(&pending.result) {
        Ok(()) => match store.remove_if_matches(&pending.metadata) {
            Ok(true) => {}
            Ok(false) => errors.push(format!(
                "workspace metadata changed during cleanup and was retained: {}",
                pending.metadata.workspace_name
            )),
            Err(error) => errors.push(format!("remove workspace metadata: {error:#}")),
        },
        Err(error) => errors.push(format!(
            "remove workspace: {error:#}; metadata retained for repair"
        )),
    }

    if errors.is_empty() {
        Ok(())
    } else {
        bail!(errors.join("; "))
    }
}

fn rollback_after(
    error: Error,
    store: Option<&WorkspaceMetadataStore>,
    pending: &mut Vec<PendingWorkspace>,
) -> Error {
    let Some(store) = store else {
        debug_assert!(pending.is_empty());
        return error;
    };
    let cleanup_errors = pending
        .drain(..)
        .rev()
        .filter_map(|workspace| rollback_pending(store, workspace).err())
        .map(|error| error.to_string())
        .collect::<Vec<_>>();
    if cleanup_errors.is_empty() {
        error
    } else {
        error.context(format!(
            "workspace cleanup also failed: {}",
            cleanup_errors.join("; ")
        ))
    }
}

fn with_cleanup_error(error: Error, cleanup: Option<Error>) -> Error {
    match cleanup {
        Some(cleanup) => error.context(format!("workspace cleanup also failed: {cleanup:#}")),
        None => error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;
    use tempfile::TempDir;

    fn run_jj(cwd: &Path, args: &[&str]) -> String {
        let output = Command::new("jj")
            .current_dir(cwd)
            .args(args)
            .output()
            .expect("execute jj");
        assert!(
            output.status.success(),
            "jj {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    fn test_repo() -> (TempDir, PathBuf) {
        let tempdir = tempfile::tempdir().expect("create temp directory");
        let root = tempdir.path().join("repo");
        run_jj(
            tempdir.path(),
            &["git", "init", root.to_str().expect("UTF-8 test path")],
        );
        run_jj(&root, &["describe", "-m", "base"]);
        run_jj(&root, &["new"]);
        (tempdir, root)
    }

    #[test]
    fn implicit_base_rejects_multi_parent_working_copy() {
        if Command::new("jj").arg("--version").output().is_err() {
            return;
        }
        let (_tempdir, root) = test_repo();
        let mut parents = Vec::new();
        for index in 0..6 {
            run_jj(&root, &["new", "root()", "-m", &format!("parent {index}")]);
            parents.push(run_jj(
                &root,
                &["log", "-r", "@", "--no-graph", "-T", "commit_id"],
            ));
        }
        let mut args = vec!["new"];
        args.extend(parents.iter().map(String::as_str));
        run_jj(&root, &args);

        let client = JjClient::new(&root);
        let error = resolve_creation_base(&client, None).expect_err("ambiguous default base");
        assert!(error.to_string().contains("use --at @"));
        assert!(resolve_creation_base(&client, Some("@")).is_ok());
    }

    #[test]
    fn adoption_imports_legacy_marker_without_mutating_jj_operation() {
        if Command::new("jj").arg("--version").output().is_err() {
            return;
        }
        let (_tempdir, root) = test_repo();
        fs::write(root.join(".jj/jw-bookmark"), "wip/legacy\n").expect("write legacy marker");
        let client = JjClient::new(&root);
        let store =
            WorkspaceMetadataStore::from_repo_config_path(root.join(".jj/repo/config.toml"))
                .expect("test metadata store");
        let before = client.operation_id().expect("operation before adoption");
        let result = adopt_workspace_with_store(
            &AdoptionRequest {
                workspace_name: "default".to_owned(),
                workspace_root: root.clone(),
                base_revset: "parents(@)".to_owned(),
                bookmark: None,
            },
            &client,
            &store,
        )
        .expect("adopt workspace");
        let after = client.operation_id().expect("operation after adoption");

        assert_eq!(before, after);
        assert_eq!(
            result.metadata.associated_bookmark.as_deref(),
            Some("wip/legacy")
        );
        assert!(
            adopt_workspace_with_store(
                &AdoptionRequest {
                    workspace_name: "default".to_owned(),
                    workspace_root: root,
                    base_revset: "parents(@)".to_owned(),
                    bookmark: Some("explicit".to_owned()),
                },
                &client,
                &store,
            )
            .expect_err("already managed")
            .to_string()
            .contains("already managed")
        );
    }

    #[test]
    fn explicit_adoption_bookmark_wins_over_invalid_legacy_marker() {
        if Command::new("jj").arg("--version").output().is_err() {
            return;
        }
        let (_tempdir, root) = test_repo();
        fs::write(root.join(".jj/jw-bookmark"), "one\ntwo\n").expect("write invalid marker");
        let client = JjClient::new(&root);
        let store =
            WorkspaceMetadataStore::from_repo_config_path(root.join(".jj/repo/config.toml"))
                .expect("test metadata store");
        let result = adopt_workspace_with_store(
            &AdoptionRequest {
                workspace_name: "default".to_owned(),
                workspace_root: root,
                base_revset: "parents(@)".to_owned(),
                bookmark: Some("explicit".to_owned()),
            },
            &client,
            &store,
        )
        .expect("explicit bookmark bypasses legacy marker");
        assert_eq!(
            result.metadata.associated_bookmark.as_deref(),
            Some("explicit")
        );
    }
}
