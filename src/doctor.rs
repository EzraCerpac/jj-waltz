use crate::jj::{JjClient, MINIMUM_SUPPORTED_JJ_VERSION};
use crate::links::{self, LinkCheckState};
use crate::metadata::{ManagedWorkspaceMetadata, WorkspaceMetadataStore};
use crate::workspace;
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
    configuration_error: Option<String>,
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
            configuration_error: None,
            repository_config_path: None,
        }
    }

    /// Builds a report when the user configuration cannot supply a trunk revset.
    pub fn current_with_configuration_error(error: impl Into<String>) -> Result<Self> {
        let mut engine = Self::new(JjClient::current()?, "");
        engine.configuration_error = Some(error.into());
        Ok(engine)
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

        if let Some(error) = &self.configuration_error {
            report.push(DoctorDiagnostic::error(
                DoctorCode::Configuration,
                format!("could not load jj-waltz configuration: {error}"),
                Some("fix or remove the invalid jj-waltz config.toml file"),
            ));
        }

        self.check_jj_version(&mut report);
        let operation_id = self.check_operation_snapshot(&mut report);
        let trunk = self.check_trunk(
            &mut report,
            operation_id.as_deref(),
            self.configuration_error.is_none(),
        );
        report.repository.trunk = trunk;

        let metadata = self.check_metadata(&mut report);
        let workspaces = self.check_workspace_paths(&mut report, operation_id.as_deref());
        self.check_metadata_consistency(
            &mut report,
            operation_id.as_deref(),
            metadata.as_deref(),
            workspaces.as_deref(),
        );
        self.check_workspace_links(&mut report, metadata.as_deref(), workspaces.as_deref());
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
        configuration_loaded: bool,
    ) -> Option<DoctorRevision> {
        if !configuration_loaded {
            report.push(DoctorDiagnostic::skipped(
                DoctorCode::TrunkRevset,
                "trunk revset was not evaluated because jj-waltz configuration could not be loaded",
                Some("fix the jj-waltz config.toml file, then rerun doctor"),
            ));
            return None;
        }
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
        let current_root = query_current_workspace_root(&self.client, operation_id).ok();

        let mut problems = 0;
        for workspace in &mut workspaces {
            let path = match query_workspace_root(&self.client, operation_id, &workspace.name) {
                Ok(path) => Ok(path),
                Err(_) if workspace.current && current_root.is_some() => {
                    Ok(current_root.clone().expect("current root checked above"))
                }
                Err(error) => Err(error),
            };
            match path {
                Ok(path) => {
                    workspace.path = Some(path.clone());
                    match validate_workspace_path(&path) {
                        Ok(()) => {}
                        Err(error) => {
                            workspace.path = None;
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

    fn check_workspace_links(
        &self,
        report: &mut DoctorReport,
        metadata: Option<&[ManagedWorkspaceMetadata]>,
        workspaces: Option<&[DoctorWorkspace]>,
    ) {
        let (Some(metadata), Some(workspaces)) = (metadata, workspaces) else {
            report.push(DoctorDiagnostic::skipped(
                DoctorCode::WorkspaceLink,
                "workspace links were not checked because metadata or workspace discovery failed",
                Some("fix the reported metadata or workspace-path problem, then rerun doctor"),
            ));
            return;
        };

        let workspace_by_name = workspaces
            .iter()
            .map(|workspace| (workspace.name.as_str(), workspace))
            .collect::<std::collections::BTreeMap<_, _>>();

        let mut inspectable = Vec::new();
        for record in metadata {
            let Some(workspace) = workspace_by_name.get(record.workspace_name.as_str()) else {
                report.push(
                    DoctorDiagnostic::skipped(
                        DoctorCode::WorkspaceLink,
                        "managed workspace has no matching JJ workspace; links could not be inspected",
                        Some("restore the workspace or run `jw prune` after confirming it is gone"),
                    )
                    .with_subject(&record.workspace_name),
                );
                continue;
            };
            let Some(workspace_root) = workspace.path.as_deref() else {
                report.push(
                    DoctorDiagnostic::skipped(
                        DoctorCode::WorkspaceLink,
                        "managed workspace has no usable checkout; links could not be inspected",
                        Some("restore the checkout or run `jw prune` if it was removed"),
                    )
                    .with_subject(&record.workspace_name),
                );
                continue;
            };
            inspectable.push((record, workspace_root));
        }

        if inspectable.is_empty() {
            return;
        }

        let Some(config_root) = default_link_config_root(workspaces) else {
            report.push(DoctorDiagnostic::skipped(
                DoctorCode::WorkspaceLink,
                "workspace links were not checked because the default workspace path is unavailable",
                Some("restore the default workspace checkout, then rerun doctor"),
            ));
            return;
        };

        let Some(link_config) = (match links::load_link_config(&config_root) {
            Ok(config) => config,
            Err(error) => {
                report.push(DoctorDiagnostic::error(
                    DoctorCode::WorkspaceLink,
                    format!("could not load configured workspace links: {error:#}"),
                    Some("fix or remove the invalid .jwlinks.toml file, then rerun doctor"),
                ));
                return;
            }
        }) else {
            return;
        };

        for (record, workspace_root) in inspectable {
            let inspections =
                match links::inspect_loaded_workspace_links(&link_config, workspace_root) {
                    Ok(inspections) => inspections,
                    Err(error) => {
                        report.push(
                        DoctorDiagnostic::error(
                            DoctorCode::WorkspaceLink,
                            format!("could not inspect configured workspace links: {error:#}"),
                            Some("fix or remove the invalid .jwlinks.toml file, then rerun doctor"),
                        )
                        .with_subject(&record.workspace_name),
                    );
                        continue;
                    }
                };

            let mut inspections = inspections;
            inspections.sort_by(|left, right| left.source.cmp(&right.source));
            for inspection in inspections {
                let subject = format!(
                    "{}:{}",
                    record.workspace_name,
                    display_link_path(workspace_root, &inspection.source)
                );
                let target = inspection.target.display();
                let diagnostic = match inspection.state {
                    LinkCheckState::Satisfied => DoctorDiagnostic::passed(
                        DoctorCode::WorkspaceLink,
                        format!("link is satisfied; target {}", target),
                    ),
                    LinkCheckState::Missing => DoctorDiagnostic::error(
                        DoctorCode::WorkspaceLink,
                        format!("link is missing; target {}", target),
                        Some(
                            "run `jw links apply` in the workspace or restore the required target",
                        ),
                    ),
                    LinkCheckState::Skipped => DoctorDiagnostic::warning(
                        DoctorCode::WorkspaceLink,
                        format!(
                            "optional link skipped because target is missing: {}",
                            target
                        ),
                        Some("no action is needed if the optional target is intentionally absent"),
                    ),
                    LinkCheckState::Conflicting => DoctorDiagnostic::error(
                        DoctorCode::WorkspaceLink,
                        format!(
                            "link conflicts with existing path; expected target {}",
                            target
                        ),
                        Some(
                            "move the private path or correct the link config, then run `jw links apply`",
                        ),
                    ),
                    LinkCheckState::Unreadable(error) => DoctorDiagnostic::error(
                        DoctorCode::WorkspaceLink,
                        format!("link could not be inspected: {error}"),
                        Some("restore access to the source or target path, then rerun `jw doctor`"),
                    ),
                };
                report.push(diagnostic.with_subject(subject));
            }
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
                diagnostic.label(),
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

    fn warning(
        code: DoctorCode,
        message: impl Into<String>,
        remedy: Option<impl Into<String>>,
    ) -> Self {
        Self {
            code,
            state: DoctorState::Skipped,
            severity: DoctorSeverity::Warning,
            subject: None,
            message: message.into(),
            remedy: remedy.map(Into::into),
        }
    }

    fn label(&self) -> &'static str {
        if self.severity == DoctorSeverity::Warning {
            "WARN"
        } else {
            self.state.label()
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
    Configuration,
    JjVersion,
    OperationSnapshot,
    TrunkRevset,
    MetadataIntegrity,
    WorkspacePath,
    MetadataConsistency,
    WorkspaceLink,
    BookmarkConflict,
    DivergentChange,
    WorkingCopyStale,
    ShellIntegration,
}

impl DoctorCode {
    fn label(self) -> &'static str {
        match self {
            Self::Configuration => "configuration",
            Self::JjVersion => "jj-version",
            Self::OperationSnapshot => "operation-snapshot",
            Self::TrunkRevset => "trunk-revset",
            Self::MetadataIntegrity => "metadata-integrity",
            Self::WorkspacePath => "workspace-path",
            Self::MetadataConsistency => "metadata-consistency",
            Self::WorkspaceLink => "workspace-link",
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
    current: bool,
    path: Option<PathBuf>,
}

fn query_revisions(
    client: &JjClient,
    operation_id: &str,
    revset: &str,
) -> Result<Vec<DoctorRevision>> {
    Ok(client
        .resolve_all_at(operation_id, revset)?
        .into_iter()
        .map(|revision| DoctorRevision {
            change_id: revision.change_id,
            commit_id: revision.commit_id,
            description: revision.description,
        })
        .collect())
}

fn query_workspaces(client: &JjClient, operation_id: &str) -> Result<Vec<DoctorWorkspace>> {
    Ok(client
        .workspace_target_facts_at(operation_id)?
        .into_values()
        .map(|facts| DoctorWorkspace {
            name: facts.name,
            commit_id: facts.commit_id,
            divergent: facts.divergent,
            current: facts.current_working_copy,
            path: None,
        })
        .collect())
}

fn default_link_config_root(workspaces: &[DoctorWorkspace]) -> Option<PathBuf> {
    if let Some(default) = workspaces
        .iter()
        .find(|workspace| workspace.name == "default")
    {
        return default.path.clone();
    }

    let current = workspaces.iter().find(|workspace| workspace.current)?;
    let current_root = current.path.as_deref()?;
    let base_root = workspace::workspace_base_root(current_root, &current.name).ok()?;
    let canonical_current = fs::canonicalize(current_root).ok()?;
    (fs::canonicalize(&base_root).ok()? == canonical_current).then(|| current_root.to_owned())
}

fn display_link_path(workspace_root: &Path, path: &Path) -> String {
    path.strip_prefix(workspace_root)
        .map(|relative| relative.display().to_string())
        .unwrap_or_else(|_| path.display().to_string())
}

fn query_workspace_root(client: &JjClient, operation_id: &str, name: &str) -> Result<PathBuf> {
    let output = client.run_at_unchecked(operation_id, ["workspace", "root", "--name", name])?;
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

fn query_current_workspace_root(client: &JjClient, operation_id: &str) -> Result<PathBuf> {
    let output = client.run_at_unchecked(operation_id, ["workspace", "root"])?;
    if !output.success() {
        let message = output.stderr();
        bail!(if message.is_empty() {
            "current workspace root lookup failed".to_owned()
        } else {
            message
        })
    }
    let path = output.trimmed_stdout()?;
    if path.is_empty() {
        bail!("current workspace root lookup returned an empty path")
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
    client.local_bookmark_names_at(operation_id)
}

fn query_conflicted_bookmarks(client: &JjClient, operation_id: &str) -> Result<Vec<String>> {
    Ok(client
        .conflicted_bookmark_names_at(operation_id)?
        .into_iter()
        .collect())
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
        assert_eq!(
            diagnostics_for(&report, DoctorCode::WorkspaceLink)
                .iter()
                .filter(|diagnostic| diagnostic.state == DoctorState::Skipped)
                .count(),
            1,
            "link inspection must report an unreadable metadata prerequisite"
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
        let store = fixture.metadata_store();
        let base = fixture
            .client()
            .resolve_one("trunk()")
            .expect("resolve fixture trunk");
        store
            .upsert(&ManagedWorkspaceMetadata {
                workspace_name: "gone".to_owned(),
                created_at_unix_ms: 1,
                creation_operation_id: fixture.client().operation_id().expect("operation"),
                creation_base_commit_id: base.commit_id,
                associated_bookmark: None,
                intended_remote: None,
            })
            .expect("write stale metadata");
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
        assert!(
            diagnostics_for(&report, DoctorCode::WorkspaceLink)
                .iter()
                .any(|diagnostic| {
                    diagnostic.subject.as_deref() == Some("gone")
                        && diagnostic.state == DoctorState::Skipped
                }),
            "{}",
            report.render_plain()
        );
    }

    #[cfg(unix)]
    #[test]
    fn workspace_links_report_optional_skip_and_private_conflict() {
        let Some(fixture) = RepoFixture::init() else {
            return;
        };
        let child = fixture.root.join("child");
        let output = Command::new("jj")
            .current_dir(&fixture.repo)
            .args(["workspace", "add", "--name", "child"])
            .arg(&child)
            .output()
            .expect("add child workspace");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );

        fs::write(
            fixture.repo.join(".jwlinks.toml"),
            r#"
                [[link]]
                source = "node_modules"
                target = "missing-node-modules"
                required = false
            "#,
        )
        .expect("write link config");
        fs::create_dir_all(child.join("node_modules")).expect("create private node_modules");

        let store = fixture.metadata_store();
        let base = fixture
            .client()
            .resolve_one("trunk()")
            .expect("resolve fixture trunk");
        for workspace_name in ["default", "child"] {
            store
                .upsert(&ManagedWorkspaceMetadata {
                    workspace_name: workspace_name.to_owned(),
                    created_at_unix_ms: 1,
                    creation_operation_id: fixture.client().operation_id().expect("operation"),
                    creation_base_commit_id: base.commit_id.clone(),
                    associated_bookmark: None,
                    intended_remote: None,
                })
                .expect("write metadata");
        }

        let report = fixture.doctor("trunk()");
        let diagnostics = diagnostics_for(&report, DoctorCode::WorkspaceLink);
        assert_eq!(diagnostics.len(), 2, "{}", report.render_plain());
        let optional_skip = diagnostics
            .iter()
            .find(|diagnostic| diagnostic.subject.as_deref() == Some("default:node_modules"))
            .expect("default optional link diagnostic");
        assert_eq!(optional_skip.state, DoctorState::Skipped);
        assert_eq!(optional_skip.severity, DoctorSeverity::Warning);
        let conflict = diagnostics
            .iter()
            .find(|diagnostic| diagnostic.subject.as_deref() == Some("child:node_modules"))
            .expect("child conflict diagnostic");
        assert_eq!(conflict.state, DoctorState::Failed);
        assert_eq!(conflict.severity, DoctorSeverity::Error);
        assert!(
            report
                .render_plain()
                .contains("WARN workspace-link [default:node_modules]")
        );
        assert!(
            report
                .render_plain()
                .contains("FAIL workspace-link [child:node_modules]")
        );
        assert_eq!(report.summary.warnings, 1);
        assert!(report.has_errors());

        let json = serde_json::to_value(&report).expect("serialize doctor report");
        assert_eq!(
            json["diagnostics"]
                .as_array()
                .expect("diagnostics array")
                .iter()
                .find(|diagnostic| diagnostic["subject"] == "default:node_modules")
                .expect("optional diagnostic")["severity"],
            "warning"
        );
    }

    #[test]
    fn workspace_links_report_unreadable_rule_and_continue_with_later_rules() {
        let Some(fixture) = RepoFixture::init() else {
            return;
        };
        fs::write(fixture.repo.join("blocked"), "not a directory")
            .expect("create unreadable target parent");
        fs::create_dir_all(fixture.repo.join("valid-target")).expect("create valid target");
        fs::write(
            fixture.repo.join(".jwlinks.toml"),
            r#"
                [[link]]
                source = "unreadable"
                target = "blocked/missing"
                required = true

                [[link]]
                source = "valid-target"
                target = "valid-target"
                required = true
            "#,
        )
        .expect("write link config");

        let store = fixture.metadata_store();
        let base = fixture
            .client()
            .resolve_one("trunk()")
            .expect("resolve fixture trunk");
        store
            .upsert(&ManagedWorkspaceMetadata {
                workspace_name: "default".to_owned(),
                created_at_unix_ms: 1,
                creation_operation_id: fixture.client().operation_id().expect("operation"),
                creation_base_commit_id: base.commit_id,
                associated_bookmark: None,
                intended_remote: None,
            })
            .expect("write metadata");

        let report = fixture.doctor("trunk()");
        let diagnostics = diagnostics_for(&report, DoctorCode::WorkspaceLink);
        assert_eq!(diagnostics.len(), 2, "{}", report.render_plain());
        let unreadable = diagnostics
            .iter()
            .find(|diagnostic| diagnostic.subject.as_deref() == Some("default:unreadable"))
            .expect("unreadable link diagnostic");
        assert_eq!(unreadable.state, DoctorState::Failed);
        assert_eq!(unreadable.severity, DoctorSeverity::Error);
        assert!(unreadable.message.contains("could not be inspected"));
        let valid = diagnostics
            .iter()
            .find(|diagnostic| diagnostic.subject.as_deref() == Some("default:valid-target"))
            .expect("valid link diagnostic");
        assert_eq!(valid.state, DoctorState::Passed);
        assert_eq!(valid.severity, DoctorSeverity::Info);
    }

    #[test]
    fn default_link_config_root_uses_current_workspace_when_default_name_is_absent() {
        let tempdir = tempfile::tempdir().expect("create temp directory");
        let root = tempdir.path().join("repository");
        fs::create_dir_all(root.join(".jj")).expect("create workspace root");
        let workspaces = vec![DoctorWorkspace {
            name: "feature".to_owned(),
            commit_id: "commit".to_owned(),
            divergent: false,
            current: true,
            path: Some(root.clone()),
        }];

        assert_eq!(default_link_config_root(&workspaces), Some(root));
    }
}
