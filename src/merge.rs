use crate::config::Config;
use crate::jj::JjClient;
use crate::trash::{self, Classifier, Verdict};
use crate::unpreserved::{self, Unpreserved};
use crate::workspace::{self, RemovalResult, WorkspaceInventory};
use anyhow::{Context, Result, bail};
use std::env;
use std::path::{Path, PathBuf};

/// Lines of unpreserved files listed before the refusal is cut short.
const UNPRESERVED_LIST_LIMIT: usize = 10;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeRequest {
    /// Workspace token to land; `None` selects the current workspace.
    pub workspace: Option<String>,
    /// Local bookmark to land on and advance; `None` inserts the work before the receiving
    /// workspace's working copy instead.
    pub into: Option<String>,
    pub squash: bool,
    pub push: bool,
    /// Forget the workspace but leave its directory, skipping the unpreserved-file check.
    pub keep_dir: bool,
    /// Delete the directory without the unpreserved-file check, losing its untracked files.
    pub force: bool,
}

#[derive(Debug)]
pub struct MergeOutcome {
    pub workspace: String,
    /// Where the work landed: `bookmark NAME` or `workspace NAME`.
    pub target: String,
    /// Revisions newly landed; zero when the work was already there.
    pub landed: usize,
    pub squashed: bool,
    pub pushed: bool,
    pub removal: RemovalResult,
    /// Where the caller's shell belongs once the merged workspace is gone.
    pub destination: PathBuf,
    /// Associated bookmarks kept instead of deleted, with the reason.
    pub warnings: Vec<String>,
}

/// Where a merge lands the workspace's stack.
enum Landing {
    /// Rebase onto a local bookmark, then fast-forward it.
    Bookmark { name: String },
    /// Insert between a workspace's working-copy commit and its parents, moving no bookmark.
    Workspace {
        name: String,
        client: JjClient,
        working_copy: String,
    },
}

/// Land a workspace's stack, then remove the workspace.
///
/// The stack is every revision reachable from the workspace's publish tip but not from the
/// landing base. By default it is inserted before the receiving workspace's working copy: the
/// current workspace's, or the default workspace's when merging the current one. With a
/// bookmark it is rebased onto that bookmark, which is fast-forwarded. Removal is planned
/// before any mutation. It deletes the merged workspace's associated bookmark only when that
/// bookmark points into the landed stack, and never trunk or the `--into` target. Unless the
/// directory is kept or `force` is set, merging refuses before any mutation when the
/// directory holds files JJ does not track that the default workspace has no identical copy
/// of. A failure before
/// removal leaves the workspace in place, and rerunning is safe: an already landed stack is
/// empty.
pub fn merge_workspace(request: &MergeRequest) -> Result<MergeOutcome> {
    let inventory = WorkspaceInventory::load()?;
    let name = match &request.workspace {
        Some(token) => inventory.resolve(token)?,
        None => inventory.current_name().to_owned(),
    };
    let default_name = inventory.default_name()?.to_owned();
    if name == default_name {
        bail!("refusing to merge the default workspace")
    }
    let default_root = inventory.root(&default_name)?;
    let merging_current = name == inventory.current_name();
    let receiver = if merging_current {
        default_name
    } else {
        inventory.current_name().to_owned()
    };
    let source_root = inventory.root(&name)?;
    let receiver_root = inventory.root(&receiver)?;
    let current_dir = env::current_dir().context("failed to determine current directory")?;

    // Each rewrite runs in the workspace whose working copy it moves, so JJ keeps it current.
    let source = JjClient::new(&source_root);
    let tracked = source
        .snapshot_tracked_files()
        .with_context(|| format!("failed to snapshot workspace {name}"))?;
    if !request.keep_dir && !request.force {
        refuse_unpreserved_files(&source, &tracked, &name, &source_root, &default_root)?;
    }

    // Merging the current workspace moves the caller to the default workspace, which also
    // keeps this process out of the directory removal deletes. Planning first surfaces
    // ownership and policy refusals before any revision is rewritten.
    let destination = if merging_current {
        let destination = current_dir
            .strip_prefix(&source_root)
            .ok()
            .map(|relative| receiver_root.join(relative))
            .filter(|path| path.is_dir())
            .unwrap_or_else(|| receiver_root.clone());
        env::set_current_dir(&receiver_root).with_context(|| {
            format!(
                "failed to enter default workspace at {}",
                receiver_root.display()
            )
        })?;
        destination
    } else {
        current_dir
    };
    let mut removal = workspace::plan_remove_workspace(
        &WorkspaceInventory::load()?,
        Some(&name),
        !request.keep_dir,
    )?;

    let landing = match &request.into {
        Some(bookmark) => Landing::Bookmark {
            name: bookmark.clone(),
        },
        None => {
            let client = JjClient::new(&receiver_root);
            client
                .run(["status"])
                .with_context(|| format!("failed to snapshot workspace {receiver}"))?;
            let facts = client
                .workspace_target_facts_at(client.operation_id()?)?
                .remove(&receiver)
                .with_context(|| format!("JJ workspace query did not return `{receiver}`"))?;
            if facts.conflicted || facts.divergent {
                bail!(
                    "workspace {receiver} has a conflicted or divergent working copy; resolve it before merging into it"
                )
            }
            Landing::Workspace {
                name: receiver,
                client,
                working_copy: facts.change_id,
            }
        }
    };

    let operation_id = source.operation_id()?;
    let base = match &landing {
        Landing::Bookmark { name } => {
            if !source
                .local_bookmark_names_at(&operation_id)?
                .contains(name)
            {
                bail!("local bookmark does not exist: {name}")
            }
            if source
                .conflicted_bookmark_names_at(&operation_id)?
                .contains(name)
            {
                bail!("bookmark {name} is conflicted; resolve it with `jj bookmark` first")
            }
            bookmark_revset(name)
        }
        // Pin the parents by commit ID: inserting the stack rewrites the working copy.
        Landing::Workspace { working_copy, .. } => source
            .resolve_all_at(&operation_id, format!("{working_copy}-"))?
            .iter()
            .map(|parent| parent.commit_id.as_str())
            .collect::<Vec<_>>()
            .join(" | "),
    };

    let mut tip = publish_tip(&source, &operation_id, &name)?;
    let stack_revset = |tip: &str| format!("({base})..({tip})");
    let stack = source.resolve_all_at(&operation_id, stack_revset(&tip))?;
    if !source
        .resolve_all_at(&operation_id, format!("merges() & {}", stack_revset(&tip)))?
        .is_empty()
    {
        bail!(
            "workspace {name} contains merge commits; linearize it with `jj rebase` before merging"
        )
    }
    if !source
        .resolve_all_at(
            &operation_id,
            format!("conflicts() & {}", stack_revset(&tip)),
        )?
        .is_empty()
    {
        bail!("workspace {name} contains conflicted revisions; resolve them before merging")
    }

    // Decide before any rewrite which associated bookmarks belong to the work being landed.
    let into = match &landing {
        Landing::Bookmark { name } => Some(name.as_str()),
        Landing::Workspace { .. } => None,
    };
    let (deletable, warnings) = classify_associated_bookmarks(
        &source,
        &operation_id,
        &removal.bookmarks,
        &stack_revset(&tip),
        into,
    )?;
    removal.bookmarks = deletable;

    let squashed = request.squash && stack.len() > 1;
    if squashed {
        let root =
            source.resolve_one_at(&operation_id, format!("roots({})", stack_revset(&tip)))?;
        // Unlike `describe`, `squash --message` keeps the text verbatim, so end it like
        // every other JJ description.
        let message = source
            .descriptions_at(&operation_id, stack_revset(&tip))?
            .iter()
            .map(|description| format!("{}\n", description.trim_end()))
            .filter(|description| description != "\n")
            .collect::<Vec<_>>()
            .join("\n");
        let sources = format!("{} ~ {}", stack_revset(&tip), root.commit_id);
        source
            .run([
                "squash",
                "--from",
                &sources,
                "--into",
                &root.commit_id,
                "--message",
                &message,
            ])
            .with_context(|| format!("failed to squash workspace {name}"))?;
        tip = root.change_id;
    }

    let target = match &landing {
        Landing::Bookmark { name } => format!("bookmark {name}"),
        Landing::Workspace { name, .. } => format!("workspace {name}"),
    };
    if !stack.is_empty() {
        let landed = match &landing {
            Landing::Bookmark { name: bookmark } => {
                source
                    .run(["rebase", "-b", &tip, "-d", &bookmark_revset(bookmark)])
                    .with_context(|| format!("failed to rebase workspace {name} onto {target}"))?;
                stack_revset(&tip)
            }
            Landing::Workspace {
                client,
                working_copy,
                ..
            } => {
                client
                    .run(["rebase", "-r", &stack_revset(&tip), "-B", working_copy])
                    .with_context(|| format!("failed to insert workspace {name} into {target}"))?;
                // Inserting the stack also moved the merged workspace's working copy.
                source
                    .run(["workspace", "update-stale"])
                    .with_context(|| format!("failed to update workspace {name}"))?;
                format!("{} | {working_copy}", stack_revset(&tip))
            }
        };
        let conflicted =
            source.resolve_all_at(source.operation_id()?, format!("conflicts() & ({landed})"))?;
        if !conflicted.is_empty() {
            bail!(
                "landing on {target} left {} conflicted revision(s); resolve them and rerun `jw merge`, or undo with `jj op restore {operation_id}`",
                conflicted.len()
            )
        }
        if let Landing::Bookmark { name: bookmark } = &landing {
            source
                .run(["bookmark", "move", bookmark, "--to", &tip])
                .with_context(|| format!("failed to move bookmark {bookmark}"))?;
        }
    }

    if let (true, Landing::Bookmark { name: bookmark }) = (request.push, &landing) {
        source
            .run(["git", "push", "--bookmark", bookmark])
            .with_context(|| {
                format!(
                    "{bookmark} was updated locally, but pushing it failed; workspace {name} was kept, rerun `jw merge --push` to retry"
                )
            })?;
    }

    let removal = workspace::execute_remove_workspace(removal, true)?;

    Ok(MergeOutcome {
        workspace: name,
        target,
        landed: if squashed { 1 } else { stack.len() },
        squashed,
        pushed: request.push,
        removal,
        destination,
        warnings,
    })
}

/// Refuse to delete a workspace directory that holds untracked files only it has.
///
/// Tracked files are preserved in the working-copy commit after the snapshot. An
/// untracked file survives deletion only when a `# jw:trash` section of an ignore file
/// marks it regenerable, or an ignore file adopted by `# jw:trash-rules` decides it, or
/// when the default workspace has an identical copy.
fn refuse_unpreserved_files(
    source: &JjClient,
    tracked: &[PathBuf],
    name: &str,
    source_root: &Path,
    default_root: &Path,
) -> Result<()> {
    let check = || {
        let mut classifier = Classifier::new(source.git_root()?, source_root.to_path_buf());
        unpreserved::unpreserved_files(source_root, tracked, default_root, |paths| {
            classifier.classify(paths)
        })
    };
    let entries = check().with_context(|| {
        format!("failed to check workspace {name} for untracked files; pass --keep-dir to keep its directory, or --force to delete it anyway")
    })?;
    if entries.is_empty() {
        return Ok(());
    }
    let files = entries.iter().map(|entry| entry.files.len()).sum::<usize>();
    bail!(
        "workspace {name} has {files} untracked file(s) that no `{marker}` ignore rule covers and that the default workspace has no identical copy of; removing the workspace would delete them:\n{}\nIf they are regenerable, move the patterns shown into a `{marker}` section of their ignore file (it ends at the next blank line).{} Otherwise pass --keep-dir to keep the directory, pass --force to delete these files with it, or delete or move them and rerun `jw merge`",
        unpreserved::summarize(&entries, UNPRESERVED_LIST_LIMIT),
        adoption_hint(&entries, tracked),
        marker = trash::TRASH_MARKER,
    )
}

/// Suggest adopting the nested ignore files that decided `entries` and that JJ does not
/// track. A tool may write such a file, and rewrite it, so it cannot keep a marker.
fn adoption_hint(entries: &[Unpreserved], tracked: &[PathBuf]) -> String {
    let mut sources = entries
        .iter()
        .filter_map(|entry| match &entry.verdict {
            Verdict::Ignored(rule) => Some(rule.source.as_path()),
            _ => None,
        })
        .filter(|source| {
            trash::adoptable_directory(source).is_some()
                && !tracked.iter().any(|file| file == source)
        })
        .collect::<Vec<_>>();
    sources.sort();
    sources.dedup();
    if sources.is_empty() {
        return String::new();
    }
    let lines = sources
        .iter()
        .map(|source| {
            // Ignore files use `/` on every platform.
            let path = source
                .components()
                .map(|part| part.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            format!("`{} {path}`", trash::ADOPT_MARKER)
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        " An untracked ignore file may be written by a tool that would overwrite a marker; if all of its rules are regenerable, add {lines} to the root .gitignore instead."
    )
}

/// Split associated bookmarks into those safe to delete and warnings for the rest.
///
/// A bookmark is deleted only when it points into the landed stack. The `--into` target,
/// bookmarks on trunk, and bookmarks pointing anywhere else, such as one wrongly associated
/// through `jw repair`, are kept.
fn classify_associated_bookmarks(
    source: &JjClient,
    operation_id: &str,
    bookmarks: &[String],
    stack: &str,
    into: Option<&str>,
) -> Result<(Vec<String>, Vec<String>)> {
    if bookmarks.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    let existing = source.local_bookmark_names_at(operation_id)?;
    let trunk = Config::load()?.trunk.revset;
    let mut deletable = Vec::new();
    let mut warnings = Vec::new();
    for bookmark in bookmarks {
        if !existing.contains(bookmark) {
            continue;
        }
        let points_into = |revset: &str| -> Result<bool> {
            Ok(!source
                .resolve_all_at(
                    operation_id,
                    format!("{} & ({revset})", bookmark_revset(bookmark)),
                )?
                .is_empty())
        };
        let reason = if into == Some(bookmark.as_str()) {
            Some("it is the merge target")
        } else if points_into(&trunk)
            .with_context(|| format!("failed to evaluate trunk revset {trunk:?}"))?
        {
            Some("it points at trunk")
        } else if !points_into(stack)? {
            Some("it points outside the work being landed")
        } else {
            None
        };
        match reason {
            Some(reason) => warnings.push(format!("kept associated bookmark {bookmark}: {reason}")),
            None => deletable.push(bookmark.clone()),
        }
    }
    Ok((deletable, warnings))
}

/// Select the revision holding the workspace's work: a non-empty `@`, otherwise its sole parent.
fn publish_tip(client: &JjClient, operation_id: &str, name: &str) -> Result<String> {
    let facts = client
        .workspace_target_facts_at(operation_id)?
        .remove(name)
        .with_context(|| format!("JJ workspace query did not return `{name}`"))?;
    if facts.conflicted {
        bail!("workspace {name} has a conflicted working copy; resolve it before merging")
    }
    if facts.divergent {
        bail!("workspace {name} has a divergent working copy; resolve it before merging")
    }
    if !facts.empty {
        return Ok(facts.change_id);
    }
    let parents = client.resolve_all_at(operation_id, format!("{}-", facts.commit_id))?;
    match parents.as_slice() {
        [parent] => Ok(parent.change_id.clone()),
        _ => bail!(
            "workspace {name} has an empty working copy with {} parents; select the work to land with `jj new` first",
            parents.len()
        ),
    }
}

/// Local bookmarks `jw merge --into` can land on, excluding conflicted ones.
pub fn completion_target_candidates() -> Result<Vec<(String, String)>> {
    let client = JjClient::current()?;
    let operation_id = client.operation_id()?;
    let conflicted = client.conflicted_bookmark_names_at(&operation_id)?;
    Ok(client
        .local_bookmark_names_at(&operation_id)?
        .into_iter()
        .filter(|name| !conflicted.contains(name))
        .map(|name| (name, "Local bookmark".to_owned()))
        .collect())
}

fn bookmark_revset(name: &str) -> String {
    let literal = serde_json::to_string(name).expect("serialize bookmark name");
    format!("bookmarks(exact:{literal})")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bookmark_revsets_quote_names_exactly() {
        assert_eq!(bookmark_revset("main"), r#"bookmarks(exact:"main")"#);
        assert_eq!(
            bookmark_revset(r#"we"ird\name"#),
            r#"bookmarks(exact:"we\"ird\\name")"#
        );
    }
}
