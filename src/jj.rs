use anyhow::{Context, Result, anyhow, bail};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Output};
use std::str::FromStr;

/// Oldest JJ release exercised by jj-waltz's compatibility test matrix.
pub const MINIMUM_SUPPORTED_JJ_VERSION: JjVersion = JjVersion::new(0, 39, 0);

// Keep every workspace field JSON encoded. Tabs and newlines inside names and descriptions cannot
// corrupt record boundaries.
const WORKSPACE_NAMES_TEMPLATE: &str = r#"json(name) ++ "\n""#;
/// `WorkspaceRef.root()` is newer than the minimum supported JJ; callers fall back
/// to per-workspace `jj workspace root --name` queries when the template is rejected.
pub(crate) const WORKSPACE_ROOTS_TEMPLATE: &str =
    r#"json(name) ++ "\t" ++ json(self.root()) ++ "\n""#;
const CURRENT_WORKSPACE_NAMES_TEMPLATE: &str =
    r#"if(target.current_working_copy(), json(name) ++ "\n", "")"#;
/// Every field here is cheap for any number of workspaces. Diff statistics are deliberately
/// absent: computing them reads file contents, which costs seconds across many workspaces, so
/// they are queried separately and only for the revisions that need them.
const WORKSPACE_FACTS_TEMPLATE: &str = r#"json(name) ++ "\t" ++ json(target.change_id()) ++ "\t" ++ json(target.commit_id()) ++ "\t" ++ json(target.description().first_line()) ++ "\t" ++ json(target.current_working_copy()) ++ "\t" ++ json(target.empty()) ++ "\t" ++ json(target.conflict()) ++ "\t" ++ json(target.divergent()) ++ "\t" ++ json(if(target.conflict(), target.conflicted_files().len(), 0)) ++ "\n""#;
/// The conflict guards keep diff statistics compatible with JJ 0.39.
const CHANGE_STATS_TEMPLATE: &str = r#"json(commit_id) ++ "\t" ++ json(self.diff().files().len()) ++ "\t" ++ json(if(self.conflict(), 0, self.diff().stat().total_added())) ++ "\t" ++ json(if(self.conflict(), 0, self.diff().stat().total_removed())) ++ "\n""#;
const COMMIT_IDS_TEMPLATE: &str = r#"commit_id ++ "\n""#;
const LOCAL_BOOKMARK_TARGETS_TEMPLATE: &str = r#"commit_id ++ local_bookmarks.map(|bookmark| "\t" ++ json(bookmark.name())).join("") ++ "\n""#;
const REVISION_TEMPLATE: &str =
    "change_id ++ \"\\0\" ++ commit_id ++ \"\\0\" ++ description.first_line() ++ \"\\0\"";
/// Revsets built from many IDs are split so one argument stays far below the smallest platform
/// command-line limit (32 KiB on Windows). Typical repositories need a single query.
const MAX_REVSET_CHARS: usize = 16 * 1024;
const LOCAL_BOOKMARK_NAMES_TEMPLATE: &str = r#"if(remote, "", json(name) ++ "\n")"#;
const BOOKMARK_NAMES_TEMPLATE: &str = r#"json(name) ++ "\n""#;
const TRACKED_FILES_TEMPLATE: &str = r#"json(path) ++ "\n""#;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JjVersion {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    pub prerelease: Option<String>,
}

impl JjVersion {
    pub const fn new(major: u64, minor: u64, patch: u64) -> Self {
        Self {
            major,
            minor,
            patch,
            prerelease: None,
        }
    }

    pub fn is_supported(&self) -> bool {
        self >= &MINIMUM_SUPPORTED_JJ_VERSION
    }
}

impl fmt::Display for JjVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if let Some(prerelease) = &self.prerelease {
            write!(formatter, "-{prerelease}")?;
        }
        Ok(())
    }
}

impl FromStr for JjVersion {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        let version = value
            .trim()
            .strip_prefix("jj ")
            .unwrap_or(value.trim())
            .split_whitespace()
            .next()
            .ok_or_else(|| anyhow!("JJ version output was empty"))?;
        let version = version.split_once('+').map_or(version, |(value, _)| value);
        let (release, prerelease) = version
            .split_once('-')
            .map_or((version, None), |(release, prerelease)| {
                (release, Some(prerelease))
            });
        if prerelease == Some("") {
            bail!("invalid JJ semantic version `{version}`")
        }
        let prerelease = prerelease.filter(|value| {
            value.len() != 40 || !value.bytes().all(|byte| byte.is_ascii_hexdigit())
        });

        let numbers = release.split('.').collect::<Vec<_>>();
        let [major, minor, patch] = numbers.as_slice() else {
            bail!("invalid JJ semantic version `{version}`")
        };

        Ok(Self {
            major: parse_release_number(major, version)?,
            minor: parse_release_number(minor, version)?,
            patch: parse_release_number(patch, version)?,
            prerelease: prerelease.map(ToOwned::to_owned),
        })
    }
}

impl PartialOrd for JjVersion {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for JjVersion {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.major, self.minor, self.patch)
            .cmp(&(other.major, other.minor, other.patch))
            .then_with(|| {
                compare_prerelease(self.prerelease.as_deref(), other.prerelease.as_deref())
            })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JjCapabilities {
    pub version: JjVersion,
    pub named_workspace_root: bool,
    pub operation_id_capture: bool,
    pub repository_config_path: bool,
}

impl JjCapabilities {
    fn for_version(version: JjVersion) -> Self {
        let supported = version.is_supported();
        Self {
            version,
            named_workspace_root: supported,
            operation_id_capture: supported,
            repository_config_path: supported,
        }
    }

    pub fn is_supported(&self) -> bool {
        self.version.is_supported()
    }
}

#[derive(Debug)]
pub struct JjOutput {
    output: Output,
}

impl JjOutput {
    pub fn success(&self) -> bool {
        self.output.status.success()
    }

    pub fn status(&self) -> ExitStatus {
        self.output.status
    }

    pub fn stdout(&self) -> Result<&str> {
        std::str::from_utf8(&self.output.stdout).context("JJ output was not valid UTF-8")
    }

    pub fn trimmed_stdout(&self) -> Result<String> {
        Ok(self.stdout()?.trim().to_owned())
    }

    pub fn stderr(&self) -> String {
        String::from_utf8_lossy(&self.output.stderr)
            .trim()
            .to_owned()
    }

    pub fn error_kind(&self) -> JjErrorKind {
        JjErrorKind::from_stderr(&self.stderr())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JjErrorKind {
    StaleWorkingCopy,
    Other,
}

impl JjErrorKind {
    fn from_stderr(stderr: &str) -> Self {
        let stderr = stderr.to_ascii_lowercase();
        if stderr.contains("working copy")
            && stderr.contains("stale")
            && stderr.contains("workspace update-stale")
        {
            Self::StaleWorkingCopy
        } else {
            Self::Other
        }
    }
}

#[derive(Debug)]
pub struct JjCommandError {
    kind: JjErrorKind,
    command: String,
    status: ExitStatus,
    stderr: String,
}

impl JjCommandError {
    fn from_output(args: &[OsString], output: &JjOutput) -> Self {
        Self {
            kind: output.error_kind(),
            command: display_command(args),
            status: output.status(),
            stderr: output.stderr(),
        }
    }

    pub fn kind(&self) -> JjErrorKind {
        self.kind
    }

    pub fn status(&self) -> ExitStatus {
        self.status
    }

    pub fn stderr(&self) -> &str {
        &self.stderr
    }
}

impl fmt::Display for JjCommandError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.stderr.is_empty() {
            write!(
                formatter,
                "{} failed with status {}",
                self.command, self.status
            )
        } else {
            write!(formatter, "{} failed: {}", self.command, self.stderr)
        }
    }
}

impl std::error::Error for JjCommandError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedRevision {
    pub change_id: String,
    pub commit_id: String,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceTargetFacts {
    pub name: String,
    pub change_id: String,
    pub commit_id: String,
    pub description: String,
    pub current_working_copy: bool,
    pub empty: bool,
    pub conflicted: bool,
    pub divergent: bool,
    pub conflicts: u32,
}

/// Size of one revision's change relative to its parents.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ChangeStats {
    pub files: u32,
    pub added: u32,
    pub removed: u32,
}

/// Where JJ records a workspace's checkout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordedRoot {
    /// The recorded checkout path. The directory may no longer exist.
    Path(PathBuf),
    /// JJ has no recorded path for this workspace.
    Unrecorded,
}

impl RecordedRoot {
    pub fn path(&self) -> Option<&Path> {
        match self {
            Self::Path(path) => Some(path),
            Self::Unrecorded => None,
        }
    }
}

/// Direct, shell-free JJ process adapter rooted at one working directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JjClient {
    cwd: PathBuf,
}

impl JjClient {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self { cwd: cwd.into() }
    }

    pub fn current() -> Result<Self> {
        Ok(Self::new(
            std::env::current_dir().context("failed to determine current directory")?,
        ))
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    pub fn run<I, S>(&self, args: I) -> Result<JjOutput>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let args = collect_args(args);
        let output = self.execute(&args)?;
        if output.success() {
            Ok(output)
        } else {
            Err(JjCommandError::from_output(&args, &output).into())
        }
    }

    /// Execute JJ while leaving non-zero status handling to the caller.
    pub fn run_unchecked<I, S>(&self, args: I) -> Result<JjOutput>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let args = collect_args(args);
        self.execute(&args)
    }

    /// Run a read-only query against one frozen repository operation.
    pub fn run_at<I, S>(&self, operation_id: impl AsRef<OsStr>, args: I) -> Result<JjOutput>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut frozen_args = vec![OsString::from("--at-operation")];
        frozen_args.push(operation_id.as_ref().to_owned());
        frozen_args.push(OsString::from("--ignore-working-copy"));
        frozen_args.extend(collect_args(args));
        self.run(frozen_args)
    }

    /// Execute against one frozen operation while leaving non-zero status handling to the caller.
    pub fn run_at_unchecked<I, S>(
        &self,
        operation_id: impl AsRef<OsStr>,
        args: I,
    ) -> Result<JjOutput>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut frozen_args = vec![OsString::from("--at-operation")];
        frozen_args.push(operation_id.as_ref().to_owned());
        frozen_args.push(OsString::from("--ignore-working-copy"));
        frozen_args.extend(collect_args(args));
        self.run_unchecked(frozen_args)
    }

    pub fn version(&self) -> Result<JjVersion> {
        self.run(["--version"])?
            .trimmed_stdout()?
            .parse()
            .context("failed to parse JJ version")
    }

    /// Probe the actual executable, including builds whose version predates the feature.
    pub fn supports_workspace_colocation(&self) -> Result<bool> {
        Ok(self
            .run(["--ignore-working-copy", "workspace", "add", "--help"])?
            .stdout()?
            .contains("--colocate"))
    }

    pub fn require_workspace_colocation(&self) -> Result<()> {
        if !self.supports_workspace_colocation()? {
            bail!(
                "workspace colocation requires JJ with `workspace add --colocate`; upgrade to JJ 0.46+ or use --no-colocate"
            )
        }
        // A local backend cannot create Git worktrees. Check before any mutation.
        self.run(["--ignore-working-copy", "git", "root"])
            .context("workspace colocation requires a Git-backed JJ repository")?;
        Ok(())
    }

    /// Inherit only a verified primary checkout's topology and effective setting,
    /// so creation from a JJ-only sibling has the same default as primary creation.
    pub(crate) fn inherited_workspace_colocation(&self) -> Result<bool> {
        if !self.supports_workspace_colocation()? {
            return Ok(false);
        }
        let caller = crate::context::discover(Some(self.cwd()));
        let Some(primary_root) = caller.jj.primary_checkout.as_ref() else {
            return Ok(false);
        };
        let primary = crate::context::discover(Some(primary_root));
        // A JJ-only sibling can legitimately have no readable Git checkout.
        // Its JJ identity must be sound; Git topology is checked at the primary.
        if caller.diagnostics.iter().any(|diagnostic| {
            matches!(
                diagnostic.code,
                "jj_metadata_invalid" | "jj_git_backend_invalid" | "jj_primary_unverified"
            )
        }) || primary.diagnostics.iter().any(|diagnostic| {
            matches!(
                diagnostic.code,
                "git_metadata_invalid"
                    | "jj_metadata_invalid"
                    | "jj_git_backend_invalid"
                    | "jj_primary_unverified"
            )
        }) {
            return Ok(false);
        }
        let same = |left: Option<&Path>, right: Option<&Path>| {
            left.zip(right).is_some_and(|(left, right)| {
                left.canonicalize()
                    .ok()
                    .zip(right.canonicalize().ok())
                    .is_some_and(|(left, right)| left == right)
            })
        };
        if !same(
            caller.jj.repository_path.as_deref(),
            primary.jj.repository_path.as_deref(),
        ) || !same(Some(primary_root), primary.jj.workspace_root.as_deref())
            || !same(
                primary.git.checkout_root.as_deref(),
                primary.jj.workspace_root.as_deref(),
            )
            || !same(
                primary.git.git_dir.as_deref(),
                primary.jj.git_backend_dir.as_deref(),
            )
            || !same(
                primary.git.common_dir.as_deref(),
                primary.jj.git_backend_common_dir.as_deref(),
            )
        {
            return Ok(false);
        }
        let setting = Self::new(primary_root).run_unchecked([
            "--ignore-working-copy",
            "config",
            "get",
            "git.colocate",
        ])?;
        // A custom build can advertise colocation without this setting. Unclear
        // topology or configuration never authorizes automatic Git worktree creation.
        Ok(setting.success() && setting.trimmed_stdout().is_ok_and(|value| value == "true"))
    }

    pub fn capabilities(&self) -> Result<JjCapabilities> {
        Ok(JjCapabilities::for_version(self.version()?))
    }

    /// List workspace names without snapshotting a working copy.
    pub fn workspace_names(&self) -> Result<Vec<String>> {
        let output = self.run([
            "--ignore-working-copy",
            "workspace",
            "list",
            "-T",
            WORKSPACE_NAMES_TEMPLATE,
        ])?;
        parse_json_string_lines(output.stdout()?, "workspace name")
    }

    /// List every workspace root with one JJ process, without snapshotting.
    ///
    /// Returns `None` when this JJ cannot render workspace roots in templates, so
    /// callers can fall back to one `jj workspace root --name` query per workspace.
    pub fn workspace_roots(&self) -> Result<Option<WorkspaceRoots>> {
        let output = self.run_unchecked([
            "--ignore-working-copy",
            "workspace",
            "list",
            "-T",
            WORKSPACE_ROOTS_TEMPLATE,
        ])?;
        if !output.success() {
            return Ok(None);
        }
        Ok(parse_workspace_roots(output.stdout()?))
    }

    /// [`Self::workspace_roots`] at one frozen repository operation.
    pub fn workspace_roots_at(&self, operation_id: &str) -> Result<Option<WorkspaceRoots>> {
        let output = self.run_at_unchecked(
            operation_id,
            ["workspace", "list", "-T", WORKSPACE_ROOTS_TEMPLATE],
        )?;
        if !output.success() {
            return Ok(None);
        }
        Ok(parse_workspace_roots(output.stdout()?))
    }

    /// Every workspace's recorded root, including the path of a deleted checkout.
    ///
    /// One batched listing answers this on JJ 0.46+. Older listings render a deleted checkout as
    /// unknown, so only those entries (and every entry on JJ without `WorkspaceRef.root()`) fall
    /// back to `jj workspace root --name`, whose error message names the recorded path.
    pub fn recorded_workspace_roots(
        &self,
        operation_id: Option<&str>,
    ) -> Result<BTreeMap<String, RecordedRoot>> {
        let listed = match operation_id {
            Some(operation_id) => self.workspace_roots_at(operation_id)?,
            None => self.workspace_roots()?,
        };
        let listed = match listed {
            Some(listed) => listed,
            None => self
                .workspace_names()?
                .into_iter()
                .map(|name| (name, None))
                .collect(),
        };
        listed
            .into_iter()
            .map(|(name, root)| {
                let root = match root {
                    Some(root) => RecordedRoot::Path(root),
                    None => self.recorded_workspace_root(&name, operation_id)?,
                };
                Ok((name, root))
            })
            .collect()
    }

    /// One workspace's recorded root, distinguishing a deleted checkout from an unrecorded path.
    pub fn recorded_workspace_root(
        &self,
        name: &str,
        operation_id: Option<&str>,
    ) -> Result<RecordedRoot> {
        let output = match operation_id {
            Some(operation_id) => {
                self.run_at_unchecked(operation_id, ["workspace", "root", "--name", name])?
            }
            None => {
                self.run_unchecked(["--ignore-working-copy", "workspace", "root", "--name", name])?
            }
        };
        if output.success() {
            return Ok(RecordedRoot::Path(PathBuf::from(output.trimmed_stdout()?)));
        }
        let message = output.stderr();
        if message.contains("Workspace has no recorded path") {
            return Ok(RecordedRoot::Unrecorded);
        }
        if let Some(path) = message.lines().find_map(|line| {
            line.trim()
                .strip_prefix("Error: ")
                .unwrap_or(line.trim())
                .strip_prefix("Cannot resolve absolute workspace path: ")
        }) {
            return Ok(RecordedRoot::Path(PathBuf::from(path)));
        }
        bail!("cannot read the recorded root of workspace {name}: {message}")
    }

    /// List targets that JJ marks as the current working copy without snapshotting it.
    pub fn current_workspace_target_names(&self) -> Result<Vec<String>> {
        let output = self.run([
            "--ignore-working-copy",
            "workspace",
            "list",
            "-T",
            CURRENT_WORKSPACE_NAMES_TEMPLATE,
        ])?;
        parse_json_string_lines(output.stdout()?, "current workspace name")
    }

    /// Read workspace target facts at one frozen repository operation.
    pub fn workspace_target_facts_at(
        &self,
        operation_id: impl AsRef<OsStr>,
    ) -> Result<BTreeMap<String, WorkspaceTargetFacts>> {
        let output = self.run_at(
            operation_id,
            ["workspace", "list", "-T", WORKSPACE_FACTS_TEMPLATE],
        )?;
        parse_workspace_target_facts(output.stdout()?)
    }

    /// Read one workspace target by its literal workspace name at a frozen operation.
    fn workspace_target_at(
        &self,
        operation_id: impl AsRef<OsStr>,
        workspace_name: &str,
    ) -> Result<WorkspaceTargetFacts> {
        self.workspace_target_facts_at(operation_id)?
            .remove(workspace_name)
            .ok_or_else(|| anyhow!("JJ workspace query did not return `{workspace_name}`"))
    }

    /// Capture the current operation without snapshotting or reconciling the working copy.
    pub fn operation_id(&self) -> Result<String> {
        let output = self.run_at(
            "@",
            [
                "operation",
                "log",
                "--limit=1",
                "--no-graph",
                "--template",
                "id ++ \"\\n\"",
            ],
        )?;
        let operation_ids = output
            .stdout()?
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>();
        match operation_ids.as_slice() {
            [operation_id] => Ok((*operation_id).to_owned()),
            _ => bail!(
                "JJ operation query returned {} IDs; expected exactly one",
                operation_ids.len()
            ),
        }
    }

    /// Resolve a revset to one full change ID, commit ID, and first-line description.
    pub fn resolve_one(&self, revset: impl AsRef<OsStr>) -> Result<ResolvedRevision> {
        let operation_id = self.operation_id()?;
        self.resolve_one_at(operation_id, revset)
    }

    /// Resolve exactly one revision at a previously captured repository operation.
    pub fn resolve_one_at(
        &self,
        operation_id: impl AsRef<OsStr>,
        revset: impl AsRef<OsStr>,
    ) -> Result<ResolvedRevision> {
        let revset = revset.as_ref();
        let revisions = self.resolve_all_at(operation_id, revset)?;
        match revisions.as_slice() {
            [revision] => Ok(revision.clone()),
            _ => bail!(
                "revset {:?} resolved to {} revisions; expected exactly one",
                revset,
                revisions.len()
            ),
        }
    }

    /// Resolve every revision in a revset at a previously captured repository operation.
    pub fn resolve_all_at(
        &self,
        operation_id: impl AsRef<OsStr>,
        revset: impl AsRef<OsStr>,
    ) -> Result<Vec<ResolvedRevision>> {
        let revset = revset.as_ref();
        let args = [
            OsString::from("log"),
            OsString::from("-r"),
            revset.to_owned(),
            OsString::from("--no-graph"),
            OsString::from("--template"),
            OsString::from(REVISION_TEMPLATE),
        ];
        let output = self.run_at(operation_id, args)?;
        parse_resolved_revisions(output.stdout()?)
    }

    /// Read local bookmark names at one frozen repository operation.
    pub fn local_bookmark_names_at(
        &self,
        operation_id: impl AsRef<OsStr>,
    ) -> Result<BTreeSet<String>> {
        let output = self.run_at(
            operation_id,
            [
                "bookmark",
                "list",
                "--template",
                LOCAL_BOOKMARK_NAMES_TEMPLATE,
            ],
        )?;
        Ok(parse_json_string_lines(output.stdout()?, "bookmark name")?
            .into_iter()
            .collect())
    }

    /// Read local bookmark names attached to a literal workspace target at one frozen operation.
    pub fn local_bookmark_names_for_workspace_at(
        &self,
        operation_id: impl AsRef<OsStr>,
        workspace_name: &str,
    ) -> Result<BTreeSet<String>> {
        let operation_id = operation_id.as_ref().to_owned();
        let target = self.workspace_target_at(&operation_id, workspace_name)?;
        self.local_bookmark_names_for_revset_at(&operation_id, &target.commit_id)
    }

    fn local_bookmark_names_for_revset_at(
        &self,
        operation_id: impl AsRef<OsStr>,
        revset: impl AsRef<OsStr>,
    ) -> Result<BTreeSet<String>> {
        let args = [
            OsString::from("bookmark"),
            OsString::from("list"),
            OsString::from("-r"),
            revset.as_ref().to_owned(),
            OsString::from("--template"),
            OsString::from(LOCAL_BOOKMARK_NAMES_TEMPLATE),
        ];
        let output = self.run_at(operation_id, args)?;
        Ok(parse_json_string_lines(output.stdout()?, "bookmark name")?
            .into_iter()
            .collect())
    }

    /// Read conflicted bookmark names at one frozen repository operation.
    pub fn conflicted_bookmark_names_at(
        &self,
        operation_id: impl AsRef<OsStr>,
    ) -> Result<BTreeSet<String>> {
        let output = self.run_at(
            operation_id,
            [
                "bookmark",
                "list",
                "--conflicted",
                "--template",
                BOOKMARK_NAMES_TEMPLATE,
            ],
        )?;
        Ok(parse_json_string_lines(output.stdout()?, "bookmark name")?
            .into_iter()
            .collect())
    }

    /// Every local bookmark's target commit IDs at one frozen operation. A conflicted bookmark
    /// has several targets. Names are never interpreted as revsets.
    pub fn local_bookmark_targets_at(
        &self,
        operation_id: impl AsRef<OsStr>,
    ) -> Result<BTreeMap<String, Vec<String>>> {
        let output = self.run_at(
            operation_id,
            [
                "log",
                "-r",
                "bookmarks()",
                "--no-graph",
                "--template",
                LOCAL_BOOKMARK_TARGETS_TEMPLATE,
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
            let commit_id = fields.next().unwrap_or_default();
            for bookmark in fields {
                let bookmark: String = parse_json_field(bookmark, index, "bookmark name")?;
                targets
                    .entry(bookmark)
                    .or_default()
                    .push(commit_id.to_owned());
            }
        }
        for commits in targets.values_mut() {
            commits.sort();
        }
        Ok(targets)
    }

    /// Change statistics for exact commit IDs at one frozen operation, in as few JJ processes as
    /// the platform's argument limits allow. Diff statistics read file contents, so callers pass
    /// only the revisions whose statistics they report.
    pub fn change_stats_at<'a>(
        &self,
        operation_id: &str,
        commit_ids: impl IntoIterator<Item = &'a str>,
    ) -> Result<BTreeMap<String, ChangeStats>> {
        let mut stats = BTreeMap::new();
        for revset in union_revsets(commit_ids.into_iter().map(ToOwned::to_owned)) {
            let output = self.run_at(
                operation_id,
                [
                    "log",
                    "-r",
                    revset.as_str(),
                    "--no-graph",
                    "--template",
                    CHANGE_STATS_TEMPLATE,
                ],
            )?;
            for (index, line) in output
                .stdout()?
                .lines()
                .filter(|line| !line.is_empty())
                .enumerate()
            {
                let fields = line.split('\t').collect::<Vec<_>>();
                let [commit_id, files, added, removed] = fields.as_slice() else {
                    bail!(
                        "JJ change statistics query returned malformed record {}",
                        index + 1
                    )
                };
                stats.insert(
                    parse_json_field(commit_id, index, "commit ID")?,
                    ChangeStats {
                        files: parse_count(files, index, "files")?,
                        added: parse_count(added, index, "added")?,
                        removed: parse_count(removed, index, "removed")?,
                    },
                );
            }
        }
        Ok(stats)
    }

    /// Which of the given exact commit IDs are present at one frozen operation. Values that are
    /// not commit IDs are never sent to JJ as revsets and are reported absent.
    pub fn existing_commit_ids_at<'a>(
        &self,
        operation_id: &str,
        commit_ids: impl IntoIterator<Item = &'a str>,
    ) -> Result<BTreeSet<String>> {
        let terms = commit_ids
            .into_iter()
            .filter(|id| is_commit_id(id))
            .map(|id| format!("present({id})"));
        let mut present = BTreeSet::new();
        for revset in union_revsets(terms) {
            let output = self.run_at(
                operation_id,
                [
                    "log",
                    "-r",
                    revset.as_str(),
                    "--no-graph",
                    "--template",
                    COMMIT_IDS_TEMPLATE,
                ],
            )?;
            present.extend(
                output
                    .stdout()?
                    .lines()
                    .filter(|line| !line.is_empty())
                    .map(ToOwned::to_owned),
            );
        }
        Ok(present)
    }

    /// Return JJ's repository-level config path.
    ///
    /// JJ may create the repository config directory when it does not exist.
    pub fn repo_config_path(&self) -> Result<PathBuf> {
        let output = self.run(["--ignore-working-copy", "config", "path", "--repo"])?;
        let mut lines = output.stdout()?.lines();
        let value = lines.next().unwrap_or_default();
        if value.is_empty() {
            bail!("JJ repository config path was empty")
        }
        if lines.next().is_some() {
            bail!("JJ repository config path returned multiple lines")
        }
        Ok(PathBuf::from(value))
    }

    /// List the files tracked in this workspace's working-copy commit, relative to
    /// the workspace root, without snapshotting the working copy.
    pub fn tracked_files(&self) -> Result<Vec<PathBuf>> {
        let output = self.run([
            "--ignore-working-copy",
            "file",
            "list",
            "-r",
            "@",
            "-T",
            TRACKED_FILES_TEMPLATE,
        ])?;
        // JJ repository paths are always `/`-separated, independent of platform.
        Ok(parse_json_string_lines(output.stdout()?, "file path")?
            .into_iter()
            .map(|path| path.split('/').collect())
            .collect())
    }

    /// Whether the watchman filesystem monitor is configured.
    ///
    /// Watchman's clock is recorded in the working-copy state and only describes the
    /// checkout that recorded it.
    pub fn uses_watchman(&self) -> Result<bool> {
        let output = self.run_unchecked([
            "--ignore-working-copy",
            "config",
            "get",
            "fsmonitor.backend",
        ])?;
        Ok(output.success() && output.trimmed_stdout()? == "watchman")
    }

    fn execute(&self, args: &[OsString]) -> Result<JjOutput> {
        let output = Command::new("jj")
            .current_dir(&self.cwd)
            .args(["--no-pager", "--color=never"])
            .args(args)
            .output()
            .with_context(|| {
                format!(
                    "failed to execute {} in {}",
                    display_command(args),
                    self.cwd.display()
                )
            })?;
        Ok(JjOutput { output })
    }
}

/// Replace a workspace's local working-copy state with a copy of another workspace's,
/// returning the replaced state.
///
/// The state records the checked-out tree, sparse patterns, and each file's size and
/// modification time, which lets JJ trust unchanged files without hashing them. Its
/// location is JJ's local working-copy layout rather than a public interface, so
/// callers must validate the result with a JJ command and restore the returned state
/// if JJ rejects it.
pub fn copy_tree_state(source_root: &Path, workspace_root: &Path) -> Result<Vec<u8>> {
    let source = tree_state_path(source_root);
    let state =
        std::fs::read(&source).with_context(|| format!("failed to read {}", source.display()))?;
    let previous = std::fs::read(tree_state_path(workspace_root)).with_context(|| {
        format!(
            "failed to read working-copy state in {}",
            workspace_root.display()
        )
    })?;
    write_tree_state(workspace_root, &state)?;
    Ok(previous)
}

/// Write a workspace's local working-copy state, as returned by [`copy_tree_state`].
pub fn write_tree_state(workspace_root: &Path, state: &[u8]) -> Result<()> {
    let path = tree_state_path(workspace_root);
    std::fs::write(&path, state).with_context(|| format!("failed to write {}", path.display()))
}

fn tree_state_path(workspace_root: &Path) -> PathBuf {
    workspace_root
        .join(".jj")
        .join("working_copy")
        .join("tree_state")
}

/// Exact commit IDs are hexadecimal of a backend-defined length. Anything else, such as a
/// revset expression stored in corrupt metadata, must never be evaluated as a revset.
pub(crate) fn is_commit_id(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Join revset terms with `|`, splitting into as few revsets as [`MAX_REVSET_CHARS`] allows.
pub(crate) fn union_revsets(terms: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut revsets = Vec::new();
    let mut current = String::new();
    for term in terms {
        if !current.is_empty() && current.len() + term.len() + 3 > MAX_REVSET_CHARS {
            revsets.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push_str(" | ");
        }
        current.push_str(&term);
    }
    if !current.is_empty() {
        revsets.push(current);
    }
    revsets
}

fn collect_args<I, S>(args: I) -> Vec<OsString>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    args.into_iter()
        .map(|arg| arg.as_ref().to_owned())
        .collect()
}

fn display_command(args: &[OsString]) -> String {
    format!("jj {args:?}")
}

fn parse_release_number(value: &str, version: &str) -> Result<u64> {
    if value.len() > 1 && value.starts_with('0') {
        bail!("invalid JJ semantic version `{version}`")
    }
    value
        .parse()
        .with_context(|| format!("invalid JJ semantic version `{version}`"))
}

fn compare_prerelease(left: Option<&str>, right: Option<&str>) -> Ordering {
    match (left, right) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(left), Some(right)) => {
            let mut left = left.split('.');
            let mut right = right.split('.');
            loop {
                match (left.next(), right.next()) {
                    (None, None) => return Ordering::Equal,
                    (None, Some(_)) => return Ordering::Less,
                    (Some(_), None) => return Ordering::Greater,
                    (Some(left), Some(right)) => {
                        let ordering = match (left.parse::<u64>(), right.parse::<u64>()) {
                            (Ok(left), Ok(right)) => left.cmp(&right),
                            (Ok(_), Err(_)) => Ordering::Less,
                            (Err(_), Ok(_)) => Ordering::Greater,
                            (Err(_), Err(_)) => left.cmp(right),
                        };
                        if ordering != Ordering::Equal {
                            return ordering;
                        }
                    }
                }
            }
        }
    }
}

fn parse_resolved_revisions(output: &str) -> Result<Vec<ResolvedRevision>> {
    let fields = output.split_terminator('\0').collect::<Vec<_>>();
    let (revisions, remainder) = fields.as_chunks::<3>();
    if !remainder.is_empty() {
        bail!("JJ revision query returned malformed template output")
    }
    Ok(revisions
        .iter()
        .map(|[change_id, commit_id, description]| ResolvedRevision {
            change_id: (*change_id).to_owned(),
            commit_id: (*commit_id).to_owned(),
            description: (*description).to_owned(),
        })
        .collect())
}

fn parse_workspace_target_facts(output: &str) -> Result<BTreeMap<String, WorkspaceTargetFacts>> {
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
            conflicts,
        ] = fields.as_slice()
        else {
            bail!(
                "JJ workspace query returned malformed record {} with {} fields; expected 9",
                index + 1,
                fields.len()
            )
        };

        let entry = WorkspaceTargetFacts {
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

/// Workspace name to recorded root; `None` when JJ could not render that root.
pub(crate) type WorkspaceRoots = BTreeMap<String, Option<PathBuf>>;

/// Parse `json(name) TAB json(root)` records. Any unparseable name makes the whole
/// listing unusable so callers fall back to per-workspace queries.
pub(crate) fn parse_workspace_roots(output: &str) -> Option<WorkspaceRoots> {
    output
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let (name, root) = line.split_once('\t')?;
            let name = serde_json::from_str::<String>(name).ok()?;
            let root = serde_json::from_str::<String>(root)
                .ok()
                .filter(|root| !root.is_empty())
                .map(PathBuf::from);
            Some((name, root))
        })
        .collect()
}

fn parse_json_string_lines(output: &str, field: &str) -> Result<Vec<String>> {
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
            "JJ query returned invalid {field} JSON in record {}",
            record_index + 1
        )
    })
}

fn parse_count(value: &str, record_index: usize, field: &str) -> Result<u32> {
    let value: u64 = parse_json_field(value, record_index, field)?;
    value.try_into().with_context(|| {
        format!(
            "JJ query returned {field} count outside u32 range in record {}",
            record_index + 1
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_batched_workspace_roots() {
        let roots = parse_workspace_roots(
            "\"default\"\t\"/repo\"\n\"tab\\tname\"\t\"/repo.tab\"\n\"lost\"\t\"\"\n",
        )
        .expect("listing parses");
        assert_eq!(roots["default"], Some(PathBuf::from("/repo")));
        assert_eq!(roots["tab\tname"], Some(PathBuf::from("/repo.tab")));
        assert_eq!(roots["lost"], None);
    }

    #[test]
    fn unparseable_workspace_root_listing_falls_back() {
        assert!(parse_workspace_roots("default /repo\n").is_none());
        assert!(parse_workspace_roots("not-json\t\"/repo\"\n").is_none());
    }

    #[test]
    fn parses_jj_semantic_versions() {
        assert_eq!(
            "jj 0.43.0".parse::<JjVersion>().unwrap(),
            JjVersion::new(0, 43, 0)
        );
        assert_eq!(
            "jj 1.2.3-beta.2+build.7"
                .parse::<JjVersion>()
                .unwrap()
                .to_string(),
            "1.2.3-beta.2"
        );
        assert_eq!(
            "jj 0.39.0-d9689cd9b51b4139d2842fcf6c30f65f4eed8cd1"
                .parse::<JjVersion>()
                .unwrap(),
            JjVersion::new(0, 39, 0)
        );
        assert!("jj 0.43".parse::<JjVersion>().is_err());
    }

    #[test]
    fn orders_prereleases_semantically() {
        let alpha = "0.43.0-alpha.2".parse::<JjVersion>().unwrap();
        let beta = "0.43.0-beta.1".parse::<JjVersion>().unwrap();
        let release = JjVersion::new(0, 43, 0);
        assert!(alpha < beta);
        assert!(beta < release);
    }

    #[test]
    fn derives_capabilities_from_supported_version() {
        let old = JjCapabilities::for_version(JjVersion::new(0, 38, 0));
        assert!(!old.is_supported());
        assert!(!old.repository_config_path);

        let minimum = JjCapabilities::for_version(MINIMUM_SUPPORTED_JJ_VERSION.clone());
        assert!(minimum.is_supported());
        assert!(minimum.named_workspace_root);
        assert!(minimum.operation_id_capture);
        assert!(minimum.repository_config_path);
    }

    #[test]
    fn classifies_only_actionable_stale_working_copy_errors() {
        assert_eq!(
            JjErrorKind::from_stderr(
                "Working copy is stale. Run `jj workspace update-stale` to update it."
            ),
            JjErrorKind::StaleWorkingCopy
        );
        assert_eq!(
            JjErrorKind::from_stderr("Working copy snapshot failed"),
            JjErrorKind::Other
        );
        assert_eq!(
            JjErrorKind::from_stderr("Revision is stale"),
            JjErrorKind::Other
        );
    }

    #[test]
    fn parses_revision_records() {
        let revisions = parse_resolved_revisions("change-id\0commit-id\0first line\0").unwrap();
        let revision = &revisions[0];
        assert_eq!(revision.change_id, "change-id");
        assert_eq!(revision.commit_id, "commit-id");
        assert_eq!(revision.description, "first line");

        let undescribed = &parse_resolved_revisions("other-change\0other-commit\0\0").unwrap()[0];
        assert!(undescribed.description.is_empty());

        assert!(parse_resolved_revisions("").unwrap().is_empty());
        assert_eq!(
            parse_resolved_revisions("change-1\0commit-1\0one\0change-2\0commit-2\0two\0")
                .unwrap()
                .len(),
            2
        );
        assert!(parse_resolved_revisions("change\0commit\0").is_err());
    }

    #[test]
    fn parses_json_tab_workspace_records_without_delimiter_ambiguity() {
        let output = [
            r#""solver\tui""#,
            r#""change-1""#,
            r#""commit-1""#,
            r#""line one\nline two""#,
            "true",
            "false",
            "true",
            "false",
            "2",
        ]
        .join("\t")
            + "\n";
        let parsed = parse_workspace_target_facts(&output).unwrap();
        let facts = &parsed["solver\tui"];
        assert_eq!(facts.description, "line one\nline two");
        assert!(facts.conflicted);
        assert_eq!(facts.conflicts, 2);

        assert!(parse_workspace_target_facts("\"name\"\t\"too-few\"\n").is_err());
        let line =
            "\"same\"\t\"change\"\t\"commit\"\t\"description\"\ttrue\ttrue\tfalse\tfalse\t0\n";
        assert!(parse_workspace_target_facts(&format!("{line}{line}")).is_err());
    }

    #[test]
    fn id_revsets_stay_within_the_argument_budget() {
        assert!(union_revsets(Vec::<String>::new()).is_empty());
        assert_eq!(
            union_revsets(["a".to_owned(), "b".to_owned()]),
            ["a | b".to_owned()]
        );
        let ids = (0..2_000)
            .map(|index| format!("{index:0>40}"))
            .collect::<Vec<_>>();
        let revsets = union_revsets(ids.clone());
        assert!(revsets.len() > 1);
        assert!(
            revsets
                .iter()
                .all(|revset| revset.len() <= MAX_REVSET_CHARS)
        );
        let rejoined = revsets
            .iter()
            .flat_map(|revset| revset.split(" | "))
            .collect::<Vec<_>>();
        assert_eq!(rejoined, ids);
    }

    #[test]
    fn commit_ids_accept_backend_lengths_but_not_revset_expressions() {
        for length in [40, 64, 128] {
            assert!(is_commit_id(&"a".repeat(length)));
        }
        for invalid in ["", "@", "root()", "abcd | efab", "xyz"] {
            assert!(!is_commit_id(invalid));
        }
    }

    #[test]
    fn batched_frozen_queries_answer_every_requested_revision_in_one_process() {
        if Command::new("jj").arg("--version").output().is_err() {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let client = JjClient::new(directory.path());
        client.run(["git", "init"]).unwrap();
        std::fs::write(directory.path().join("file"), "one\ntwo\n").unwrap();
        client.run(["commit", "-m", "first"]).unwrap();
        client
            .run(["bookmark", "create", "topic", "-r", "@-"])
            .unwrap();
        let first = client.resolve_one("@-").unwrap().commit_id;
        let empty = client.resolve_one("@").unwrap().commit_id;
        let operation_id = client.operation_id().unwrap();

        let stats = client
            .change_stats_at(&operation_id, [first.as_str(), empty.as_str()])
            .unwrap();
        assert_eq!(
            stats[&first],
            ChangeStats {
                files: 1,
                added: 2,
                removed: 0
            }
        );
        assert_eq!(stats[&empty], ChangeStats::default());
        assert!(
            client
                .change_stats_at(&operation_id, std::iter::empty())
                .unwrap()
                .is_empty()
        );

        let missing = "f".repeat(first.len());
        assert_eq!(
            client
                .existing_commit_ids_at(&operation_id, [first.as_str(), &missing, "root()"])
                .unwrap(),
            BTreeSet::from([first.clone()])
        );

        assert_eq!(
            client.local_bookmark_targets_at(&operation_id).unwrap(),
            BTreeMap::from([("topic".to_owned(), vec![first])])
        );

        let roots = client
            .recorded_workspace_roots(Some(&operation_id))
            .unwrap();
        assert_eq!(
            roots["default"]
                .path()
                .map(|path| path.canonicalize().unwrap()),
            Some(directory.path().canonicalize().unwrap())
        );
    }

    #[test]
    fn captures_and_queries_a_frozen_operation() {
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
        let operation_id = client.operation_id().unwrap();
        let revision = client.resolve_one_at(&operation_id, "@").unwrap();
        assert!(!operation_id.is_empty());
        assert!(!revision.change_id.is_empty());
        assert!(!revision.commit_id.is_empty());
        assert_eq!(client.workspace_names().unwrap(), ["default"]);
        assert_eq!(
            client.current_workspace_target_names().unwrap(),
            ["default"]
        );
        assert!(
            client
                .workspace_target_facts_at(&operation_id)
                .unwrap()
                .contains_key("default")
        );
        assert!(
            client
                .local_bookmark_names_at(&operation_id)
                .unwrap()
                .is_empty()
        );

        client
            .run(["bookmark", "create", "topic", "-r", "@"])
            .unwrap();
        let bookmark_operation = client.operation_id().unwrap();
        assert_eq!(
            client
                .local_bookmark_names_at(&bookmark_operation)
                .unwrap()
                .into_iter()
                .collect::<Vec<_>>(),
            ["topic"]
        );
        assert_eq!(
            client
                .local_bookmark_names_for_workspace_at(&bookmark_operation, "default")
                .unwrap()
                .into_iter()
                .collect::<Vec<_>>(),
            ["topic"]
        );
    }
}
