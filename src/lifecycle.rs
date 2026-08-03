use crate::config::{self, Config};
use crate::links::{self, LinkApplication, LinkApplyReport};
use crate::workspace::{self, AddOptions, AddResult, SwitchOptions, SwitchResult};
use anyhow::{Context, Error, Result, bail};
use std::path::Path;

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

pub fn add_workspaces(names: &[String], policy: &CreationPolicy) -> Result<Vec<CreatedWorkspace>> {
    let mut inventory = workspace::WorkspaceInventory::load()?;
    let config_root = policy
        .apply_links
        .then(|| inventory.root(inventory.default_name()?))
        .transpose()?;
    let mut created = Vec::new();
    for name in names {
        let resolved = match inventory.resolve(name) {
            Ok(resolved) => resolved,
            Err(error) => return Err(rollback_after(error, &created)),
        };
        match create_workspace(&resolved, policy, true, &inventory, config_root.as_deref()) {
            Ok(outcome) => {
                inventory.record_created(&outcome.result);
                created.push(outcome);
            }
            Err(error) => return Err(rollback_after(error, &created)),
        }
    }
    Ok(created)
}

pub fn switch_workspaces(names: &[String], policy: &CreationPolicy) -> Result<SwitchOutcome> {
    let (final_name, intermediate_names) = names
        .split_last()
        .ok_or_else(|| anyhow::anyhow!("at least one workspace name is required"))?;
    let mut inventory = workspace::WorkspaceInventory::load()?;
    let config_root = policy
        .apply_links
        .then(|| inventory.root(inventory.default_name()?))
        .transpose()?;
    let mut intermediate = Vec::new();

    for name in intermediate_names {
        let resolved = match inventory.resolve(name) {
            Ok(resolved) => resolved,
            Err(error) => return Err(rollback_after(error, &intermediate)),
        };
        if inventory.contains(&resolved) {
            continue;
        }
        match create_workspace(&resolved, policy, false, &inventory, config_root.as_deref()) {
            Ok(outcome) => {
                inventory.record_created(&outcome.result);
                intermediate.push(outcome);
            }
            Err(error) => return Err(rollback_after(error, &intermediate)),
        }
    }

    let resolved_final = match inventory.resolve(final_name) {
        Ok(resolved) => resolved,
        Err(error) => return Err(rollback_after(error, &intermediate)),
    };
    let result = match workspace::switch_workspace(
        &inventory,
        &resolved_final,
        &SwitchOptions {
            at_revset: policy.at_revset.clone(),
            bookmark: policy.bookmark_for(&resolved_final, true),
            preserve_subdir: true,
        },
    ) {
        Ok(result) => result,
        Err(error) => return Err(rollback_after(error, &intermediate)),
    };

    let link_application = match apply_links(config_root.as_deref(), &result.path) {
        Ok(application) => application,
        Err(error) => {
            let rollback = switch_rollback_workspaces(&intermediate, &result);
            return Err(rollback_after(error, &rollback));
        }
    };

    if let Err(error) = workspace::record_switch(&result) {
        let error = rollback_links_after(error, link_application);
        let rollback = switch_rollback_workspaces(&intermediate, &result);
        return Err(rollback_after(error, &rollback));
    }

    Ok(SwitchOutcome {
        intermediate,
        result,
        links: link_application.map(LinkApplication::into_report),
    })
}

fn switch_rollback_workspaces(
    intermediate: &[CreatedWorkspace],
    result: &SwitchResult,
) -> Vec<CreatedWorkspace> {
    let mut rollback = intermediate.to_vec();
    if result.created {
        rollback.push(CreatedWorkspace {
            result: AddResult {
                workspace: result.workspace.clone(),
                path: result.path.clone(),
                bookmark: result.bookmark.clone(),
            },
            links: None,
        });
    }
    rollback
}

fn create_workspace(
    name: &str,
    policy: &CreationPolicy,
    allow_explicit: bool,
    inventory: &workspace::WorkspaceInventory,
    config_root: Option<&Path>,
) -> Result<CreatedWorkspace> {
    let result = workspace::add_workspace(
        inventory,
        name,
        &AddOptions {
            at_revset: policy.at_revset.clone(),
            bookmark: policy.bookmark_for(name, allow_explicit),
        },
    )
    .with_context(|| format!("failed to add workspace {name}"))?;

    match apply_links(config_root, &result.path) {
        Ok(application) => Ok(CreatedWorkspace {
            result,
            links: application.map(LinkApplication::into_report),
        }),
        Err(error) => {
            let cleanup = workspace::rollback_added_workspace(&result).err();
            if let Some(cleanup) = cleanup {
                Err(error.context(format!("workspace cleanup also failed: {cleanup}")))
            } else {
                Err(error)
            }
        }
    }
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

fn rollback_after(error: Error, created: &[CreatedWorkspace]) -> Error {
    let cleanup_errors = created
        .iter()
        .rev()
        .filter_map(|workspace| workspace::rollback_added_workspace(&workspace.result).err())
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
