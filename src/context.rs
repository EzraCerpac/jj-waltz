use anyhow::Error;
use serde::Serialize;
use std::env;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

pub const CONTEXT_SCHEMA_VERSION: u32 = 1;

/// A read-only description of the checkout and repository visible from a path.
#[derive(Debug, Clone, Serialize)]
pub struct ContextReport {
    pub schema_version: u32,
    pub command: &'static str,
    pub path: PathBuf,
    pub git: GitContext,
    pub jj: JjContext,
    pub diagnostics: Vec<ContextDiagnostic>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct GitContext {
    pub checkout_root: Option<PathBuf>,
    pub git_dir: Option<PathBuf>,
    pub common_dir: Option<PathBuf>,
    pub linked_worktree: Option<bool>,
    pub head_commit: Option<String>,
    pub head_ref: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct JjContext {
    pub workspace_root: Option<PathBuf>,
    pub workspace_name: Option<String>,
    pub repository_path: Option<PathBuf>,
    pub git_backend_dir: Option<PathBuf>,
    pub git_backend_common_dir: Option<PathBuf>,
    pub primary_checkout: Option<PathBuf>,
    pub primary_workspace: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ContextDiagnostic {
    pub severity: DiagnosticSeverity,
    pub code: &'static str,
    pub message: String,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DiagnosticSeverity {
    Warning,
    Error,
}

#[derive(Debug)]
struct ProbeError {
    program: &'static str,
    error: io::Error,
}

impl ContextDiagnostic {
    fn warning(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            severity: DiagnosticSeverity::Warning,
            code,
            message: message.into(),
        }
    }

    fn error(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            severity: DiagnosticSeverity::Error,
            code,
            message: message.into(),
        }
    }
}

/// Inspect the path without snapshotting a working copy or changing repository state.
pub fn discover(path: Option<&Path>) -> ContextReport {
    let (path, mut diagnostics) = normalize_path(path);
    let mut git = GitContext::default();
    let mut jj = JjContext::default();

    if path.is_dir() && fs::read_dir(&path).is_ok() {
        let git_probe = probe_git(&path, &mut diagnostics);
        git = git_probe.context;

        let jj_probe = probe_jj(&path, &mut diagnostics);
        jj = jj_probe.context;

        if jj_probe.found {
            discover_primary_from_jj(&git, &mut jj, &mut diagnostics);
        } else if git.checkout_root.is_some() {
            discover_primary_from_git(&git, &mut jj, &mut diagnostics);
        }
    } else {
        diagnostics.push(ContextDiagnostic::error(
            "path_unavailable",
            format!("path is not an accessible directory: {}", path.display()),
        ));
    }

    ContextReport {
        schema_version: CONTEXT_SCHEMA_VERSION,
        command: "context",
        path,
        git,
        jj,
        diagnostics,
    }
}

/// Add a useful explanation when a workspace command was run from Git without JJ metadata.
pub fn add_workspace_hint(error: Error) -> Error {
    if !looks_like_missing_jj_repository(&error) {
        return error;
    }
    let Ok(path) = env::current_dir() else {
        return error;
    };
    let report = discover(Some(&path));
    if report.git.checkout_root.is_none() || report.jj.workspace_root.is_some() {
        return error;
    }

    let hint = match report.jj.primary_checkout.as_deref() {
        Some(primary) => format!(
            "current directory is a Git checkout without a JJ workspace; a related JJ checkout was verified at {}. Run `jw context` there and use that workspace for JJ operations",
            primary.display()
        ),
        None => "current directory is a Git checkout without a JJ workspace; run `jw context` from a JJ checkout before using workspace commands".to_owned(),
    };
    error.context(hint)
}

fn looks_like_missing_jj_repository(error: &Error) -> bool {
    let text = format!("{error:#}").to_ascii_lowercase();
    text.contains("no jj repo")
        || text.contains("there is no jj repo")
        || text.contains("not a jj repo")
}

#[derive(Debug, Default)]
struct GitProbe {
    context: GitContext,
}

fn probe_git(path: &Path, diagnostics: &mut Vec<ContextDiagnostic>) -> GitProbe {
    let topology = match run_git(
        path,
        [
            "rev-parse",
            "--path-format=absolute",
            "--absolute-git-dir",
            "--git-common-dir",
            "--is-inside-work-tree",
            "--is-bare-repository",
        ],
    ) {
        Ok(output) => output,
        Err(error) => {
            diagnostics.push(ContextDiagnostic::error(
                "git_unavailable",
                format!("could not run git: {}", error.error),
            ));
            return GitProbe::default();
        }
    };

    if !topology.status.success() {
        if has_git_metadata(path) {
            diagnostics.push(ContextDiagnostic::error(
                "git_metadata_invalid",
                format_probe_failure("git metadata could not be read", &topology),
            ));
        }
        return GitProbe::default();
    }

    let values = nonempty_lines(&topology.stdout);
    if values.len() != 4 {
        diagnostics.push(ContextDiagnostic::error(
            "git_metadata_invalid",
            format!(
                "git topology query returned {} fields; expected 4",
                values.len()
            ),
        ));
        return GitProbe::default();
    }

    let git_dir = canonical_or_path(PathBuf::from(values[0]));
    let common_dir = canonical_or_path(PathBuf::from(values[1]));
    let inside_work_tree = values[2] == "true";
    let bare = values[3] == "true";
    let checkout_root = if inside_work_tree {
        match run_git(path, ["rev-parse", "--show-toplevel"]) {
            Ok(output) if output.status.success() => {
                Some(canonical_or_path(PathBuf::from(trimmed(&output.stdout))))
            }
            Ok(output) => {
                diagnostics.push(ContextDiagnostic::error(
                    "git_metadata_invalid",
                    format_probe_failure("Git checkout root could not be read", &output),
                ));
                None
            }
            Err(error) => {
                diagnostics.push(ContextDiagnostic::error(
                    "git_unavailable",
                    format!("could not read Git checkout root: {}", error.error),
                ));
                None
            }
        }
    } else {
        None
    };
    let (head_commit, head_ref) = git_head(path, diagnostics);

    GitProbe {
        context: GitContext {
            checkout_root,
            git_dir: Some(git_dir.clone()),
            common_dir: Some(common_dir.clone()),
            linked_worktree: Some(inside_work_tree && git_dir != common_dir),
            head_commit,
            head_ref,
        },
    }
    .with_bare_warning(bare, diagnostics)
}

impl GitProbe {
    fn with_bare_warning(self, bare: bool, diagnostics: &mut Vec<ContextDiagnostic>) -> Self {
        if bare {
            diagnostics.push(ContextDiagnostic::warning(
                "bare_repository",
                "Git reported a bare repository; no working-tree checkout is available",
            ));
        }
        self
    }
}

fn git_head(
    path: &Path,
    diagnostics: &mut Vec<ContextDiagnostic>,
) -> (Option<String>, Option<String>) {
    let commit = match run_git(path, ["rev-parse", "--verify", "HEAD"]) {
        Ok(output) if output.status.success() => Some(trimmed(&output.stdout)),
        Ok(output) => {
            diagnostics.push(ContextDiagnostic::warning(
                "git_head_unavailable",
                format_probe_failure(
                    "Git HEAD is unavailable (the repository may be unborn or damaged)",
                    &output,
                ),
            ));
            None
        }
        Err(error) => {
            diagnostics.push(ContextDiagnostic::error(
                "git_unavailable",
                format!("could not read Git HEAD: {}", error.error),
            ));
            None
        }
    };
    let head_ref = match run_git(path, ["symbolic-ref", "--quiet", "--short", "HEAD"]) {
        Ok(output) if output.status.success() => Some(trimmed(&output.stdout)),
        Ok(_) => None,
        Err(error) => {
            diagnostics.push(ContextDiagnostic::error(
                "git_unavailable",
                format!("could not read Git HEAD reference: {}", error.error),
            ));
            None
        }
    };
    (commit, head_ref)
}

#[derive(Debug, Default)]
struct JjProbe {
    context: JjContext,
    found: bool,
}

fn probe_jj(path: &Path, diagnostics: &mut Vec<ContextDiagnostic>) -> JjProbe {
    let root = match run_jj(path, ["root"]) {
        Ok(output) => output,
        Err(error) => {
            diagnostics.push(ContextDiagnostic::error(
                "jj_unavailable",
                format!("could not run jj: {}", error.error),
            ));
            return JjProbe::default();
        }
    };
    if !root.status.success() {
        if has_jj_metadata(path) {
            diagnostics.push(ContextDiagnostic::error(
                "jj_metadata_invalid",
                format_probe_failure("JJ metadata could not be read", &root),
            ));
        }
        return JjProbe::default();
    }

    let workspace_root = canonical_or_path(PathBuf::from(trimmed(&root.stdout)));
    let repository_path = match repository_path(&workspace_root) {
        Ok(path) => Some(path),
        Err(message) => {
            diagnostics.push(ContextDiagnostic::error("jj_metadata_invalid", message));
            None
        }
    };
    let names = match workspace_names(path) {
        Ok(names) => names,
        Err(message) => {
            diagnostics.push(ContextDiagnostic::error("jj_metadata_invalid", message));
            Vec::new()
        }
    };
    let current_names = names
        .iter()
        .filter_map(|(name, current)| (*current).then_some(name.clone()))
        .collect::<Vec<_>>();
    let workspace_name = match current_names.as_slice() {
        [name] => Some(name.clone()),
        _ => current_names.into_iter().find(|name| {
            workspace_root_for(path, name)
                .ok()
                .map(canonical_or_path)
                .is_some_and(|root| root == workspace_root)
        }),
    };
    let (primary_checkout, primary_workspace) =
        find_primary_workspace(path, &names, repository_path.as_deref(), diagnostics);
    let (git_backend_dir, git_backend_common_dir) = jj_git_backend(&workspace_root, diagnostics);

    JjProbe {
        found: true,
        context: JjContext {
            workspace_root: Some(workspace_root),
            workspace_name,
            repository_path,
            git_backend_dir,
            git_backend_common_dir,
            primary_checkout,
            primary_workspace,
        },
    }
}

fn discover_primary_from_jj(
    git: &GitContext,
    jj: &mut JjContext,
    diagnostics: &mut Vec<ContextDiagnostic>,
) {
    let Some(repository_path) = jj.repository_path.as_deref() else {
        return;
    };
    let Some(primary_root) = jj.primary_checkout.clone() else {
        return;
    };
    let primary_root = canonical_or_path(primary_root);
    if !same_repository(&primary_root, repository_path) {
        clear_primary(jj);
        diagnostics.push(ContextDiagnostic::warning(
            "jj_primary_unverified",
            format!(
                "JJ primary workspace at {} does not point to the current repository metadata",
                primary_root.display()
            ),
        ));
        return;
    }
    if let Some(common_dir) = git.common_dir.as_deref() {
        let primary_backend = jj_git_backend(&primary_root, diagnostics);
        if primary_backend.1.as_deref() != Some(common_dir) {
            clear_primary(jj);
            diagnostics.push(ContextDiagnostic::warning(
                "jj_primary_unverified",
                format!(
                    "JJ primary workspace at {} does not share Git common directory {}",
                    primary_root.display(),
                    common_dir.display()
                ),
            ));
        }
    }
}

fn clear_primary(jj: &mut JjContext) {
    jj.primary_checkout = None;
    jj.primary_workspace = None;
}

fn discover_primary_from_git(
    git: &GitContext,
    jj: &mut JjContext,
    diagnostics: &mut Vec<ContextDiagnostic>,
) {
    let Some(common_dir) = git.common_dir.as_deref() else {
        return;
    };
    let Some(candidate) = common_dir.parent() else {
        return;
    };
    let candidate = canonical_or_path(candidate.to_path_buf());
    if !candidate.is_dir() {
        return;
    }
    let candidate_jj = probe_jj(&candidate, diagnostics);
    if candidate_jj.context.workspace_root.as_deref() != Some(candidate.as_path()) {
        return;
    }
    let Some(primary_checkout) = candidate_jj.context.primary_checkout.as_deref() else {
        return;
    };
    let Some(primary_workspace) = candidate_jj.context.primary_workspace.as_deref() else {
        return;
    };
    let primary_backend = jj_git_backend(primary_checkout, diagnostics);
    if primary_backend.1.as_deref() != Some(common_dir) {
        return;
    }
    jj.primary_checkout = Some(primary_checkout.to_path_buf());
    jj.primary_workspace = Some(primary_workspace.to_owned());
}

fn find_primary_workspace(
    path: &Path,
    names: &[(String, bool)],
    expected_repository_path: Option<&Path>,
    diagnostics: &mut Vec<ContextDiagnostic>,
) -> (Option<PathBuf>, Option<String>) {
    for (name, _) in names {
        let Ok(root) = workspace_root_for(path, name) else {
            continue;
        };
        let root = canonical_or_path(root);
        let pointer = root.join(".jj/repo");
        let Ok(metadata) = fs::symlink_metadata(&pointer) else {
            continue;
        };
        if metadata.is_dir()
            && expected_repository_path
                .is_none_or(|expected| repository_path(&root).ok().as_deref() == Some(expected))
        {
            return (Some(root), Some(name.clone()));
        }
    }
    if !names.is_empty() {
        diagnostics.push(ContextDiagnostic::warning(
            "jj_primary_unverified",
            "could not identify a primary JJ workspace from repository pointers",
        ));
    }
    (None, None)
}

fn jj_git_backend(
    workspace_root: &Path,
    diagnostics: &mut Vec<ContextDiagnostic>,
) -> (Option<PathBuf>, Option<PathBuf>) {
    let output = match run_jj(workspace_root, ["git", "root"]) {
        Ok(output) => output,
        Err(_) => return (None, None),
    };
    if !output.status.success() {
        // A native JJ repository has no Git backend. That is valid and does not
        // make the JJ identity unusable.
        return (None, None);
    }
    let raw = trimmed(&output.stdout);
    if raw.is_empty() {
        diagnostics.push(ContextDiagnostic::warning(
            "jj_git_backend_invalid",
            "JJ Git backend query returned an empty path",
        ));
        return (None, None);
    }
    let git_dir = canonical_or_path(if Path::new(&raw).is_absolute() {
        PathBuf::from(raw)
    } else {
        workspace_root.join(raw)
    });
    let common_dir = match git_common_dir_for_backend(&git_dir) {
        Ok(path) => Some(path),
        Err(message) => {
            diagnostics.push(ContextDiagnostic::warning(
                "jj_git_backend_invalid",
                message,
            ));
            None
        }
    };
    (Some(git_dir), common_dir)
}

fn git_common_dir_for_backend(git_dir: &Path) -> Result<PathBuf, String> {
    let output = run_git_backend(
        git_dir,
        ["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .map_err(|error| format!("could not query Git backend: {error}"))?;
    if !output.status.success() {
        return Err(format_probe_failure(
            "Git backend common directory query failed",
            &output,
        ));
    }
    let common_dir = trimmed(&output.stdout);
    if common_dir.is_empty() {
        return Err("Git backend common directory query returned an empty path".to_owned());
    }
    Ok(canonical_or_path(PathBuf::from(common_dir)))
}

fn workspace_names(path: &Path) -> Result<Vec<(String, bool)>, String> {
    let output = run_jj(
        path,
        [
            "workspace",
            "list",
            "-T",
            r#"json(name) ++ "\t" ++ json(target.current_working_copy()) ++ "\n""#,
        ],
    )
    .map_err(|error| format!("could not list JJ workspaces: {}", error.error))?;
    if !output.status.success() {
        return Err(format_probe_failure("JJ workspace list failed", &output));
    }
    std::str::from_utf8(&output.stdout)
        .map_err(|error| format!("JJ workspace list was not valid UTF-8: {error}"))?
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let (name, current) = line
                .split_once('\t')
                .ok_or_else(|| format!("invalid JJ workspace record: {line:?}"))?;
            let name = serde_json::from_str(name)
                .map_err(|error| format!("invalid JJ workspace name: {error}"))?;
            let current = current
                .parse()
                .map_err(|error| format!("invalid JJ current-workspace flag: {error}"))?;
            Ok((name, current))
        })
        .collect()
}

fn workspace_root_for(path: &Path, name: &str) -> Result<PathBuf, String> {
    let output = run_jj(path, ["workspace", "root", "--name", name])
        .map_err(|error| format!("could not read JJ workspace root: {}", error.error))?;
    if !output.status.success() {
        return Err(format_probe_failure("JJ workspace root failed", &output));
    }
    let root = trimmed(&output.stdout);
    if root.is_empty() {
        return Err("JJ workspace root query returned an empty path".to_owned());
    }
    Ok(PathBuf::from(root))
}

fn repository_path(workspace_root: &Path) -> Result<PathBuf, String> {
    let pointer = workspace_root.join(".jj/repo");
    let metadata = fs::symlink_metadata(&pointer).map_err(|error| {
        format!(
            "could not inspect JJ repository pointer {}: {error}",
            pointer.display()
        )
    })?;
    if metadata.is_dir() {
        return Ok(canonical_or_path(pointer));
    }
    if !metadata.is_file() {
        return Err(format!(
            "JJ repository pointer is not a file or directory: {}",
            pointer.display()
        ));
    }
    let value = fs::read_to_string(&pointer).map_err(|error| {
        format!(
            "could not read JJ repository pointer {}: {error}",
            pointer.display()
        )
    })?;
    let value = value.trim();
    if value.is_empty() {
        return Err(format!(
            "JJ repository pointer is empty: {}",
            pointer.display()
        ));
    }
    let target = pointer.parent().expect(".jj/repo has a parent").join(value);
    if !target.is_dir() {
        return Err(format!(
            "JJ repository pointer target is not a directory: {}",
            target.display()
        ));
    }
    Ok(canonical_or_path(target))
}

fn same_repository(workspace_root: &Path, expected_repository_path: &Path) -> bool {
    repository_path(workspace_root)
        .ok()
        .is_some_and(|candidate| candidate == expected_repository_path)
}

fn run_git<I, S>(path: &Path, args: I) -> Result<Output, ProbeError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let mut command = Command::new("git");
    // Explicit PATH inspection must not follow a caller's Git-hook environment.
    for variable in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_INDEX_FILE",
    ] {
        command.env_remove(variable);
    }
    command
        .arg("--no-optional-locks")
        .arg("-C")
        .arg(path)
        .args(args);
    command.output().map_err(|error| ProbeError {
        program: "git",
        error,
    })
}

fn run_git_backend<I, S>(git_dir: &Path, args: I) -> Result<Output, ProbeError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let mut command = Command::new("git");
    // Explicit PATH inspection must not follow a caller's Git-hook environment.
    for variable in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_INDEX_FILE",
    ] {
        command.env_remove(variable);
    }
    command
        .arg("--no-optional-locks")
        .arg("--git-dir")
        .arg(git_dir)
        .args(args);
    command.output().map_err(|error| ProbeError {
        program: "git",
        error,
    })
}

fn run_jj<I, S>(path: &Path, args: I) -> Result<Output, ProbeError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let mut command = Command::new("jj");
    command
        .current_dir(path)
        .args([
            OsString::from("--no-pager"),
            OsString::from("--color=never"),
        ])
        .args([OsString::from("--at-operation"), OsString::from("@")])
        .args([OsString::from("--ignore-working-copy")])
        .args(args);
    command.output().map_err(|error| ProbeError {
        program: "jj",
        error,
    })
}

fn normalize_path(path: Option<&Path>) -> (PathBuf, Vec<ContextDiagnostic>) {
    let mut diagnostics = Vec::new();
    let supplied = path.map(Path::to_path_buf).unwrap_or_else(|| {
        env::current_dir().unwrap_or_else(|error| {
            diagnostics.push(ContextDiagnostic::error(
                "path_unavailable",
                format!("could not determine current directory: {error}"),
            ));
            PathBuf::from(".")
        })
    });
    let absolute = if supplied.is_absolute() {
        supplied
    } else {
        match env::current_dir() {
            Ok(current) => current.join(supplied),
            Err(error) => {
                diagnostics.push(ContextDiagnostic::error(
                    "path_unavailable",
                    format!("could not make path absolute: {error}"),
                ));
                supplied
            }
        }
    };
    (canonical_or_path(absolute), diagnostics)
}

fn canonical_or_path(path: PathBuf) -> PathBuf {
    fs::canonicalize(&path).unwrap_or(path)
}

fn has_git_metadata(path: &Path) -> bool {
    path.ancestors()
        .any(|ancestor| ancestor.join(".git").exists())
}

fn has_jj_metadata(path: &Path) -> bool {
    path.ancestors()
        .any(|ancestor| ancestor.join(".jj").exists())
}

fn nonempty_lines(bytes: &[u8]) -> Vec<&str> {
    std::str::from_utf8(bytes)
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect()
}

fn trimmed(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).trim().to_owned()
}

fn format_probe_failure(prefix: &str, output: &Output) -> String {
    let stderr = trimmed(&output.stderr);
    if stderr.is_empty() {
        format!("{prefix} (exit status {})", output.status)
    } else {
        format!("{prefix}: {stderr}")
    }
}

impl std::fmt::Display for ProbeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.program, self.error)
    }
}
