use anyhow::{Context, Result, anyhow, bail};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::Duration;

const PREVIOUS_WORKSPACE_FILE: &str = "jw-prev-workspace";
const JJ_RETRY_DELAY_MS: u64 = 50;
const JJ_MAX_ATTEMPTS: usize = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceEntry {
    pub name: String,
    pub root: Option<PathBuf>,
    pub is_current: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwitchResult {
    pub workspace: String,
    pub path: PathBuf,
    pub created: bool,
    pub bookmark: Option<String>,
    pub relative_subdir: Option<PathBuf>,
}

#[derive(Debug, Clone, Default)]
pub struct SwitchOptions {
    pub at_revset: Option<String>,
    pub bookmark: Option<String>,
    pub preserve_subdir: bool,
}

pub fn current_workspace_name() -> Result<String> {
    let output = run_jj(&[
        "workspace",
        "list",
        "-T",
        "if(target.current_working_copy(), name ++ \"\\n\", \"\")",
        "--color=never",
    ])?;
    let name = trimmed_stdout(output)?;
    if name.is_empty() {
        bail!("could not determine current workspace")
    }
    Ok(name)
}

pub fn workspace_entries() -> Result<Vec<WorkspaceEntry>> {
    let current = current_workspace_name().ok();
    let mut entries = Vec::new();

    for name in workspace_names()? {
        let root = workspace_root_by_name(&name).ok();
        entries.push(WorkspaceEntry {
            is_current: current.as_deref() == Some(name.as_str()),
            name,
            root,
        });
    }

    Ok(entries)
}

pub fn workspace_root_by_name(name: &str) -> Result<PathBuf> {
    let args = ["workspace", "root", "--name", name];
    let output =
        run_jj_raw(&args).with_context(|| "failed to execute `jj workspace root`".to_string())?;

    if output.status.success() {
        return Ok(PathBuf::from(trimmed_stdout(output)?));
    }

    if name == "default"
        && workspace_names()?
            .iter()
            .any(|candidate| candidate == "default")
    {
        let guessed = guessed_default_workspace_root()?;
        if guessed.is_dir() {
            return Ok(guessed);
        }
    }

    bail!(stderr_message(output, "workspace not found"));
}

pub fn workspace_exists(name: &str) -> Result<bool> {
    Ok(workspace_names()?.iter().any(|entry| entry == name))
}

pub fn resolve_workspace_token(token: &str) -> Result<String> {
    match token {
        "@" => current_workspace_name(),
        "-" => previous_workspace_name(),
        "^" | "default" => default_workspace_name(),
        other => Ok(other.to_owned()),
    }
}

pub fn default_workspace_name() -> Result<String> {
    if workspace_exists("default")? {
        return Ok("default".to_owned());
    }

    let guessed_root = guessed_default_workspace_root()?;
    let current_root = workspace_root_current()?;

    if canonicalize_dir(&current_root)? == canonicalize_dir(&guessed_root)? {
        current_workspace_name()
    } else {
        bail!("could not determine default workspace")
    }
}

pub fn workspace_root_current() -> Result<PathBuf> {
    let output = run_jj(&["workspace", "root"])?;
    Ok(PathBuf::from(trimmed_stdout(output)?))
}

pub fn switch_workspace(target: &str, options: &SwitchOptions) -> Result<SwitchResult> {
    let current_name = current_workspace_name()?;
    let current_root = workspace_root_current()?;
    let current_dir = env::current_dir().context("failed to determine current directory")?;
    let relative_subdir = if options.preserve_subdir {
        current_dir
            .strip_prefix(&current_root)
            .ok()
            .map(Path::to_path_buf)
    } else {
        None
    };

    let resolved_name = resolve_workspace_token(target)?;
    let mut created = false;

    let target_path = if workspace_exists(&resolved_name)? {
        workspace_root_by_name(&resolved_name)?
    } else {
        validate_workspace_name(&resolved_name)?;
        let path = workspace_dir_for_name(&resolved_name)?;
        if path.exists() {
            bail!("directory already exists: {}", path.display());
        }

        let mut args = vec![
            "workspace".to_owned(),
            "add".to_owned(),
            "--name".to_owned(),
            resolved_name.clone(),
        ];

        if let Some(revset) = &options.at_revset {
            args.push("--revision".to_owned());
            args.push(revset.clone());
        }

        args.push(path.display().to_string());
        run_jj_owned(&args)?;
        created = true;

        if let Some(bookmark) = &options.bookmark {
            let output = Command::new("jj")
                .current_dir(&path)
                .args(["bookmark", "create", bookmark, "-r", "@"])
                .output()
                .with_context(|| "failed to create bookmark".to_string())?;
            if !output.status.success() {
                bail!(stderr_message(output, "failed to create bookmark"));
            }
        }

        path
    };

    remember_previous_workspace(&current_name, &current_root, &resolved_name, &target_path)?;

    Ok(SwitchResult {
        workspace: resolved_name,
        path: target_path,
        created,
        bookmark: options.bookmark.clone(),
        relative_subdir,
    })
}

pub fn default_workspace_root() -> Result<PathBuf> {
    workspace_root_by_name(&default_workspace_name()?)
}

pub fn path_for_workspace(token: &str) -> Result<PathBuf> {
    let name = resolve_workspace_token(token)?;
    workspace_root_by_name(&name)
}

pub fn remove_workspace(token: Option<&str>, delete_dir: bool) -> Result<(String, PathBuf)> {
    let name = match token {
        Some(value) => resolve_workspace_token(value)?,
        None => current_workspace_name()?,
    };

    if name == "default" {
        bail!("refusing to remove workspace named 'default'")
    }

    let path = workspace_root_by_name(&name)?;
    if delete_dir && name == current_workspace_name()? {
        bail!("cannot delete the current workspace directory; switch away first")
    }

    run_jj(&["workspace", "forget", &name])?;

    if delete_dir && path.is_dir() {
        fs::remove_dir_all(&path)
            .with_context(|| format!("failed to delete workspace directory {}", path.display()))?;
    }

    Ok((name, path))
}

pub fn prune_missing_workspaces() -> Result<Vec<String>> {
    let mut removed = Vec::new();

    for entry in workspace_entries()? {
        match &entry.root {
            Some(path) if path.is_dir() => {}
            _ => {
                run_jj(&["workspace", "forget", &entry.name])?;
                removed.push(entry.name);
            }
        }
    }

    Ok(removed)
}

pub fn previous_workspace_name() -> Result<String> {
    let root = workspace_root_current()?;
    let state_path = workspace_state_file(&root);
    let contents =
        fs::read_to_string(&state_path).with_context(|| "no previous workspace recorded")?;
    let name = contents.trim();
    if name.is_empty() {
        bail!("no previous workspace recorded")
    }
    if workspace_exists(name)? {
        Ok(name.to_owned())
    } else {
        bail!("no previous workspace recorded")
    }
}

pub fn completion_workspace_candidates() -> Result<Vec<(String, String)>> {
    let current = current_workspace_name().ok();
    let previous = previous_workspace_name().ok();
    let default = default_workspace_name().ok();

    let mut candidates = Vec::new();

    for entry in workspace_entries()? {
        let description = if current.as_deref() == Some(entry.name.as_str()) {
            "Existing workspace (current)"
        } else if previous.as_deref() == Some(entry.name.as_str()) {
            "Existing workspace (previous)"
        } else if default.as_deref() == Some(entry.name.as_str()) {
            "Existing workspace (default)"
        } else {
            "Existing workspace"
        };
        candidates.push((entry.name, description.to_owned()));
    }

    candidates.push(("@".to_owned(), "Current workspace".to_owned()));
    candidates.push(("-".to_owned(), "Previous workspace".to_owned()));
    candidates.push(("^".to_owned(), "Default workspace".to_owned()));

    Ok(candidates)
}

fn remember_previous_workspace(
    from_name: &str,
    from_root: &Path,
    to_name: &str,
    to_root: &Path,
) -> Result<()> {
    if from_name == to_name {
        return Ok(());
    }

    fs::write(workspace_state_file(from_root), format!("{to_name}\n"))
        .with_context(|| "failed to record previous workspace")?;

    let to_state_dir = to_root.join(".jj");
    if to_state_dir.is_dir() {
        fs::write(workspace_state_file(to_root), format!("{from_name}\n"))
            .with_context(|| "failed to record target previous workspace")?;
    }

    Ok(())
}

fn workspace_state_file(root: &Path) -> PathBuf {
    root.join(".jj").join(PREVIOUS_WORKSPACE_FILE)
}

fn guessed_default_workspace_root() -> Result<PathBuf> {
    workspace_dir_for_name("default")
}

fn workspace_dir_for_name(name: &str) -> Result<PathBuf> {
    let current_root = workspace_root_current()?;
    let parent = current_root
        .parent()
        .ok_or_else(|| anyhow!("workspace root has no parent directory"))?;
    let base_name = workspace_base_name()?;

    if name == "default" {
        Ok(parent.join(base_name))
    } else {
        Ok(parent.join(format!("{base_name}.{name}")))
    }
}

fn workspace_base_name() -> Result<String> {
    let current_root = workspace_root_current()?;
    let current_name = current_workspace_name()?;
    let mut base = current_root
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow!("workspace root has no valid basename"))?
        .to_owned();

    let suffix = format!(".{current_name}");
    if current_name != "default" && base.ends_with(&suffix) {
        let new_len = base.len() - suffix.len();
        base.truncate(new_len);
    } else if current_name != "default" && base == current_name && base.contains('.') {
        if let Some((prefix, _)) = base.rsplit_once('.') {
            base = prefix.to_owned();
        }
    }

    Ok(base)
}

fn validate_workspace_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("workspace name cannot be empty")
    }
    if name.contains('\n') {
        bail!("workspace name cannot contain newlines")
    }
    if name.contains("..") {
        bail!("workspace name cannot contain '..'")
    }
    if name.starts_with('/') {
        bail!("workspace name cannot start with '/'")
    }
    if name.starts_with('-') {
        bail!("workspace name cannot start with '-'")
    }
    if name.contains(':') {
        bail!("workspace name cannot contain ':'")
    }
    Ok(())
}

fn workspace_names() -> Result<Vec<String>> {
    let output = run_jj(&[
        "workspace",
        "list",
        "-T",
        "name ++ \"\\n\"",
        "--color=never",
    ])?;

    Ok(trimmed_stdout(output)?
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .collect())
}

fn canonicalize_dir(path: &Path) -> Result<PathBuf> {
    path.canonicalize()
        .with_context(|| format!("failed to resolve {}", path.display()))
}

fn run_jj(args: &[&str]) -> Result<std::process::Output> {
    let output =
        run_jj_raw(args).with_context(|| format!("failed to execute `jj {}`", args.join(" ")))?;

    if output.status.success() {
        Ok(output)
    } else {
        bail!(stderr_message(output, "jj command failed"))
    }
}

fn run_jj_owned(args: &[String]) -> Result<std::process::Output> {
    let borrowed = args.iter().map(String::as_str).collect::<Vec<_>>();
    let output = run_jj_raw(&borrowed)
        .with_context(|| format!("failed to execute `jj {}`", args.join(" ")))?;

    if output.status.success() {
        Ok(output)
    } else {
        bail!(stderr_message(output, "jj command failed"))
    }
}

fn run_jj_raw(args: &[&str]) -> Result<std::process::Output> {
    let mut last_error: Option<std::io::Error> = None;
    let mut last_output: Option<std::process::Output> = None;

    for attempt in 1..=JJ_MAX_ATTEMPTS {
        match Command::new("jj").args(args).output() {
            Ok(output) => {
                if output.status.success() || !is_transient_jj_failure(&output) {
                    return Ok(output);
                }
                last_output = Some(output);
                if attempt < JJ_MAX_ATTEMPTS {
                    thread::sleep(Duration::from_millis(JJ_RETRY_DELAY_MS));
                }
            }
            Err(err) => {
                last_error = Some(err);
                if attempt < JJ_MAX_ATTEMPTS {
                    thread::sleep(Duration::from_millis(JJ_RETRY_DELAY_MS));
                }
            }
        }
    }

    if let Some(err) = last_error {
        return Err(err).context("failed to execute `jj`");
    }

    last_output.ok_or_else(|| anyhow!("failed to execute `jj {}`", args.join(" ")))
}

fn is_transient_jj_failure(output: &std::process::Output) -> bool {
    let message = String::from_utf8_lossy(&output.stderr).to_lowercase();
    message.contains("temporarily unavailable")
        || message.contains("timed out")
        || message.contains("resource busy")
        || message.contains("would block")
        || message.contains("lock")
}

fn trimmed_stdout(output: std::process::Output) -> Result<String> {
    String::from_utf8(output.stdout)
        .context("jj output was not valid UTF-8")
        .map(|value| value.trim().to_owned())
}

fn stderr_message(output: std::process::Output, fallback: &str) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    if stderr.is_empty() {
        fallback.to_owned()
    } else {
        stderr
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_workspace_names() {
        for valid in ["feature", "feature-1", "feature_branch", "a.b.c", "default"] {
            assert!(validate_workspace_name(valid).is_ok(), "{valid}");
        }
        assert!(validate_workspace_name("").is_err());
        assert!(validate_workspace_name("../bad").is_err());
        assert!(validate_workspace_name("-bad").is_err());
        assert!(validate_workspace_name("bad:name").is_err());
        assert!(validate_workspace_name("bad\nname").is_err());
        assert!(validate_workspace_name("/bad").is_err());
    }
}
