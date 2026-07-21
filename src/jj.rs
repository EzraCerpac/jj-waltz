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
// corrupt record boundaries. The conflict guards keep diff statistics compatible with JJ 0.39.
const WORKSPACE_NAMES_TEMPLATE: &str = r#"json(name) ++ "\n""#;
const CURRENT_WORKSPACE_NAMES_TEMPLATE: &str =
    r#"if(target.current_working_copy(), json(name) ++ "\n", "")"#;
const WORKSPACE_FACTS_TEMPLATE: &str = r#"json(name) ++ "\t" ++ json(target.change_id()) ++ "\t" ++ json(target.commit_id()) ++ "\t" ++ json(target.description().first_line()) ++ "\t" ++ json(target.current_working_copy()) ++ "\t" ++ json(target.empty()) ++ "\t" ++ json(target.conflict()) ++ "\t" ++ json(target.divergent()) ++ "\t" ++ json(target.diff().files().len()) ++ "\t" ++ json(if(target.conflict(), 0, target.diff().stat().total_added())) ++ "\t" ++ json(if(target.conflict(), 0, target.diff().stat().total_removed())) ++ "\t" ++ json(target.conflicted_files().len()) ++ "\n""#;
const REVISION_TEMPLATE: &str =
    "change_id ++ \"\\0\" ++ commit_id ++ \"\\0\" ++ description.first_line() ++ \"\\0\"";
const LOCAL_BOOKMARK_NAMES_TEMPLATE: &str = r#"if(remote, "", json(name) ++ "\n")"#;
const BOOKMARK_NAMES_TEMPLATE: &str = r#"json(name) ++ "\n""#;

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
    pub files: u32,
    pub added: u32,
    pub removed: u32,
    pub conflicts: u32,
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
    if fields.len() % 3 != 0 {
        bail!("JJ revision query returned malformed template output")
    }
    Ok(fields
        .chunks_exact(3)
        .map(|fields| ResolvedRevision {
            change_id: fields[0].to_owned(),
            commit_id: fields[1].to_owned(),
            description: fields[2].to_owned(),
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
            files,
            added,
            removed,
            conflicts,
        ] = fields.as_slice()
        else {
            bail!(
                "JJ workspace query returned malformed record {} with {} fields; expected 12",
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
            files: parse_count(files, index, "files")?,
            added: parse_count(added, index, "added")?,
            removed: parse_count(removed, index, "removed")?,
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
            "JJ workspace query returned {field} count outside u32 range in record {}",
            record_index + 1
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
            "3",
            "18",
            "4",
            "2",
        ]
        .join("\t")
            + "\n";
        let parsed = parse_workspace_target_facts(&output).unwrap();
        let facts = &parsed["solver\tui"];
        assert_eq!(facts.description, "line one\nline two");
        assert_eq!(facts.files, 3);
        assert_eq!(facts.conflicts, 2);

        assert!(parse_workspace_target_facts("\"name\"\t\"too-few\"\n").is_err());
        let line = "\"same\"\t\"change\"\t\"commit\"\t\"description\"\ttrue\ttrue\tfalse\tfalse\t0\t0\t0\t0\n";
        assert!(parse_workspace_target_facts(&format!("{line}{line}")).is_err());
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
    }
}
