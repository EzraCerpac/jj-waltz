use crate::config::{self, Config};
use crate::links::{self, LinkApplyReport};
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
    let mut created = Vec::new();
    for name in names {
        let resolved = workspace::resolve_workspace_token(name)?;
        match create_workspace(&resolved, policy, true) {
            Ok(outcome) => created.push(outcome),
            Err(error) => return Err(rollback_after(error, &created)),
        }
    }
    Ok(created)
}

pub fn switch_workspaces(names: &[String], policy: &CreationPolicy) -> Result<SwitchOutcome> {
    let (final_name, intermediate_names) = names
        .split_last()
        .ok_or_else(|| anyhow::anyhow!("at least one workspace name is required"))?;
    let mut intermediate = Vec::new();

    for name in intermediate_names {
        let resolved = workspace::resolve_workspace_token(name)?;
        if workspace::workspace_exists(&resolved)? {
            continue;
        }
        match create_workspace(&resolved, policy, false) {
            Ok(outcome) => intermediate.push(outcome),
            Err(error) => return Err(rollback_after(error, &intermediate)),
        }
    }

    let resolved_final = workspace::resolve_workspace_token(final_name)?;
    let result = match workspace::switch_workspace(
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

    let links = match apply_links(&result.path, policy.apply_links) {
        Ok(report) => report,
        Err(error) => {
            let mut rollback = intermediate.clone();
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
            return Err(rollback_after(error, &rollback));
        }
    };

    if let Err(error) = workspace::record_switch(&result) {
        let mut rollback = intermediate.clone();
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
        return Err(rollback_after(error, &rollback));
    }

    Ok(SwitchOutcome {
        intermediate,
        result,
        links,
    })
}

fn create_workspace(
    name: &str,
    policy: &CreationPolicy,
    allow_explicit: bool,
) -> Result<CreatedWorkspace> {
    let result = workspace::add_workspace(
        name,
        &AddOptions {
            at_revset: policy.at_revset.clone(),
            bookmark: policy.bookmark_for(name, allow_explicit),
        },
    )
    .with_context(|| format!("failed to add workspace {name}"))?;

    match apply_links(&result.path, policy.apply_links) {
        Ok(links) => Ok(CreatedWorkspace { result, links }),
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

fn apply_links(path: &Path, enabled: bool) -> Result<Option<LinkApplyReport>> {
    if !enabled {
        return Ok(None);
    }
    let config_root = workspace::default_workspace_root().unwrap_or_else(|_| path.to_path_buf());
    links::apply_workspace_links_with_config_root(&config_root, path).map(Some)
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
