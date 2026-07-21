use anyhow::{Context, Result, anyhow, bail};
use std::cmp::Ordering;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Output};
use std::str::FromStr;

/// Oldest JJ release exercised by jj-waltz's compatibility test matrix.
pub const MINIMUM_SUPPORTED_JJ_VERSION: JjVersion = JjVersion::new(0, 39, 0);

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
        let args = [
            OsString::from("log"),
            OsString::from("-r"),
            revset.to_owned(),
            OsString::from("--no-graph"),
            OsString::from("--template"),
            OsString::from(
                "change_id ++ \"\\0\" ++ commit_id ++ \"\\0\" ++ description.first_line() ++ \"\\0\"",
            ),
        ];
        let output = self.run_at(operation_id, args)?;
        parse_resolved_revisions(revset, output.stdout()?)
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

fn parse_resolved_revisions(revset: &OsStr, output: &str) -> Result<ResolvedRevision> {
    let fields = output.split_terminator('\0').collect::<Vec<_>>();
    if fields.len() % 3 != 0 {
        bail!("JJ revision query returned malformed template output")
    }
    let revisions = fields
        .chunks_exact(3)
        .map(|fields| ResolvedRevision {
            change_id: fields[0].to_owned(),
            commit_id: fields[1].to_owned(),
            description: fields[2].to_owned(),
        })
        .collect::<Vec<_>>();
    match revisions.as_slice() {
        [revision] => Ok(revision.clone()),
        _ => bail!(
            "revset {:?} resolved to {} revisions; expected exactly one",
            revset,
            revisions.len()
        ),
    }
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
    fn parses_exactly_one_revision() {
        let revision =
            parse_resolved_revisions(OsStr::new("trunk()"), "change-id\0commit-id\0first line\0")
                .unwrap();
        assert_eq!(revision.change_id, "change-id");
        assert_eq!(revision.commit_id, "commit-id");
        assert_eq!(revision.description, "first line");

        let undescribed =
            parse_resolved_revisions(OsStr::new("@"), "other-change\0other-commit\0\0").unwrap();
        assert!(undescribed.description.is_empty());

        assert!(parse_resolved_revisions(OsStr::new("none()"), "").is_err());
        assert!(
            parse_resolved_revisions(
                OsStr::new("all()"),
                "change-1\0commit-1\0one\0change-2\0commit-2\0two\0",
            )
            .is_err()
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
    }
}
