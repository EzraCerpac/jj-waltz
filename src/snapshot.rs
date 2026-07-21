use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const SNAPSHOT_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotEnvelope {
    pub schema_version: u32,
    pub command: SnapshotCommand,
    pub repository: RepositorySnapshot,
    pub workspaces: Vec<WorkspaceSnapshot>,
    pub warnings: Vec<SnapshotWarning>,
}

impl SnapshotEnvelope {
    pub fn new(
        command: SnapshotCommand,
        repository: RepositorySnapshot,
        workspaces: Vec<WorkspaceSnapshot>,
        warnings: Vec<SnapshotWarning>,
    ) -> Self {
        Self {
            schema_version: SNAPSHOT_SCHEMA_VERSION,
            command,
            repository,
            workspaces,
            warnings,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SnapshotCommand {
    List,
    Status,
    Doctor,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositorySnapshot {
    pub captured_at_unix_ms: u64,
    pub repository_id: String,
    pub operation_id: String,
    pub trunk: ResolvedTrunk,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedTrunk {
    pub revset: String,
    pub change_id: String,
    pub commit_id: String,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceSnapshot {
    pub name: String,
    pub path: Option<PathBuf>,
    pub role: WorkspaceRole,
    pub management: ManagementState,
    pub working_copy: WorkingCopyStatus,
    pub working_copy_refreshed: bool,
    pub change_id: String,
    pub commit_id: String,
    pub description: String,
    pub associated_bookmark: Option<String>,
    pub created_at_unix_ms: Option<u64>,
    pub creation_operation_id: Option<String>,
    pub creation_base_commit_id: Option<String>,
    pub intended_remote: Option<String>,
    pub hazards: Vec<Hazard>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceRole {
    pub current: bool,
    pub previous: bool,
    pub default: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ManagementState {
    Managed,
    Unmanaged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "kebab-case")]
pub enum WorkingCopyStatus {
    Empty,
    Modified {
        files: u32,
        added: u32,
        removed: u32,
    },
    Conflicted {
        conflicts: u32,
    },
    Stale,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hazard {
    pub id: HazardId,
    pub message: String,
}

impl Hazard {
    pub fn new(id: HazardId, message: impl Into<String>) -> Self {
        Self {
            id,
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HazardId {
    StaleWorkingCopy,
    Conflicted,
    DivergentChange,
    AmbiguousPublishTip,
    UnmanagedWorkspace,
    MissingCreationBase,
    TrunkUnresolved,
    TrunkMultipleRevisions,
    StackMultipleHeads,
    StackCrossesExternalMerge,
    SharedRevisionRewrite,
    BookmarkConflict,
    RemoteBehind,
    RemoteDiverged,
    UnpublishedWork,
    HookFailed,
    ForgeUnavailable,
    MissingWorkspacePath,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotWarning {
    pub id: String,
    pub message: String,
}

impl SnapshotWarning {
    pub fn new(id: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            message: message.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn repository() -> RepositorySnapshot {
        RepositorySnapshot {
            captured_at_unix_ms: 1_753_088_400_000,
            repository_id: "repo-1".to_owned(),
            operation_id: "op-1".to_owned(),
            trunk: ResolvedTrunk {
                revset: "trunk()".to_owned(),
                change_id: "trunk-change".to_owned(),
                commit_id: "trunk-commit".to_owned(),
                description: "main line".to_owned(),
            },
        }
    }

    fn workspace(working_copy: WorkingCopyStatus) -> WorkspaceSnapshot {
        WorkspaceSnapshot {
            name: "solver".to_owned(),
            path: Some(PathBuf::from("/repo/.workspaces/solver")),
            role: WorkspaceRole {
                current: true,
                previous: false,
                default: true,
            },
            management: ManagementState::Managed,
            working_copy,
            working_copy_refreshed: true,
            change_id: "solver-change".to_owned(),
            commit_id: "solver-commit".to_owned(),
            description: "Improve solver".to_owned(),
            associated_bookmark: Some("wip/solver".to_owned()),
            created_at_unix_ms: Some(1_750_000_000_123),
            creation_operation_id: Some("operation-1".to_owned()),
            creation_base_commit_id: Some("base-commit".to_owned()),
            intended_remote: Some("origin".to_owned()),
            hazards: Vec::new(),
        }
    }

    #[test]
    fn list_json_uses_versioned_envelope() {
        let envelope = SnapshotEnvelope::new(
            SnapshotCommand::List,
            repository(),
            vec![workspace(WorkingCopyStatus::Empty)],
            vec![SnapshotWarning::new(
                "refresh-skipped",
                "working copy was not refreshed",
            )],
        );

        let value = serde_json::to_value(&envelope).unwrap();
        assert_eq!(value["schema_version"], SNAPSHOT_SCHEMA_VERSION);
        assert_eq!(value["command"], "list");
        assert_eq!(value["repository"]["operation_id"], "op-1");
        assert_eq!(value["repository"]["trunk"]["revset"], "trunk()");
        assert_eq!(value["workspaces"][0]["management"], "managed");
        assert_eq!(
            value["workspaces"][0]["role"],
            json!({ "current": true, "previous": false, "default": true })
        );
        assert_eq!(value["workspaces"][0]["path"], "/repo/.workspaces/solver");
        assert_eq!(value["workspaces"][0]["working_copy_refreshed"], true);
        assert_eq!(
            value["workspaces"][0]["working_copy"],
            json!({ "state": "empty" })
        );
        assert_eq!(value["warnings"][0]["id"], "refresh-skipped");
    }

    #[test]
    fn status_json_keeps_fields_and_kebab_case_enum_values() {
        let mut entry = workspace(WorkingCopyStatus::Modified {
            files: 3,
            added: 18,
            removed: 4,
        });
        entry.role = WorkspaceRole {
            current: false,
            previous: true,
            default: false,
        };
        entry.management = ManagementState::Unmanaged;
        entry.path = None;
        entry.working_copy_refreshed = false;
        entry.associated_bookmark = None;
        entry.created_at_unix_ms = None;
        entry.creation_operation_id = None;
        entry.creation_base_commit_id = None;
        entry.intended_remote = None;
        entry.hazards.push(Hazard::new(
            HazardId::TrunkMultipleRevisions,
            "trunk revset resolved to multiple revisions",
        ));

        let envelope = SnapshotEnvelope::new(
            SnapshotCommand::Status,
            repository(),
            vec![entry],
            Vec::new(),
        );
        let value = serde_json::to_value(&envelope).unwrap();
        let entry = &value["workspaces"][0];

        assert_eq!(value["command"], "status");
        assert_eq!(
            entry["role"],
            json!({ "current": false, "previous": true, "default": false })
        );
        assert_eq!(entry["management"], "unmanaged");
        assert_eq!(entry["path"], Value::Null);
        assert_eq!(entry["working_copy_refreshed"], false);
        assert_eq!(
            entry["working_copy"],
            json!({ "state": "modified", "files": 3, "added": 18, "removed": 4 })
        );
        assert_eq!(entry["associated_bookmark"], Value::Null);
        assert_eq!(entry["created_at_unix_ms"], Value::Null);
        assert_eq!(entry["creation_operation_id"], Value::Null);
        assert_eq!(entry["creation_base_commit_id"], Value::Null);
        assert_eq!(entry["intended_remote"], Value::Null);
        assert_eq!(entry["hazards"][0]["id"], "trunk-multiple-revisions");

        let decoded: SnapshotEnvelope = serde_json::from_value(value).unwrap();
        assert_eq!(decoded, envelope);
    }

    #[test]
    fn conflicted_working_copy_is_internally_tagged() {
        assert_eq!(
            serde_json::to_value(WorkingCopyStatus::Conflicted { conflicts: 2 }).unwrap(),
            json!({ "state": "conflicted", "conflicts": 2 })
        );
        assert_eq!(
            serde_json::to_value(SnapshotCommand::Doctor).unwrap(),
            json!("doctor")
        );
    }
}
