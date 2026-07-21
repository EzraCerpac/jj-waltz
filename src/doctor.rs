use crate::jj::{JjClient, MINIMUM_SUPPORTED_JJ_VERSION};
use crate::metadata::{ManagedWorkspaceMetadata, WorkspaceMetadataStore};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

pub const DOCTOR_SCHEMA_VERSION: u32 = 1;

/// Repository diagnostics that never change the JJ operation or working copy. Every repository
/// query is pinned to one operation and ignores the working copy. JJ may initialize its empty
/// secure-config directory while returning the documented repository config path.
#[derive(Debug, Clone)]
pub struct DoctorEngine {
    client: JjClient,
    trunk_revset: String,
    repository_config_path: Option<PathBuf>,
}

impl DoctorEngine {
    pub fn current(trunk_revset: impl Into<String>) -> Result<Self> {
        Ok(Self::new(JjClient::current()?, trunk_revset))
    }

    pub fn new(client: JjClient, trunk_revset: impl Into<String>) -> Self {
        Self {
            client,
            trunk_revset: trunk_revset.into(),
            repository_config_path: None,
        }
    }

    /// Overrides metadata discovery. Useful when the caller already captured
    /// the repository config path and for hermetic tests.
    pub fn with_repository_config_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.repository_config_path = Some(path.into());
        self
    }

    /// Builds a report even when individual checks fail.
    pub fn run(&self) -> DoctorReport {
        let mut report = DoctorReport::new(self.trunk_revset.clone());

        self.check_jj_version(&mut report);
        let operation_id = self.check_operation_snapshot(&mut report);
        let trunk = self.check_trunk(&mut report, operation_id.as_deref());
        report.repository.trunk = trunk;

        let metadata = self.check_metadata(&mut report);
        let workspaces = self.check_workspace_paths(&mut report, operation_id.as_deref());
        self.check_metadata_consistency(
            &mut report,
            operation_id.as_deref(),
            metadata.as_deref(),
            workspaces.as_deref(),
        );
        self.check_bookmarks_and_divergence(
            &mut report,
            operation_id.as_deref(),
            workspaces.as_deref(),
        );

        report.push(DoctorDiagnostic::skipped(
            DoctorCode::WorkingCopyStale,
            "working-copy freshness requires evaluating each checkout and was not probed globally",
            Some("run `jw status <workspace>` to inspect a specific working copy"),
        ));
        report.push(DoctorDiagnostic::skipped(
            DoctorCode::ShellIntegration,
            "shell integration depends on the caller's active shell and was not probed",
            Some("run `jw shell init <shell>` in the shell that uses jj-waltz"),
        ));
        report.finish()
    }

    fn check_jj_version(&self, report: &mut DoctorReport) {
        match self.client.capabilities() {
            Ok(capabilities) if capabilities.is_supported() => {
                report.repository.jj_version = Some(capabilities.version.to_string());
                report.push(DoctorDiagnostic::passed(
                    DoctorCode::JjVersion,
                    format!(
                        "JJ {} supports frozen workspace diagnostics",
                        capabilities.version
                    ),
                ));
            }
            Ok(capabilities) => {
                report.repository.jj_version = Some(capabilities.version.to_string());
                report.push(DoctorDiagnostic::error(
                    DoctorCode::JjVersion,
                    format!(
                        "JJ {} is older than supported minimum {}",
                        capabilities.version, MINIMUM_SUPPORTED_JJ_VERSION
                    ),
                    Some(format!(
                        "upgrade JJ to {} or newer",
                        MINIMUM_SUPPORTED_JJ_VERSION
                    )),
                ));
            }
            Err(error) => report.push(DoctorDiagnostic::error(
                DoctorCode::JjVersion,
                format!("could not determine JJ capabilities: {error:#}"),
                Some("install a supported JJ binary and ensure it is on PATH"),
            )),
        }
    }

    fn check_operation_snapshot(&self, report: &mut DoctorReport) -> Option<String> {
        match self.client.operation_id() {
            Ok(operation_id) => {
                report.repository.operation_id = Some(operation_id.clone());
                report.push(DoctorDiagnostic::passed(
                    DoctorCode::OperationSnapshot,
                    format!("captured repository operation {operation_id}"),
                ));
                Some(operation_id)
            }
            Err(error) => {
                report.push(DoctorDiagnostic::error(
                    DoctorCode::OperationSnapshot,
                    format!("could not capture repository operation: {error:#}"),
                    Some("run doctor from inside a readable JJ workspace"),
                ));
                None
            }
        }
    }

    fn check_trunk(
        &self,
        report: &mut DoctorReport,
        operation_id: Option<&str>,
    ) -> Option<DoctorRevision> {
        if self.trunk_revset.trim().is_empty() {
            report.push(DoctorDiagnostic::error(
                DoctorCode::TrunkRevset,
                "configured trunk revset is blank",
                Some("set `[trunk].revset = \"trunk()\"` or another exact-one revset"),
            ));
            return None;
        }
        let Some(operation_id) = operation_id else {
            report.push(DoctorDiagnostic::skipped(
                DoctorCode::TrunkRevset,
                "trunk revset was not evaluated because operation capture failed",
                None::<String>,
            ));
            return None;
        };

        match query_revisions(&self.client, operation_id, &self.trunk_revset) {
            Ok(mut revisions) if revisions.len() == 1 => {
                let revision = revisions.pop().expect("length checked above");
                report.push(DoctorDiagnostic::passed(
                    DoctorCode::TrunkRevset,
                    format!(
                        "trunk revset `{}` resolved to {}",
                        self.trunk_revset, revision.commit_id
                    ),
                ));
                Some(revision)
            }
            Ok(revisions) if revisions.is_empty() => {
                report.push(DoctorDiagnostic::error(
                    DoctorCode::TrunkRevset,
                    format!(
                        "trunk revset `{}` resolved to no revisions",
                        self.trunk_revset
                    ),
                    Some("configure a revset that resolves to exactly one revision"),
                ));
                None
            }
            Ok(revisions) => {
                report.push(DoctorDiagnostic::error(
                    DoctorCode::TrunkRevset,
                    format!(
                        "trunk revset `{}` resolved to {} revisions",
                        self.trunk_revset,
                        revisions.len()
                    ),
                    Some("narrow the configured revset to exactly one revision"),
                ));
                None
            }
            Err(error) => {
                report.push(DoctorDiagnostic::error(
                    DoctorCode::TrunkRevset,
                    format!(
                        "could not evaluate trunk revset `{}`: {error:#}",
                        self.trunk_revset
                    ),
                    Some("fix `[trunk].revset` and rerun doctor"),
                ));
                None
            }
        }
    }

    fn check_metadata(&self, report: &mut DoctorReport) -> Option<Vec<ManagedWorkspaceMetadata>> {
        let config_path = match &self.repository_config_path {
            Some(path) => Ok(path.clone()),
            None => self.client.repo_config_path(),
        };
        let store = match config_path.and_then(WorkspaceMetadataStore::from_repo_config_path) {
            Ok(store) => store,
            Err(error) => {
                report.push(DoctorDiagnostic::error(
                    DoctorCode::MetadataIntegrity,
                    format!("could not locate workspace metadata: {error:#}"),
                    Some("check repository config permissions and JJ secure-config access"),
                ));
                return None;
            }
        };

        report.repository.repository_id = Some(store.repository_id().to_owned());
        match store.list() {
            Ok(records) => {
                report.push(DoctorDiagnostic::passed(
                    DoctorCode::MetadataIntegrity,
                    format!("workspace metadata is readable ({} records)", records.len()),
                ));
                Some(records)
            }
            Err(error) => {
                report.push(DoctorDiagnostic::error(
                    DoctorCode::MetadataIntegrity,
                    format!("workspace metadata is corrupt or unreadable: {error:#}"),
                    Some(
                        "repair or restore the reported metadata file; doctor will not replace it",
                    ),
                ));
                None
            }
        }
    }

    fn check_workspace_paths(
        &self,
        report: &mut DoctorReport,
        operation_id: Option<&str>,
    ) -> Option<Vec<DoctorWorkspace>> {
        let Some(operation_id) = operation_id else {
            report.push(DoctorDiagnostic::skipped(
                DoctorCode::WorkspacePath,
                "workspace paths were not checked because operation capture failed",
                None::<String>,
            ));
            return None;
        };
        let mut workspaces = match query_workspaces(&self.client, operation_id) {
            Ok(workspaces) => workspaces,
            Err(error) => {
                report.push(DoctorDiagnostic::error(
                    DoctorCode::WorkspacePath,
                    format!("could not list workspaces: {error:#}"),
                    Some("check JJ repository and workspace-store readability"),
                ));
                return None;
            }
        };

        let mut problems = 0;
        for workspace in &mut workspaces {
            match query_workspace_root(&self.client, operation_id, &workspace.name) {
                Ok(path) => {
                    workspace.path = Some(path.clone());
                    match validate_workspace_path(&path) {
                        Ok(()) => {}
                        Err(error) => {
                            problems += 1;
                            report.push(
                                DoctorDiagnostic::error(
                                    DoctorCode::WorkspacePath,
                                    format!("workspace path is unusable: {error:#}"),
                                    Some(
                                        "restore the checkout or run `jw prune` if it was removed",
                                    ),
                                )
                                .with_subject(&workspace.name),
                            );
                        }
                    }
                }
                Err(error) => {
                    problems += 1;
                    report.push(
                        DoctorDiagnostic::error(
                            DoctorCode::WorkspacePath,
                            format!("workspace path is missing or unreadable: {error:#}"),
                            Some("restore the checkout or run `jw prune` if it was removed"),
                        )
                        .with_subject(&workspace.name),
                    );
                }
            }
        }
        if problems == 0 {
            report.push(DoctorDiagnostic::passed(
                DoctorCode::WorkspacePath,
                format!("all {} workspace paths are usable", workspaces.len()),
            ));
        }
        Some(workspaces)
    }

    fn check_metadata_consistency(
        &self,
        report: &mut DoctorReport,
        operation_id: Option<&str>,
        metadata: Option<&[ManagedWorkspaceMetadata]>,
        workspaces: Option<&[DoctorWorkspace]>,
    ) {
        let (Some(operation_id), Some(metadata), Some(workspaces)) =
            (operation_id, metadata, workspaces)
        else {
            report.push(DoctorDiagnostic::skipped(
                DoctorCode::MetadataConsistency,
                "metadata/JJ consistency could not be checked because an earlier probe failed",
                None::<String>,
            ));
            return;
        };

        let workspace_names = workspaces
            .iter()
            .map(|workspace| workspace.name.as_str())
            .collect::<BTreeSet<_>>();
        let bookmarks = query_bookmark_names(&self.client, operation_id);
        let mut problems = 0;

        for record in metadata {
            if !workspace_names.contains(record.workspace_name.as_str()) {
                problems += 1;
                report.push(
                    DoctorDiagnostic::error(
                        DoctorCode::MetadataConsistency,
                        "managed metadata has no matching JJ workspace",
                        Some("remove stale metadata only after confirming the workspace is gone"),
                    )
                    .with_subject(&record.workspace_name),
                );
            }

            match query_revisions(&self.client, operation_id, &record.creation_base_commit_id) {
                Ok(revisions) if revisions.len() == 1 => {}
                Ok(revisions) => {
                    problems += 1;
                    report.push(
                        DoctorDiagnostic::error(
                            DoctorCode::MetadataConsistency,
                            format!(
                                "creation base resolves to {} revisions; expected one",
                                revisions.len()
                            ),
                            Some("adopt the workspace again with a valid exact base"),
                        )
                        .with_subject(&record.workspace_name),
                    );
                }
                Err(error) => {
                    problems += 1;
                    report.push(
                        DoctorDiagnostic::error(
                            DoctorCode::MetadataConsistency,
                            format!("creation base cannot be resolved: {error:#}"),
                            Some("adopt the workspace again with a valid exact base"),
                        )
                        .with_subject(&record.workspace_name),
                    );
                }
            }

            if let Some(bookmark) = &record.associated_bookmark {
                match &bookmarks {
                    Ok(names) if names.contains(bookmark) => {}
                    Ok(_) => {
                        problems += 1;
                        report.push(
                            DoctorDiagnostic::error(
                                DoctorCode::MetadataConsistency,
                                format!("associated bookmark `{bookmark}` does not exist"),
                                Some("recreate the bookmark or adopt without an association"),
                            )
                            .with_subject(&record.workspace_name),
                        );
                    }
                    Err(error) => {
                        problems += 1;
                        report.push(
                            DoctorDiagnostic::error(
                                DoctorCode::MetadataConsistency,
                                format!("could not verify associated bookmark: {error:#}"),
                                Some("check bookmark readability and rerun doctor"),
                            )
                            .with_subject(&record.workspace_name),
                        );
                    }
                }
            }
        }

        if problems == 0 {
            report.push(DoctorDiagnostic::passed(
                DoctorCode::MetadataConsistency,
                format!(
                    "{} managed workspace records match JJ state",
                    metadata.len()
                ),
            ));
        }
    }

    fn check_bookmarks_and_divergence(
        &self,
        report: &mut DoctorReport,
        operation_id: Option<&str>,
        workspaces: Option<&[DoctorWorkspace]>,
    ) {
        let Some(operation_id) = operation_id else {
            report.push(DoctorDiagnostic::skipped(
                DoctorCode::BookmarkConflict,
                "bookmark conflicts were not checked because operation capture failed",
                None::<String>,
            ));
            report.push(DoctorDiagnostic::skipped(
                DoctorCode::DivergentChange,
                "workspace divergence was not checked because operation capture failed",
                None::<String>,
            ));
            return;
        };

        match query_conflicted_bookmarks(&self.client, operation_id) {
            Ok(bookmarks) if bookmarks.is_empty() => report.push(DoctorDiagnostic::passed(
                DoctorCode::BookmarkConflict,
                "no conflicted bookmarks found",
            )),
            Ok(bookmarks) => {
                for bookmark in bookmarks {
                    report.push(
                        DoctorDiagnostic::error(
                            DoctorCode::BookmarkConflict,
                            "bookmark has conflicting targets",
                            Some("resolve bookmark targets before publishing"),
                        )
                        .with_subject(bookmark),
                    );
                }
            }
            Err(error) => report.push(DoctorDiagnostic::error(
                DoctorCode::BookmarkConflict,
                format!("could not inspect bookmark conflicts: {error:#}"),
                Some("check bookmark readability and rerun doctor"),
            )),
        }

        let Some(workspaces) = workspaces else {
            report.push(DoctorDiagnostic::skipped(
                DoctorCode::DivergentChange,
                "workspace divergence was not checked because workspace listing failed",
                None::<String>,
            ));
            return;
        };
        let divergent = workspaces
            .iter()
            .filter(|workspace| workspace.divergent)
            .collect::<Vec<_>>();
        if divergent.is_empty() {
            report.push(DoctorDiagnostic::passed(
                DoctorCode::DivergentChange,
                "no workspace targets use divergent change IDs",
            ));
        } else {
            for workspace in divergent {
                report.push(
                    DoctorDiagnostic::error(
                        DoctorCode::DivergentChange,
                        format!(
                            "workspace target {} has a divergent change ID",
                            workspace.commit_id
                        ),
                        Some("merge or abandon the unintended divergent commit"),
                    )
                    .with_subject(&workspace.name),
                );
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DoctorReport {
    pub schema_version: u32,
    pub command: DoctorCommand,
    pub repository: DoctorRepository,
    pub diagnostics: Vec<DoctorDiagnostic>,
    pub summary: DoctorSummary,
    pub healthy: bool,
}

impl DoctorReport {
    fn new(trunk_revset: String) -> Self {
        Self {
            schema_version: DOCTOR_SCHEMA_VERSION,
            command: DoctorCommand::Doctor,
            repository: DoctorRepository {
                jj_version: None,
                repository_id: None,
                operation_id: None,
                trunk_revset,
                trunk: None,
            },
            diagnostics: Vec::new(),
            summary: DoctorSummary::default(),
            healthy: false,
        }
    }

    fn push(&mut self, diagnostic: DoctorDiagnostic) {
        self.diagnostics.push(diagnostic);
    }

    fn finish(mut self) -> Self {
        self.summary = DoctorSummary::from_diagnostics(&self.diagnostics);
        self.healthy = self.summary.errors == 0;
        self
    }

    pub fn has_errors(&self) -> bool {
        self.summary.errors != 0
    }

    /// Deterministic human rendering for CLI output.
    pub fn render_plain(&self) -> String {
        let mut output = format!(
            "doctor: {} ({} errors, {} warnings, {} skipped)\n",
            if self.healthy { "healthy" } else { "unhealthy" },
            self.summary.errors,
            self.summary.warnings,
            self.summary.skipped
        );
        for diagnostic in &self.diagnostics {
            let subject = diagnostic
                .subject
                .as_deref()
                .map(|subject| format!(" [{subject}]"))
                .unwrap_or_default();
            output.push_str(&format!(
                "{} {}{}: {}\n",
                diagnostic.state.label(),
                diagnostic.code.label(),
                subject,
                diagnostic.message
            ));
            if let Some(remedy) = &diagnostic.remedy {
                output.push_str(&format!("  remedy: {remedy}\n"));
            }
        }
        output
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DoctorCommand {
    Doctor,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DoctorRepository {
    pub jj_version: Option<String>,
    pub repository_id: Option<String>,
    pub operation_id: Option<String>,
    pub trunk_revset: String,
    pub trunk: Option<DoctorRevision>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DoctorRevision {
    pub change_id: String,
    pub commit_id: String,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DoctorDiagnostic {
    pub code: DoctorCode,
    pub state: DoctorState,
    pub severity: DoctorSeverity,
    pub subject: Option<String>,
    pub message: String,
    pub remedy: Option<String>,
}

impl DoctorDiagnostic {
    fn passed(code: DoctorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            state: DoctorState::Passed,
            severity: DoctorSeverity::Info,
            subject: None,
            message: message.into(),
            remedy: None,
        }
    }

    fn error(
        code: DoctorCode,
        message: impl Into<String>,
        remedy: Option<impl Into<String>>,
    ) -> Self {
        Self {
            code,
            state: DoctorState::Failed,
            severity: DoctorSeverity::Error,
            subject: None,
            message: message.into(),
            remedy: remedy.map(Into::into),
        }
    }

    fn skipped(
        code: DoctorCode,
        message: impl Into<String>,
        remedy: Option<impl Into<String>>,
    ) -> Self {
        Self {
            code,
            state: DoctorState::Skipped,
            severity: DoctorSeverity::Info,
            subject: None,
            message: message.into(),
            remedy: remedy.map(Into::into),
        }
    }

    fn with_subject(mut self, subject: impl Into<String>) -> Self {
        self.subject = Some(subject.into());
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DoctorCode {
    JjVersion,
    OperationSnapshot,
    TrunkRevset,
    MetadataIntegrity,
    WorkspacePath,
    MetadataConsistency,
    BookmarkConflict,
    DivergentChange,
    WorkingCopyStale,
    ShellIntegration,
}

impl DoctorCode {
    fn label(self) -> &'static str {
        match self {
            Self::JjVersion => "jj-version",
            Self::OperationSnapshot => "operation-snapshot",
            Self::TrunkRevset => "trunk-revset",
            Self::MetadataIntegrity => "metadata-integrity",
            Self::WorkspacePath => "workspace-path",
            Self::MetadataConsistency => "metadata-consistency",
            Self::BookmarkConflict => "bookmark-conflict",
            Self::DivergentChange => "divergent-change",
            Self::WorkingCopyStale => "working-copy-stale",
            Self::ShellIntegration => "shell-integration",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DoctorState {
    Passed,
    Failed,
    Skipped,
}

impl DoctorState {
    fn label(self) -> &'static str {
        match self {
            Self::Passed => "PASS",
            Self::Failed => "FAIL",
            Self::Skipped => "SKIP",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DoctorSeverity {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DoctorSummary {
    pub passed: u32,
    pub failed: u32,
    pub skipped: u32,
    pub warnings: u32,
    pub errors: u32,
}

impl DoctorSummary {
    fn from_diagnostics(diagnostics: &[DoctorDiagnostic]) -> Self {
        let mut summary = Self::default();
        for diagnostic in diagnostics {
            match diagnostic.state {
                DoctorState::Passed => summary.passed += 1,
                DoctorState::Failed => summary.failed += 1,
                DoctorState::Skipped => summary.skipped += 1,
            }
            match diagnostic.severity {
                DoctorSeverity::Info => {}
                DoctorSeverity::Warning => summary.warnings += 1,
                DoctorSeverity::Error => summary.errors += 1,
            }
        }
        summary
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DoctorWorkspace {
    name: String,
    commit_id: String,
    divergent: bool,
    path: Option<PathBuf>,
}

fn query_revisions(
    client: &JjClient,
    operation_id: &str,
    revset: &str,
) -> Result<Vec<DoctorRevision>> {
    let output = client.run_at(
        operation_id,
        [
            "log",
            "-r",
            revset,
            "--no-graph",
            "--template",
            "change_id ++ \"\\0\" ++ commit_id ++ \"\\0\" ++ description.first_line() ++ \"\\0\"",
        ],
    )?;
    let fields = output.stdout()?.split_terminator('\0').collect::<Vec<_>>();
    if fields.len() % 3 != 0 {
        bail!("JJ revision query returned malformed template output")
    }
    Ok(fields
        .chunks_exact(3)
        .map(|fields| DoctorRevision {
            change_id: fields[0].to_owned(),
            commit_id: fields[1].to_owned(),
            description: fields[2].to_owned(),
        })
        .collect())
}

fn query_workspaces(client: &JjClient, operation_id: &str) -> Result<Vec<DoctorWorkspace>> {
    let output = client.run_at(
        operation_id,
        [
            "workspace",
            "list",
            "--template",
            "json(name) ++ \"\\t\" ++ json(target.commit_id()) ++ \"\\t\" ++ json(target.divergent()) ++ \"\\n\"",
        ],
    )?;
    let mut workspaces = output
        .stdout()?
        .lines()
        .filter(|line| !line.is_empty())
        .map(parse_workspace_row)
        .collect::<Result<Vec<_>>>()?;
    workspaces.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(workspaces)
}

fn parse_workspace_row(line: &str) -> Result<DoctorWorkspace> {
    let fields = line.split('\t').collect::<Vec<_>>();
    let [name, commit_id, divergent] = fields.as_slice() else {
        bail!("JJ workspace query returned malformed template output")
    };
    Ok(DoctorWorkspace {
        name: serde_json::from_str(name).context("workspace name was not valid JSON")?,
        commit_id: serde_json::from_str(commit_id)
            .context("workspace commit ID was not valid JSON")?,
        divergent: serde_json::from_str(divergent)
            .context("workspace divergence flag was not valid JSON")?,
        path: None,
    })
}

fn query_workspace_root(client: &JjClient, operation_id: &str, name: &str) -> Result<PathBuf> {
    let output = client.run_at_unchecked(
        operation_id,
        ["workspace", "root", "--name", name],
    )?;
    if !output.success() {
        let message = output.stderr();
        bail!(if message.is_empty() {
            "workspace root lookup failed".to_owned()
        } else {
            message
        })
    }
    let path = output.trimmed_stdout()?;
    if path.is_empty() {
        bail!("workspace root lookup returned an empty path")
    }
    Ok(PathBuf::from(path))
}

fn validate_workspace_path(path: &Path) -> Result<()> {
    let metadata =
        fs::metadata(path).with_context(|| format!("failed to inspect {}", path.display()))?;
    if !metadata.is_dir() {
        bail!("{} is not a directory", path.display())
    }
    let jj_path = path.join(".jj");
    let jj_metadata = fs::metadata(&jj_path)
        .with_context(|| format!("failed to inspect {}", jj_path.display()))?;
    if !jj_metadata.is_dir() {
        bail!("{} is not a directory", jj_path.display())
    }
    Ok(())
}

fn query_bookmark_names(client: &JjClient, operation_id: &str) -> Result<BTreeSet<String>> {
    let output = client.run_at(
        operation_id,
        [
            "bookmark",
            "list",
            "--template",
            "if(remote, \"\", json(name) ++ \"\\n\")",
        ],
    )?;
    parse_json_lines(output.stdout()?)
}

fn query_conflicted_bookmarks(client: &JjClient, operation_id: &str) -> Result<Vec<String>> {
    let output = client.run_at(
        operation_id,
        [
            "bookmark",
            "list",
            "--conflicted",
            "--template",
            "json(name) ++ \"\\n\"",
        ],
    )?;
    Ok(parse_json_lines(output.stdout()?)?.into_iter().collect())
}

fn parse_json_lines(output: &str) -> Result<BTreeSet<String>> {
    output
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_str(line).context("JJ name template returned invalid JSON"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use tempfile::TempDir;

    struct RepoFixture {
        _tempdir: TempDir,
        root: PathBuf,
        repo: PathBuf,
        config_path: PathBuf,
    }

    impl RepoFixture {
        fn init() -> Option<Self> {
            if Command::new("jj").arg("--version").output().is_err() {
                return None;
            }
            let tempdir = tempfile::tempdir().expect("create fixture directory");
            let root = tempdir.path().to_owned();
            let repo = root.join("repo");
            let output = Command::new("jj")
                .args(["git", "init"])
                .arg(&repo)
                .output()
                .expect("initialize JJ repository");
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let config_directory = root.join("config");
            fs::create_dir_all(&config_directory).expect("create config directory");
            Some(Self {
                _tempdir: tempdir,
                root,
                repo,
                config_path: config_directory.join("config.toml"),
            })
        }

        fn client(&self) -> JjClient {
            JjClient::new(&self.repo)
        }

        fn doctor(&self, trunk_revset: &str) -> DoctorReport {
            DoctorEngine::new(self.client(), trunk_revset)
                .with_repository_config_path(&self.config_path)
                .run()
        }

        fn metadata_store(&self) -> WorkspaceMetadataStore {
            WorkspaceMetadataStore::from_repo_config_path(&self.config_path)
                .expect("open fixture metadata")
        }
    }

    fn diagnostics_for(report: &DoctorReport, code: DoctorCode) -> Vec<&DoctorDiagnostic> {
        report
            .diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.code == code)
            .collect()
    }

    #[test]
    fn healthy_repo_is_serializable_and_rendered_deterministically() {
        let Some(fixture) = RepoFixture::init() else {
            return;
        };
        let before = fixture.client().operation_id().expect("operation before");
        let report = fixture.doctor("trunk()");
        let after = fixture.client().operation_id().expect("operation after");

        assert!(report.healthy, "{}", report.render_plain());
        assert!(!report.has_errors());
        assert_eq!(
            before, after,
            "doctor must not create a repository operation"
        );
        assert_eq!(report.repository.trunk_revset, "trunk()");
        assert!(report.repository.trunk.is_some());
        assert_eq!(report.summary.skipped, 2);
        let json = serde_json::to_value(&report).expect("serialize report");
        assert_eq!(json["schema_version"], DOCTOR_SCHEMA_VERSION);
        assert_eq!(json["command"], "doctor");
        assert!(report.render_plain().starts_with("doctor: healthy"));
    }

    #[test]
    fn trunk_requires_nonblank_exact_one_revset() {
        let Some(fixture) = RepoFixture::init() else {
            return;
        };
        for revset in ["", "none()", "all()"] {
            let report = fixture.doctor(revset);
            assert!(report.has_errors());
            let diagnostics = diagnostics_for(&report, DoctorCode::TrunkRevset);
            assert!(
                diagnostics
                    .iter()
                    .any(|diagnostic| diagnostic.state == DoctorState::Failed),
                "{}",
                report.render_plain()
            );
            assert!(report.repository.trunk.is_none());
        }
    }

    #[test]
    fn corrupt_metadata_is_reported_without_replacement() {
        let Some(fixture) = RepoFixture::init() else {
            return;
        };
        let store = fixture.metadata_store();
        let base = fixture
            .client()
            .resolve_one("trunk()")
            .expect("resolve fixture trunk");
        store
            .upsert(&ManagedWorkspaceMetadata {
                workspace_name: "default".to_owned(),
                created_at_unix_ms: 1,
                creation_operation_id: fixture.client().operation_id().expect("fixture operation"),
                creation_base_commit_id: base.commit_id,
                associated_bookmark: None,
                intended_remote: None,
            })
            .expect("write metadata");
        let record = fs::read_dir(store.root().join("workspaces"))
            .expect("list metadata records")
            .next()
            .expect("metadata record")
            .expect("read metadata entry")
            .path();
        fs::write(&record, b"{not-json").expect("corrupt metadata record");

        let report = fixture.doctor("trunk()");
        assert!(report.has_errors());
        assert_eq!(fs::read(&record).expect("record remains"), b"{not-json");
        assert!(
            diagnostics_for(&report, DoctorCode::MetadataIntegrity)
                .iter()
                .any(|diagnostic| diagnostic.message.contains("corrupt"))
        );
    }

    #[test]
    fn missing_workspace_path_is_reported_without_reset() {
        let Some(fixture) = RepoFixture::init() else {
            return;
        };
        let missing = fixture.root.join("missing-workspace");
        let output = Command::new("jj")
            .current_dir(&fixture.repo)
            .args(["workspace", "add", "--name", "gone"])
            .arg(&missing)
            .output()
            .expect("add fixture workspace");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        fs::remove_dir_all(&missing).expect("remove fixture checkout");
        let before = fixture.client().operation_id().expect("operation before");

        let report = fixture.doctor("trunk()");
        let after = fixture.client().operation_id().expect("operation after");
        assert_eq!(
            before, after,
            "doctor must not reset or mutate workspace state"
        );
        assert!(report.has_errors());
        assert!(
            diagnostics_for(&report, DoctorCode::WorkspacePath)
                .iter()
                .any(|diagnostic| diagnostic.subject.as_deref() == Some("gone"))
        );
    }
}
