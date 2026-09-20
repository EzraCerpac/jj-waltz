use crate::jj::{JjClient, WorkspaceTargetFacts};
use crate::observe::ObservationEngine;
use crate::snapshot::{ManagementState, SnapshotEnvelope, WorkspaceSnapshot};
use anyhow::{Context, Result, bail};
use std::collections::{BTreeMap, BTreeSet};

/// Evidence for how one revision relates to the configured trunk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Integration {
    InTrunk,
    OutsideTrunk,
    Missing,
    Conflicted,
    Unassociated,
    Unknown,
}

impl Integration {
    pub(crate) fn label(&self) -> &'static str {
        match self {
            Self::InTrunk => "in-trunk",
            Self::OutsideTrunk => "outside-trunk",
            Self::Missing => "missing",
            Self::Conflicted => "conflicted",
            Self::Unassociated => "unassociated",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ManagerSnapshot {
    pub snapshot: SnapshotEnvelope,
    pub rows: Vec<ManagerRow>,
}

#[derive(Clone, Debug)]
pub(crate) struct ManagerRow {
    pub workspace: WorkspaceSnapshot,
    pub bookmark_integration: Integration,
    pub work_integration: Integration,
    pub warnings: Vec<String>,
}

/// Capture manager rows from one final, frozen JJ operation.
///
/// The observation engine owns inventory, metadata, trunk resolution, and the requested refresh
/// policy. Integration then performs read-only graph queries at the operation recorded by that
/// snapshot. A broken integration query becomes an `Unknown` row warning so one bad bookmark does
/// not hide the rest of the workspace list.
pub fn capture(trunk: &str, refresh: &[String]) -> anyhow::Result<ManagerSnapshot> {
    capture_with_client(&JjClient::current()?, trunk, refresh)
}

fn capture_with_client(
    client: &JjClient,
    trunk: &str,
    refresh: &[String],
) -> Result<ManagerSnapshot> {
    let engine = ObservationEngine::new(client.clone(), trunk)?;
    capture_with_engine(client, &engine, refresh)
}

fn capture_with_engine(
    client: &JjClient,
    engine: &ObservationEngine,
    refresh: &[String],
) -> Result<ManagerSnapshot> {
    let snapshot = engine.capture_list_named(refresh)?;
    let operation_id = snapshot.repository.operation_id.clone();
    let trunk_commit = snapshot.repository.trunk.commit_id.clone();

    let facts = client.workspace_target_facts_at(&operation_id).ok();
    let local_bookmarks = client.local_bookmark_names_at(&operation_id);

    let mut roots = BTreeSet::from([trunk_commit.clone()]);
    let bookmark_targets = query_bookmark_targets(client, &operation_id);
    let mut bookmark_revisions = BTreeMap::<String, Vec<String>>::new();
    let mut root_errors = BTreeMap::<String, String>::new();

    for workspace in &snapshot.workspaces {
        let Some(bookmark) = authoritative_bookmark(workspace) else {
            continue;
        };
        let Some(local_bookmarks) = local_bookmarks.as_ref().ok() else {
            continue;
        };
        if !local_bookmarks.contains(bookmark) {
            continue;
        }
        match bookmark_targets.as_ref() {
            Ok(targets) => {
                let ids = targets.get(bookmark).cloned().unwrap_or_default();
                roots.extend(ids.iter().cloned());
                bookmark_revisions.insert(workspace.name.clone(), ids);
            }
            Err(error) => {
                root_errors.insert(
                    workspace.name.clone(),
                    format!("could not resolve recorded bookmark `{bookmark}`: {error:#}"),
                );
            }
        }
    }

    let mut work_revisions = BTreeMap::<String, WorkTip>::new();
    if let Some(facts) = facts.as_ref() {
        for workspace in &snapshot.workspaces {
            let Some(fact) = facts.get(&workspace.name) else {
                root_errors.insert(
                    workspace.name.clone(),
                    "workspace facts were missing from the frozen observation".to_owned(),
                );
                continue;
            };
            let tip = select_publish_tip(fact);
            if let Some(commit_id) = tip.commit_id() {
                roots.insert(commit_id.to_owned());
            }
            work_revisions.insert(workspace.name.clone(), tip);
        }
    }

    let graph = query_graph(client, &operation_id, &roots);
    let trunk_ancestors = graph
        .as_ref()
        .ok()
        .and_then(|graph| graph.ancestors_including(&trunk_commit).ok());
    let integration_context = IntegrationContext {
        operation_id: &operation_id,
        bookmark_revisions: &bookmark_revisions,
        local_bookmarks: &local_bookmarks,
        root_errors: &root_errors,
        graph: graph.as_ref(),
        trunk_ancestors: trunk_ancestors.as_ref(),
    };
    let mut rows = Vec::with_capacity(snapshot.workspaces.len());
    for workspace in snapshot.workspaces.iter().cloned() {
        let mut warnings = snapshot
            .warnings
            .iter()
            .filter(|warning| warning.id == format!("refresh-failed:{}", workspace.name))
            .map(|warning| warning.message.clone())
            .collect::<Vec<_>>();
        let bookmark_integration =
            bookmark_integration(&integration_context, &workspace, &mut warnings);
        let work_integration = work_integration(
            &trunk_commit,
            work_revisions.get(&workspace.name),
            graph.as_ref(),
            trunk_ancestors.as_ref(),
            &mut warnings,
        );
        rows.push(ManagerRow {
            workspace,
            bookmark_integration,
            work_integration,
            warnings,
        });
    }

    if let Err(error) = &local_bookmarks {
        for row in &mut rows {
            if authoritative_bookmark(&row.workspace).is_some() {
                row.warnings.push(format!(
                    "could not inspect local bookmarks at the frozen operation: {error:#}"
                ));
            }
        }
    }
    if let Err(error) = &bookmark_targets {
        for row in &mut rows {
            if authoritative_bookmark(&row.workspace).is_some() {
                row.warnings.push(format!(
                    "could not inspect bookmark targets at the frozen operation: {error:#}"
                ));
            }
        }
    }
    if let Some(error) = graph.as_ref().err() {
        for row in &mut rows {
            row.warnings
                .push(format!("could not inspect revision ancestry: {error:#}"));
        }
    }
    if facts.is_none() {
        for row in &mut rows {
            row.warnings
                .push("could not inspect workspace facts at the frozen operation".to_owned());
        }
    }

    Ok(ManagerSnapshot { snapshot, rows })
}

fn authoritative_bookmark(workspace: &WorkspaceSnapshot) -> Option<&str> {
    (workspace.management == ManagementState::Managed)
        .then_some(workspace.associated_bookmark.as_deref())
        .flatten()
}

struct IntegrationContext<'a> {
    operation_id: &'a str,
    bookmark_revisions: &'a BTreeMap<String, Vec<String>>,
    local_bookmarks: &'a Result<BTreeSet<String>>,
    root_errors: &'a BTreeMap<String, String>,
    graph: Result<&'a RevisionGraph, &'a anyhow::Error>,
    trunk_ancestors: Option<&'a BTreeSet<String>>,
}

fn bookmark_integration(
    context: &IntegrationContext<'_>,
    workspace: &WorkspaceSnapshot,
    warnings: &mut Vec<String>,
) -> Integration {
    let Some(bookmark) = authoritative_bookmark(workspace) else {
        return Integration::Unassociated;
    };
    if let Some(error) = context.root_errors.get(&workspace.name) {
        warnings.push(error.clone());
        return Integration::Unknown;
    }
    let Ok(local_bookmarks) = context.local_bookmarks else {
        return Integration::Unknown;
    };
    if !local_bookmarks.contains(bookmark) {
        warnings.push(format!("recorded bookmark `{bookmark}` is missing"));
        return Integration::Missing;
    }
    let Some(revisions) = context.bookmark_revisions.get(&workspace.name) else {
        warnings.push(format!(
            "recorded bookmark `{bookmark}` could not be resolved at operation {}",
            context.operation_id
        ));
        return Integration::Unknown;
    };
    if revisions.is_empty() {
        warnings.push(format!("recorded bookmark `{bookmark}` has no target"));
        return Integration::Missing;
    }
    if revisions.len() > 1 {
        warnings.push(format!(
            "recorded bookmark `{bookmark}` has {} conflicting targets",
            revisions.len()
        ));
        return Integration::Conflicted;
    }
    relation_for_revision(
        context.graph,
        context.trunk_ancestors,
        &revisions[0],
        warnings,
    )
}

fn work_integration(
    trunk_commit: &str,
    tip: Option<&WorkTip>,
    graph: Result<&RevisionGraph, &anyhow::Error>,
    trunk_ancestors: Option<&BTreeSet<String>>,
    warnings: &mut Vec<String>,
) -> Integration {
    let Some(tip) = tip else {
        warnings.push("publish tip could not be selected".to_owned());
        return Integration::Unknown;
    };
    match tip {
        WorkTip::Conflicted => Integration::Conflicted,
        WorkTip::Divergent => Integration::Unknown,
        WorkTip::Commit(commit_id) => {
            relation_for_revision(graph, trunk_ancestors, commit_id, warnings)
        }
        WorkTip::Empty(commit_id) => {
            let Ok(graph) = graph else {
                return Integration::Unknown;
            };
            let Some(node) = graph.nodes.get(commit_id) else {
                warnings.push(format!(
                    "empty working-copy revision `{commit_id}` was absent from the frozen graph"
                ));
                return Integration::Unknown;
            };
            match node.parents.as_slice() {
                [] if commit_id == trunk_commit => Integration::InTrunk,
                [] => Integration::Missing,
                [parent] => relation_for_revision(Ok(graph), trunk_ancestors, parent, warnings),
                parents => {
                    warnings.push(format!(
                        "empty working-copy revision `{commit_id}` has {} parents; publish tip is ambiguous",
                        parents.len()
                    ));
                    Integration::Unknown
                }
            }
        }
    }
}

fn select_publish_tip(facts: &WorkspaceTargetFacts) -> WorkTip {
    if facts.conflicted {
        return WorkTip::Conflicted;
    }
    if facts.divergent {
        return WorkTip::Divergent;
    }
    if !facts.empty {
        return WorkTip::Commit(facts.commit_id.clone());
    }
    WorkTip::Empty(facts.commit_id.clone())
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum WorkTip {
    Commit(String),
    Empty(String),
    Conflicted,
    Divergent,
}

impl WorkTip {
    fn commit_id(&self) -> Option<&str> {
        match self {
            Self::Commit(id) | Self::Empty(id) => Some(id),
            Self::Conflicted | Self::Divergent => None,
        }
    }
}

fn relation_for_revision(
    graph: Result<&RevisionGraph, &anyhow::Error>,
    trunk_ancestors: Option<&BTreeSet<String>>,
    revision: &str,
    warnings: &mut Vec<String>,
) -> Integration {
    let Ok(graph) = graph else {
        return Integration::Unknown;
    };
    if trunk_ancestors.is_some_and(|ancestors| ancestors.contains(revision))
        && is_zero_commit_id(revision)
    {
        return Integration::InTrunk;
    }
    let Some(node) = graph.nodes.get(revision) else {
        warnings.push(format!(
            "revision `{revision}` was absent from the frozen graph"
        ));
        return Integration::Unknown;
    };
    if node.conflicted {
        return Integration::Conflicted;
    }
    if node.parents.len() > 1 {
        warnings.push(format!(
            "revision `{revision}` has {} parents; ancestry is ambiguous",
            node.parents.len()
        ));
        return Integration::Unknown;
    }
    if graph
        .change_counts
        .get(&node.change_id)
        .copied()
        .unwrap_or(0)
        > 1
    {
        warnings.push(format!(
            "revision `{revision}` has a divergent change ID in the frozen graph"
        ));
        return Integration::Unknown;
    }
    match trunk_ancestors {
        Some(trunk_ancestors) if trunk_ancestors.contains(revision) => Integration::InTrunk,
        Some(_) => Integration::OutsideTrunk,
        None => {
            warnings.push("could not derive trunk ancestry from the frozen graph".to_owned());
            Integration::Unknown
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RevisionNode {
    change_id: String,
    parents: Vec<String>,
    conflicted: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RevisionGraph {
    nodes: BTreeMap<String, RevisionNode>,
    change_counts: BTreeMap<String, usize>,
}

impl RevisionGraph {
    fn ancestors_including(&self, start: &str) -> Result<BTreeSet<String>> {
        let mut result = BTreeSet::new();
        let mut pending = vec![start.to_owned()];
        while let Some(commit_id) = pending.pop() {
            if !result.insert(commit_id.clone()) {
                continue;
            }
            if is_zero_commit_id(&commit_id) {
                continue;
            }
            let Some(node) = self.nodes.get(&commit_id) else {
                bail!("revision `{commit_id}` was absent from the frozen graph")
            };
            pending.extend(node.parents.iter().cloned());
        }
        Ok(result)
    }
}

fn query_graph(
    client: &JjClient,
    operation_id: &str,
    roots: &BTreeSet<String>,
) -> Result<RevisionGraph> {
    let roots = roots
        .iter()
        .filter(|id| !is_zero_commit_id(id))
        .map(|id| format!("::{id}"))
        .collect::<Vec<_>>();
    if roots.is_empty() {
        bail!("frozen ancestry query had no non-empty revisions")
    }
    let revset = roots.join(" | ");
    let output = client.run_at(
        operation_id,
        [
            "log",
            "-r",
            revset.as_str(),
            "--no-graph",
            "--template",
            r#"commit_id ++ "\t" ++ change_id ++ "\t" ++ json(conflict) ++ "\t" ++ parents.map(|parent| parent.commit_id()).join(",") ++ "\n""#,
        ],
    )?;
    parse_graph(output.stdout()?)
}

fn parse_graph(output: &str) -> Result<RevisionGraph> {
    let mut nodes = BTreeMap::new();
    let mut change_counts = BTreeMap::new();
    for (index, line) in output.lines().filter(|line| !line.is_empty()).enumerate() {
        let fields = line.split('\t').collect::<Vec<_>>();
        let [commit_id, change_id, conflicted, parents] = fields.as_slice() else {
            bail!(
                "frozen ancestry query returned malformed record {}",
                index + 1
            )
        };
        let conflicted: bool = serde_json::from_str(conflicted)
            .with_context(|| format!("invalid conflict flag in graph record {}", index + 1))?;
        let parents = parents
            .split(',')
            .filter(|parent| !parent.is_empty())
            .map(ToOwned::to_owned)
            .collect::<Vec<_>>();
        if nodes
            .insert(
                (*commit_id).to_owned(),
                RevisionNode {
                    change_id: (*change_id).to_owned(),
                    parents,
                    conflicted,
                },
            )
            .is_some()
        {
            bail!("frozen ancestry query returned duplicate revision `{commit_id}`")
        }
        *change_counts.entry((*change_id).to_owned()).or_default() += 1;
    }
    if nodes.is_empty() {
        bail!("frozen ancestry query returned no revisions")
    }
    Ok(RevisionGraph {
        nodes,
        change_counts,
    })
}

fn is_zero_commit_id(commit_id: &str) -> bool {
    !commit_id.is_empty() && commit_id.bytes().all(|byte| byte == b'0')
}

/// Reads every local bookmark target in one frozen query. `bookmarks` is the local bookmark
/// collection exposed by JJ's commit template, so no bookmark name is treated as a revset.
fn query_bookmark_targets(
    client: &JjClient,
    operation_id: &str,
) -> Result<BTreeMap<String, Vec<String>>> {
    let output = client.run_at(
        operation_id,
        [
            "log",
            "-r",
            "bookmarks()",
            "--no-graph",
            "--template",
            r#"commit_id ++ "\t" ++ bookmarks.map(|bookmark| json(bookmark.name())).join("\t") ++ "\n""#,
        ],
    )?;
    let mut targets = BTreeMap::<String, Vec<String>>::new();
    for (index, line) in output
        .stdout()?
        .lines()
        .filter(|line| !line.is_empty())
        .enumerate()
    {
        let mut fields = line.split('\t');
        let Some(commit_id) = fields.next() else {
            bail!(
                "bookmark target query returned malformed record {}",
                index + 1
            )
        };
        for bookmark in fields {
            let bookmark: String = serde_json::from_str(bookmark)
                .with_context(|| format!("invalid bookmark name in target record {}", index + 1))?;
            targets
                .entry(bookmark)
                .or_default()
                .push(commit_id.to_owned());
        }
    }
    Ok(targets)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::{ManagedWorkspaceMetadata, WorkspaceMetadataStore};
    use std::fs;
    use tempfile::TempDir;

    fn repo() -> (TempDir, JjClient) {
        let directory = tempfile::tempdir().unwrap();
        let client = JjClient::new(directory.path());
        client.run(["git", "init"]).unwrap();
        (directory, client)
    }

    fn commit(client: &JjClient, directory: &TempDir, name: &str, contents: &str) -> String {
        fs::write(directory.path().join(name), contents).unwrap();
        client.run(["commit", "-m", name]).unwrap();
        client.resolve_one("@-").unwrap().commit_id
    }

    fn test_store(directory: &TempDir) -> WorkspaceMetadataStore {
        WorkspaceMetadataStore::from_repo_config_path(directory.path().join("repo-config")).unwrap()
    }

    fn store(store: &WorkspaceMetadataStore, workspace: &str, bookmark: Option<&str>) {
        store
            .insert(&ManagedWorkspaceMetadata {
                workspace_name: workspace.to_owned(),
                created_at_unix_ms: 1,
                creation_operation_id: "operation".to_owned(),
                creation_base_commit_id: "base".to_owned(),
                associated_bookmark: bookmark.map(ToOwned::to_owned),
                intended_remote: None,
            })
            .unwrap();
    }

    #[test]
    fn graph_parses_parent_edges_and_zero_root() {
        let graph = parse_graph(
            "trunk\ttrunk-change\tfalse\tparent\nparent\tparent-change\tfalse\t0000000000000000000000000000000000000000\n",
        )
        .unwrap();
        assert_eq!(
            graph.ancestors_including("trunk").unwrap(),
            BTreeSet::from([
                "trunk".to_owned(),
                "parent".to_owned(),
                "0000000000000000000000000000000000000000".to_owned(),
            ])
        );
    }

    #[test]
    fn real_jj_marks_newer_work_outside_trunk_and_bookmark_in_trunk() {
        let (directory, client) = repo();
        let base = commit(&client, &directory, "base", "base");
        client
            .run(["bookmark", "create", "published", "-r", &base])
            .unwrap();
        let work = commit(&client, &directory, "work", "work");
        let metadata = test_store(&directory);
        store(&metadata, "default", Some("published"));
        let engine = ObservationEngine::with_metadata_store(client.clone(), &base, metadata);

        let result = capture_with_engine(&client, &engine, &[]).unwrap();
        let row = &result.rows[0];
        assert_eq!(row.bookmark_integration, Integration::InTrunk, "{row:?}");
        assert_eq!(row.work_integration, Integration::OutsideTrunk);
        assert_eq!(
            row.workspace.commit_id,
            client.resolve_one("@").unwrap().commit_id
        );
        assert_ne!(work, base);
    }

    #[test]
    fn real_jj_uses_the_sole_parent_of_an_empty_working_copy() {
        let (directory, client) = repo();
        let base = commit(&client, &directory, "base", "base");
        let metadata = test_store(&directory);
        store(&metadata, "default", None);
        let engine = ObservationEngine::with_metadata_store(client.clone(), &base, metadata);

        let result = capture_with_engine(&client, &engine, &[]).unwrap();
        let row = &result.rows[0];
        assert_eq!(row.work_integration, Integration::InTrunk, "{row:?}");
        assert_eq!(row.bookmark_integration, Integration::Unassociated);
    }

    #[test]
    fn unmanaged_work_is_unassociated_and_missing_bookmark_is_missing() {
        let (directory, client) = repo();
        let base = commit(&client, &directory, "base", "base");
        let metadata = test_store(&directory);
        let engine =
            ObservationEngine::with_metadata_store(client.clone(), &base, metadata.clone());
        let result = capture_with_engine(&client, &engine, &[]).unwrap();
        assert_eq!(
            result.rows[0].bookmark_integration,
            Integration::Unassociated
        );

        store(&metadata, "default", Some("does-not-exist"));
        let engine = ObservationEngine::with_metadata_store(client.clone(), &base, metadata);
        let result = capture_with_engine(&client, &engine, &[]).unwrap();
        assert_eq!(result.rows[0].bookmark_integration, Integration::Missing);
    }

    #[test]
    fn integration_labels_are_stable_machine_labels() {
        assert_eq!(Integration::InTrunk.label(), "in-trunk");
        assert_eq!(Integration::OutsideTrunk.label(), "outside-trunk");
        assert_eq!(Integration::Missing.label(), "missing");
        assert_eq!(Integration::Conflicted.label(), "conflicted");
        assert_eq!(Integration::Unassociated.label(), "unassociated");
        assert_eq!(Integration::Unknown.label(), "unknown");
    }

    #[test]
    fn real_jj_root_working_copy_is_already_in_root_trunk() {
        let (directory, client) = repo();
        let trunk = client.resolve_one("root()").unwrap().commit_id;
        let metadata = test_store(&directory);
        let engine = ObservationEngine::with_metadata_store(client.clone(), &trunk, metadata);

        let result = capture_with_engine(&client, &engine, &[]).unwrap();
        assert_eq!(
            result.rows[0].work_integration,
            Integration::InTrunk,
            "{:?}",
            result.rows[0]
        );
    }

    #[test]
    fn real_jj_empty_merge_publish_tip_is_unknown() {
        let (directory, client) = repo();
        let base = commit(&client, &directory, "base", "base");
        let left = commit(&client, &directory, "left", "left");
        client.run(["new", &base]).unwrap();
        let right = commit(&client, &directory, "right", "right");
        client.run(["new", &left, &right]).unwrap();

        let metadata = test_store(&directory);
        let engine = ObservationEngine::with_metadata_store(client.clone(), &base, metadata);
        let result = capture_with_engine(&client, &engine, &[]).unwrap();
        assert_eq!(result.rows[0].work_integration, Integration::Unknown);
        assert!(
            result.rows[0]
                .warnings
                .iter()
                .any(|warning| warning.contains("publish tip is ambiguous"))
        );
    }

    #[test]
    fn real_jj_conflicted_working_copy_is_conflicted() {
        let (directory, client) = repo();
        let base = commit(&client, &directory, "shared", "base");
        let left = commit(&client, &directory, "shared", "left");
        client.run(["new", &base]).unwrap();
        let right = commit(&client, &directory, "shared", "right");
        client.run(["new", &left, &right]).unwrap();

        let metadata = test_store(&directory);
        let engine = ObservationEngine::with_metadata_store(client.clone(), &base, metadata);
        let result = capture_with_engine(&client, &engine, &[]).unwrap();
        assert_eq!(result.rows[0].work_integration, Integration::Conflicted);
    }

    #[test]
    fn failed_named_refresh_reports_the_error_without_hiding_other_rows() {
        let (_directory, client) = repo();
        let siblings = tempfile::tempdir().unwrap();
        for name in ["broken", "healthy"] {
            client
                .run([
                    "workspace",
                    "add",
                    "--name",
                    name,
                    "-r",
                    "root()",
                    siblings.path().join(name).to_str().unwrap(),
                ])
                .unwrap();
        }
        fs::write(
            siblings.path().join("broken/.jj/working_copy/tree_state"),
            b"invalid state",
        )
        .unwrap();
        let captured =
            capture_with_client(&client, "root()", &["broken".into(), "healthy".into()]).unwrap();
        let broken = captured
            .rows
            .iter()
            .find(|row| row.workspace.name == "broken")
            .unwrap();
        let healthy = captured
            .rows
            .iter()
            .find(|row| row.workspace.name == "healthy")
            .unwrap();
        assert!(!broken.workspace.working_copy_refreshed);
        assert!(
            broken
                .warnings
                .iter()
                .any(|warning| warning.contains("failed to refresh workspace `broken`"))
        );
        assert!(healthy.workspace.working_copy_refreshed);
    }
}
