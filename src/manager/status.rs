use crate::jj::{self, JjClient, WorkspaceTargetFacts};
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
/// policy. Integration then performs two read-only queries at the operation recorded by that
/// snapshot, however many workspaces exist. A broken integration query becomes an `Unknown` row
/// warning so one bad bookmark does not hide the rest of the workspace list.
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
    let observation = engine.capture_list_named(refresh)?;
    let snapshot = observation.envelope;
    let operation_id = snapshot.repository.operation_id.clone();
    let trunk_commit = snapshot.repository.trunk.commit_id.clone();

    let bookmark_targets = client.local_bookmark_targets_at(&operation_id);
    let mut revisions = BTreeSet::new();
    let mut empty_tips = BTreeSet::new();
    if let Ok(targets) = &bookmark_targets {
        for workspace in &snapshot.workspaces {
            if let Some([target]) = authoritative_bookmark(workspace)
                .and_then(|bookmark| targets.get(bookmark))
                .map(Vec::as_slice)
            {
                revisions.insert(target.clone());
            }
        }
    }

    let mut work_tips = BTreeMap::<String, WorkTip>::new();
    let mut tip_errors = BTreeMap::<String, String>::new();
    for workspace in &snapshot.workspaces {
        let Some(facts) = observation.facts.get(&workspace.name) else {
            tip_errors.insert(
                workspace.name.clone(),
                "workspace facts were missing from the frozen observation".to_owned(),
            );
            continue;
        };
        let tip = select_publish_tip(facts);
        match &tip {
            WorkTip::Commit(commit_id) => {
                revisions.insert(commit_id.clone());
            }
            WorkTip::Empty(commit_id) => {
                revisions.insert(commit_id.clone());
                empty_tips.insert(commit_id.clone());
            }
            WorkTip::Conflicted | WorkTip::Divergent => {}
        }
        work_tips.insert(workspace.name.clone(), tip);
    }

    let relations = query_relations(
        client,
        &operation_id,
        &trunk_commit,
        &revisions,
        &empty_tips,
    );
    let context = IntegrationContext {
        bookmark_targets: bookmark_targets.as_ref(),
        relations: relations.as_ref(),
    };
    let mut rows = Vec::with_capacity(snapshot.workspaces.len());
    for workspace in snapshot.workspaces.iter().cloned() {
        let mut warnings = snapshot
            .warnings
            .iter()
            .filter(|warning| warning.id == format!("refresh-failed:{}", workspace.name))
            .map(|warning| warning.message.clone())
            .collect::<Vec<_>>();
        let bookmark_integration = bookmark_integration(&context, &workspace, &mut warnings);
        if let Some(error) = tip_errors.get(&workspace.name) {
            warnings.push(error.clone());
        }
        let work_integration = work_integration(
            &context,
            &trunk_commit,
            work_tips.get(&workspace.name),
            &mut warnings,
        );
        rows.push(ManagerRow {
            workspace,
            bookmark_integration,
            work_integration,
            warnings,
        });
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
    if let Err(error) = &relations {
        for row in &mut rows {
            row.warnings
                .push(format!("could not inspect revision ancestry: {error:#}"));
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
    bookmark_targets: Result<&'a BTreeMap<String, Vec<String>>, &'a anyhow::Error>,
    relations: Result<&'a Relations, &'a anyhow::Error>,
}

fn bookmark_integration(
    context: &IntegrationContext<'_>,
    workspace: &WorkspaceSnapshot,
    warnings: &mut Vec<String>,
) -> Integration {
    let Some(bookmark) = authoritative_bookmark(workspace) else {
        return Integration::Unassociated;
    };
    let Ok(targets) = context.bookmark_targets else {
        return Integration::Unknown;
    };
    let Some(revisions) = targets.get(bookmark) else {
        warnings.push(format!("recorded bookmark `{bookmark}` is missing"));
        return Integration::Missing;
    };
    if revisions.len() > 1 {
        warnings.push(format!(
            "recorded bookmark `{bookmark}` has {} conflicting targets",
            revisions.len()
        ));
        return Integration::Conflicted;
    }
    relation_for_revision(context.relations, &revisions[0], warnings)
}

fn work_integration(
    context: &IntegrationContext<'_>,
    trunk_commit: &str,
    tip: Option<&WorkTip>,
    warnings: &mut Vec<String>,
) -> Integration {
    let Some(tip) = tip else {
        warnings.push("publish tip could not be selected".to_owned());
        return Integration::Unknown;
    };
    match tip {
        WorkTip::Conflicted => Integration::Conflicted,
        WorkTip::Divergent => Integration::Unknown,
        WorkTip::Commit(commit_id) => relation_for_revision(context.relations, commit_id, warnings),
        WorkTip::Empty(commit_id) => {
            let Ok(relations) = context.relations else {
                return Integration::Unknown;
            };
            let Some(node) = relations.get(commit_id) else {
                warnings.push(format!(
                    "empty working-copy revision `{commit_id}` was absent from the frozen ancestry query"
                ));
                return Integration::Unknown;
            };
            match node.parents.as_slice() {
                [] if commit_id == trunk_commit => Integration::InTrunk,
                [] => Integration::Missing,
                [parent] => relation_for_revision(Ok(relations), parent, warnings),
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

fn relation_for_revision(
    relations: Result<&Relations, &anyhow::Error>,
    revision: &str,
    warnings: &mut Vec<String>,
) -> Integration {
    let Ok(relations) = relations else {
        return Integration::Unknown;
    };
    let Some(node) = relations.get(revision) else {
        warnings.push(format!(
            "revision `{revision}` was absent from the frozen ancestry query"
        ));
        return Integration::Unknown;
    };
    if node.conflicted {
        return Integration::Conflicted;
    }
    if node.divergent {
        warnings.push(format!(
            "revision `{revision}` has a divergent change ID at the frozen operation"
        ));
        return Integration::Unknown;
    }
    if node.in_trunk {
        Integration::InTrunk
    } else {
        Integration::OutsideTrunk
    }
}

/// How each inspected revision relates to trunk, keyed by commit ID.
type Relations = BTreeMap<String, RevisionRelation>;

#[derive(Clone, Debug, PartialEq, Eq)]
struct RevisionRelation {
    parents: Vec<String>,
    conflicted: bool,
    divergent: bool,
    in_trunk: bool,
}

/// Reads trunk ancestry for exactly the inspected revisions, plus the parents of empty working
/// copies, at one frozen operation. JJ answers `::trunk` membership itself, so the output size
/// depends on the number of workspaces and bookmarks, never on the length of history.
fn query_relations(
    client: &JjClient,
    operation_id: &str,
    trunk_commit: &str,
    revisions: &BTreeSet<String>,
    empty_tips: &BTreeSet<String>,
) -> Result<Relations> {
    if !jj::is_commit_id(trunk_commit) {
        bail!("resolved trunk `{trunk_commit}` is not a commit ID")
    }
    if let Some(invalid) = revisions.iter().find(|id| !jj::is_commit_id(id)) {
        bail!("inspected revision `{invalid}` is not a commit ID")
    }
    let in_trunk = format!(
        "self.contained_in({})",
        serde_json::to_string(&format!("::{trunk_commit}"))?
    );
    let template = format!(
        r#"commit_id ++ "\t" ++ json(conflict) ++ "\t" ++ json(divergent) ++ "\t" ++ json({in_trunk}) ++ "\t" ++ parents.map(|parent| parent.commit_id()).join(",") ++ "\n""#
    );
    let terms = revisions
        .iter()
        .cloned()
        .chain(empty_tips.iter().map(|id| format!("parents({id})")));
    let mut relations = Relations::new();
    for revset in jj::union_revsets(terms) {
        let output = client.run_at(
            operation_id,
            [
                "log",
                "-r",
                revset.as_str(),
                "--no-graph",
                "--template",
                template.as_str(),
            ],
        )?;
        relations.extend(parse_relations(output.stdout()?)?);
    }
    Ok(relations)
}

fn parse_relations(output: &str) -> Result<Relations> {
    let mut relations = Relations::new();
    for (index, line) in output.lines().filter(|line| !line.is_empty()).enumerate() {
        let fields = line.split('\t').collect::<Vec<_>>();
        let [commit_id, conflicted, divergent, in_trunk, parents] = fields.as_slice() else {
            bail!(
                "frozen ancestry query returned malformed record {}",
                index + 1
            )
        };
        let flag = |value: &str, name: &str| -> Result<bool> {
            serde_json::from_str(value)
                .with_context(|| format!("invalid {name} flag in ancestry record {}", index + 1))
        };
        relations.insert(
            (*commit_id).to_owned(),
            RevisionRelation {
                parents: parents
                    .split(',')
                    .filter(|parent| !parent.is_empty())
                    .map(ToOwned::to_owned)
                    .collect(),
                conflicted: flag(conflicted, "conflict")?,
                divergent: flag(divergent, "divergent")?,
                in_trunk: flag(in_trunk, "trunk")?,
            },
        );
    }
    Ok(relations)
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
                external_owner: None,
                owned_git_worktree: None,
            })
            .unwrap();
    }

    #[test]
    fn relations_parse_flags_and_parent_edges() {
        let relations = parse_relations(
            "merge\tfalse\tfalse\ttrue\tleft,right\nroot\tfalse\tfalse\ttrue\t\nwork\ttrue\ttrue\tfalse\tmerge\n",
        )
        .unwrap();
        assert_eq!(relations["merge"].parents, ["left", "right"]);
        assert!(relations["root"].parents.is_empty() && relations["root"].in_trunk);
        let work = &relations["work"];
        assert!(work.conflicted && work.divergent && !work.in_trunk);
        assert!(parse_relations("work\tfalse\n").is_err());
        assert!(parse_relations("work\tmaybe\tfalse\tfalse\t\n").is_err());
    }

    #[test]
    fn ancestry_query_output_is_bounded_by_inspected_revisions_not_history() {
        let (directory, client) = repo();
        for index in 0..20 {
            commit(&client, &directory, "history", &index.to_string());
        }
        let trunk = client.resolve_one("@-").unwrap().commit_id;
        let tip = commit(&client, &directory, "work", "work");
        let operation_id = client.operation_id().unwrap();
        let relations = query_relations(
            &client,
            &operation_id,
            &trunk,
            &BTreeSet::from([tip.clone(), trunk.clone()]),
            &BTreeSet::new(),
        )
        .unwrap();
        assert_eq!(relations.len(), 2);
        assert!(relations[&trunk].in_trunk);
        assert!(!relations[&tip].in_trunk);
        assert_eq!(relations[&tip].parents, [trunk]);
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
    fn real_jj_classifies_an_exact_merge_bookmark_by_trunk_ancestry() {
        let (directory, client) = repo();
        let base = commit(&client, &directory, "base", "base");
        let left = commit(&client, &directory, "left.txt", "left");
        client.run(["new", &base]).unwrap();
        let right = commit(&client, &directory, "right.txt", "right");
        client.run(["new", &left, &right]).unwrap();
        fs::write(directory.path().join("merge.txt"), "merge").unwrap();
        client.run(["commit", "-m", "merge"]).unwrap();
        let merge = client.resolve_one("@-").unwrap().commit_id;
        let parent_count = client
            .run(["log", "-r", &merge, "--no-graph", "-T", "parents.len()"])
            .unwrap()
            .trimmed_stdout()
            .unwrap()
            .parse::<usize>()
            .unwrap();
        assert_eq!(
            parent_count, 2,
            "fixture must contain an exact merge commit"
        );

        let trunk = commit(&client, &directory, "trunk.txt", "after merge");
        client
            .run(["bookmark", "create", "published", "-r", &merge])
            .unwrap();
        // Keep lifecycle metadata outside the checkout: editing the historical merge below
        // replaces the working tree and must not erase the fixture itself.
        let metadata_directory = tempfile::tempdir().unwrap();
        let metadata = WorkspaceMetadataStore::from_repo_config_path(
            metadata_directory.path().join("repo-config"),
        )
        .unwrap();
        store(&metadata, "default", Some("published"));
        assert_eq!(metadata.list().unwrap().len(), 1);
        let engine = ObservationEngine::with_metadata_store(client.clone(), &trunk, metadata);

        client.run(["edit", &merge]).unwrap();
        let result = capture_with_engine(&client, &engine, &[]).unwrap();
        let row = &result.rows[0];
        assert_eq!(row.bookmark_integration, Integration::InTrunk, "{row:?}");
        assert_eq!(row.work_integration, Integration::InTrunk, "{row:?}");
        assert_eq!(row.workspace.commit_id, merge);

        client.run(["new", &base]).unwrap();
        let outside_left = commit(&client, &directory, "outside-left.txt", "left");
        client.run(["new", &base]).unwrap();
        let outside_right = commit(&client, &directory, "outside-right.txt", "right");
        client.run(["new", &outside_left, &outside_right]).unwrap();
        fs::write(directory.path().join("outside-merge.txt"), "merge").unwrap();
        client.run(["commit", "-m", "outside merge"]).unwrap();
        let outside_merge = client.resolve_one("@-").unwrap().commit_id;
        let parent_count = client
            .run([
                "log",
                "-r",
                &outside_merge,
                "--no-graph",
                "-T",
                "parents.len()",
            ])
            .unwrap()
            .trimmed_stdout()
            .unwrap()
            .parse::<usize>()
            .unwrap();
        assert_eq!(
            parent_count, 2,
            "fixture must contain a second merge commit"
        );
        client.run(["edit", &outside_merge]).unwrap();

        let outside = capture_with_engine(&client, &engine, &[]).unwrap();
        let row = &outside.rows[0];
        assert_eq!(row.bookmark_integration, Integration::InTrunk, "{row:?}");
        assert_eq!(row.work_integration, Integration::OutsideTrunk, "{row:?}");
        assert_eq!(row.workspace.commit_id, outside_merge);
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
        let engine =
            ObservationEngine::with_metadata_store(client.clone(), "root()", test_store(&siblings));
        let captured =
            capture_with_engine(&client, &engine, &["broken".into(), "healthy".into()]).unwrap();
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
