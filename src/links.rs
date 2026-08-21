use anyhow::{Context, Error, Result, anyhow, bail};
use serde::Deserialize;
use std::collections::HashSet;
use std::fs;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

const LINKS_FILE: &str = ".jwlinks.toml";
const LINKS_LOCAL_FILE: &str = ".jwlinks.local.toml";

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct LinkApplyReport {
    pub linked: usize,
    pub satisfied: usize,
    pub skipped_missing_target: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LinkCheckState {
    Satisfied,
    Missing,
    Skipped,
    Conflicting,
    Unreadable(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LinkCheck {
    pub(crate) source: PathBuf,
    pub(crate) target: PathBuf,
    pub(crate) required: bool,
    pub(crate) state: LinkCheckState,
    pub(crate) target_exists: bool,
}

#[derive(Debug)]
pub(crate) struct LinkApplication {
    report: LinkApplyReport,
    created_links: Vec<CreatedLink>,
    created_directories: Vec<PathBuf>,
}

impl LinkApplication {
    pub(crate) fn into_report(self) -> LinkApplyReport {
        self.report
    }

    pub(crate) fn rollback(self) -> Result<()> {
        rollback(&self.created_links, &self.created_directories)
    }
}

impl LinkApplyReport {
    pub fn has_entries(&self) -> bool {
        self.linked > 0 || self.satisfied > 0 || self.skipped_missing_target > 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LinkRule {
    workspace_root: PathBuf,
    source: PathBuf,
    target: PathBuf,
    required: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CreatedLink {
    source: PathBuf,
    target: PathBuf,
}

#[derive(Debug)]
struct LinkPlan {
    links_to_create: Vec<LinkRule>,
    directories_to_create: Vec<PathBuf>,
    report: LinkApplyReport,
}

#[derive(Debug, Deserialize)]
struct LinksFile {
    #[serde(default)]
    link: Vec<LinkRuleRaw>,
}

#[derive(Debug, Clone)]
pub(crate) struct LinkConfig {
    rules: Vec<LinkRuleRaw>,
}

#[derive(Debug, Deserialize, Clone)]
struct LinkRuleRaw {
    source: String,
    target: String,
    #[serde(default)]
    required: bool,
}

/// Apply link rules owned by `config_root` to `workspace_root`.
///
/// Every rule is validated and preflighted before any directory or symlink is created.
pub fn apply_workspace_links(config_root: &Path, workspace_root: &Path) -> Result<LinkApplyReport> {
    apply_workspace_links_reversible(config_root, workspace_root).map(LinkApplication::into_report)
}

pub(crate) fn apply_workspace_links_reversible(
    config_root: &Path,
    workspace_root: &Path,
) -> Result<LinkApplication> {
    let config = load_link_config(config_root)?;
    let rules = config
        .as_ref()
        .map(|config| normalize_rules(config, workspace_root))
        .transpose()?
        .unwrap_or_default();
    LinkPlan::build(workspace_root, rules)?.apply()
}

/// Read and merge the repository-owned link configuration once.
///
/// The local file replaces a base-file entry with the same raw `source`, matching the
/// long-standing override behavior. Targets are intentionally left unresolved here because
/// relative targets are relative to each receiving workspace.
pub(crate) fn load_link_config(config_root: &Path) -> Result<Option<LinkConfig>> {
    let mut combined: Vec<LinkRuleRaw> = Vec::new();
    let mut found = false;

    for file_name in [LINKS_FILE, LINKS_LOCAL_FILE] {
        let path = config_root.join(file_name);
        match fs::symlink_metadata(&path) {
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("failed to inspect {}", path.display()));
            }
        }
        found = true;

        let contents = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let parsed: LinksFile = toml::from_str(&contents)
            .with_context(|| format!("failed to parse {}", path.display()))?;

        for rule in parsed.link {
            if let Some(existing) = combined
                .iter_mut()
                .find(|entry| entry.source == rule.source)
            {
                *existing = rule;
            } else {
                combined.push(rule);
            }
        }
    }

    Ok(found.then_some(LinkConfig { rules: combined }))
}

pub(crate) fn inspect_loaded_workspace_links(
    config: &LinkConfig,
    workspace_root: &Path,
) -> Vec<LinkCheck> {
    config
        .rules
        .iter()
        .map(|raw| match normalize_rule(workspace_root, raw.clone()) {
            Ok(rule) => classify_rule(&rule),
            Err(error) => {
                invalid_rule_check(workspace_root, raw, format!("invalid link rule: {error:#}"))
            }
        })
        .collect()
}

fn normalize_rules(config: &LinkConfig, workspace_root: &Path) -> Result<Vec<LinkRule>> {
    config
        .rules
        .iter()
        .cloned()
        .map(|raw| normalize_rule(workspace_root, raw))
        .collect()
}

fn normalize_rule(workspace_root: &Path, raw: LinkRuleRaw) -> Result<LinkRule> {
    let source_raw = PathBuf::from(raw.source.trim());
    if source_raw.as_os_str().is_empty() {
        bail!("link source cannot be empty")
    }

    let mut source_rel = PathBuf::new();
    for component in source_raw.components() {
        match component {
            Component::Normal(part) => source_rel.push(part),
            Component::CurDir => {}
            Component::ParentDir => bail!(
                "link source cannot contain parent traversal: {}",
                source_raw.display()
            ),
            Component::Prefix(_) | Component::RootDir => {
                bail!("link source must be relative: {}", source_raw.display())
            }
        }
    }
    if source_rel.as_os_str().is_empty() {
        bail!("link source must name a path inside the workspace")
    }

    let target_raw = PathBuf::from(raw.target.trim());
    if target_raw.as_os_str().is_empty() {
        bail!("link target cannot be empty")
    }
    let target = if target_raw.is_absolute() {
        target_raw
    } else {
        workspace_root.join(target_raw)
    };

    Ok(LinkRule {
        workspace_root: workspace_root.to_path_buf(),
        source: workspace_root.join(source_rel),
        target,
        required: raw.required,
    })
}

impl LinkPlan {
    fn build(workspace_root: &Path, rules: Vec<LinkRule>) -> Result<Self> {
        if !workspace_root.is_dir() {
            bail!(
                "workspace root is not a directory: {}",
                workspace_root.display()
            )
        }

        let mut links_to_create = Vec::new();
        let mut skipped_sources = Vec::new();
        let mut report = LinkApplyReport::default();

        for rule in rules {
            let check = classify_rule(&rule);
            match check.state {
                LinkCheckState::Satisfied => report.satisfied += 1,
                LinkCheckState::Skipped => {
                    report.skipped_missing_target += 1;
                    skipped_sources.push(check.source.clone());
                }
                LinkCheckState::Missing if check.target_exists => links_to_create.push(rule),
                LinkCheckState::Missing => {
                    bail!(
                        "required link target is missing for {}: {}",
                        display_in_workspace(workspace_root, &check.source),
                        check.target.display()
                    )
                }
                LinkCheckState::Conflicting => {
                    let source_kind = match inspect_source_parents(&check.source, workspace_root) {
                        Err(SourceParentIssue::Conflict(reason)) => reason,
                        _ => match fs::symlink_metadata(&check.source) {
                            Ok(metadata) if metadata.file_type().is_symlink() => {
                                "existing symlink does not point to".to_owned()
                            }
                            _ => "path exists and is not a symlink to".to_owned(),
                        },
                    };
                    bail!(
                        "link conflict at {}: {source_kind} {}",
                        display_in_workspace(workspace_root, &check.source),
                        check.target.display()
                    )
                }
                LinkCheckState::Unreadable(error) => {
                    bail!(
                        "link is unreadable at {}: {error}",
                        display_in_workspace(workspace_root, &check.source),
                    )
                }
            }
        }

        let directories_to_create =
            preflight_creation_paths(workspace_root, &links_to_create, &skipped_sources)?;
        report.linked = links_to_create.len();

        Ok(Self {
            links_to_create,
            directories_to_create,
            report,
        })
    }

    fn apply(self) -> Result<LinkApplication> {
        let mut created_directories = Vec::new();
        let mut created_links = Vec::new();

        for directory in &self.directories_to_create {
            match create_planned_directory(directory) {
                Ok(true) => created_directories.push(directory.clone()),
                Ok(false) => {}
                Err(error) => {
                    return Err(error_after_rollback(
                        error,
                        &created_links,
                        &created_directories,
                    ));
                }
            }
        }

        for rule in &self.links_to_create {
            if let Err(error) = create_symlink(&rule.target, &rule.source) {
                return Err(error_after_rollback(
                    error,
                    &created_links,
                    &created_directories,
                ));
            }
            created_links.push(CreatedLink {
                source: rule.source.clone(),
                target: rule.target.clone(),
            });
        }

        Ok(LinkApplication {
            report: self.report,
            created_links,
            created_directories,
        })
    }
}

fn classify_rule(rule: &LinkRule) -> LinkCheck {
    if let Err(issue) = inspect_source_parents(&rule.source, &rule.workspace_root) {
        let state = match issue {
            SourceParentIssue::Conflict(_) => LinkCheckState::Conflicting,
            SourceParentIssue::Unreadable(error) => LinkCheckState::Unreadable(error),
        };
        return LinkCheck {
            source: rule.source.clone(),
            target: rule.target.clone(),
            required: rule.required,
            state,
            target_exists: false,
        };
    }

    let target_exists = match fs::metadata(&rule.target) {
        Ok(_) => true,
        Err(error) if error.kind() == ErrorKind::NotFound => false,
        Err(error) => {
            return unreadable_check(
                rule,
                format!(
                    "failed to inspect target {}: {error}",
                    rule.target.display()
                ),
            );
        }
    };

    let state = match fs::symlink_metadata(&rule.source) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            let existing = match fs::read_link(&rule.source) {
                Ok(existing) => existing,
                Err(error) => {
                    return unreadable_check(
                        rule,
                        format!("failed to read symlink {}: {error}", rule.source.display()),
                    );
                }
            };
            let existing_abs = if existing.is_absolute() {
                existing
            } else {
                rule.source
                    .parent()
                    .unwrap_or_else(|| Path::new("."))
                    .join(existing)
            };
            match same_existing_path(&existing_abs, &rule.target) {
                Ok(true) if target_exists => LinkCheckState::Satisfied,
                Ok(true) if rule.required => LinkCheckState::Missing,
                Ok(true) => LinkCheckState::Skipped,
                Ok(false) => LinkCheckState::Conflicting,
                Err(error) => LinkCheckState::Unreadable(format!(
                    "failed to compare {} with {}: {error:#}",
                    existing_abs.display(),
                    rule.target.display()
                )),
            }
        }
        Ok(_) if target_exists => match same_existing_path(&rule.source, &rule.target) {
            Ok(true) => LinkCheckState::Satisfied,
            Ok(false) => LinkCheckState::Conflicting,
            Err(error) => LinkCheckState::Unreadable(format!(
                "failed to compare {} with {}: {error:#}",
                rule.source.display(),
                rule.target.display()
            )),
        },
        Ok(_) => LinkCheckState::Conflicting,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            if !target_exists && !rule.required {
                LinkCheckState::Skipped
            } else {
                LinkCheckState::Missing
            }
        }
        Err(error) => LinkCheckState::Unreadable(format!(
            "failed to inspect source {}: {error}",
            rule.source.display()
        )),
    };

    LinkCheck {
        source: rule.source.clone(),
        target: rule.target.clone(),
        required: rule.required,
        state,
        target_exists,
    }
}

fn invalid_rule_check(workspace_root: &Path, raw: &LinkRuleRaw, error: String) -> LinkCheck {
    let source_raw = PathBuf::from(raw.source.trim());
    let target_raw = PathBuf::from(raw.target.trim());
    LinkCheck {
        source: if source_raw.is_absolute() {
            source_raw
        } else {
            workspace_root.join(source_raw)
        },
        target: if target_raw.is_absolute() {
            target_raw
        } else {
            workspace_root.join(target_raw)
        },
        required: raw.required,
        state: LinkCheckState::Unreadable(error),
        target_exists: false,
    }
}

fn unreadable_check(rule: &LinkRule, error: String) -> LinkCheck {
    LinkCheck {
        source: rule.source.clone(),
        target: rule.target.clone(),
        required: rule.required,
        state: LinkCheckState::Unreadable(error),
        target_exists: false,
    }
}

#[derive(Debug)]
enum SourceParentIssue {
    Conflict(String),
    Unreadable(String),
}

fn inspect_source_parents(
    source: &Path,
    workspace_root: &Path,
) -> std::result::Result<(), SourceParentIssue> {
    let Some(parent) = source.parent() else {
        return Ok(());
    };

    let relative_parent = parent.strip_prefix(workspace_root).map_err(|error| {
        SourceParentIssue::Unreadable(format!(
            "source parent {} is outside workspace {}: {error}",
            parent.display(),
            workspace_root.display()
        ))
    })?;
    let mut candidate = workspace_root.to_path_buf();
    for component in relative_parent.components() {
        candidate.push(component.as_os_str());
        match fs::symlink_metadata(&candidate) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(SourceParentIssue::Conflict(format!(
                    "source parent cannot be a symlink: {}",
                    candidate.display()
                )));
            }
            Ok(metadata) if !metadata.is_dir() => {
                return Err(SourceParentIssue::Conflict(format!(
                    "source parent is not a directory: {}",
                    candidate.display()
                )));
            }
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(SourceParentIssue::Unreadable(format!(
                    "failed to inspect source parent {}: {error}",
                    candidate.display()
                )));
            }
        }
    }
    Ok(())
}

fn preflight_creation_paths(
    workspace_root: &Path,
    links_to_create: &[LinkRule],
    skipped_sources: &[PathBuf],
) -> Result<Vec<PathBuf>> {
    let mut sources = links_to_create
        .iter()
        .map(|rule| rule.source.as_path())
        .collect::<Vec<_>>();
    sources.sort_unstable();

    for pair in sources.windows(2) {
        if paths_overlap(pair[0], pair[1]) {
            bail!(
                "link sources overlap: {} and {}",
                display_in_workspace(workspace_root, pair[0]),
                display_in_workspace(workspace_root, pair[1])
            )
        }
    }

    for skipped in skipped_sources {
        for planned in &sources {
            if paths_overlap(skipped, planned) {
                bail!(
                    "link sources overlap: {} and {}",
                    display_in_workspace(workspace_root, skipped),
                    display_in_workspace(workspace_root, planned)
                )
            }
        }
    }

    let mut missing_directories = HashSet::new();
    for rule in links_to_create {
        let parent = rule
            .source
            .parent()
            .expect("validated link sources have a parent");
        let relative_parent = parent.strip_prefix(workspace_root).with_context(|| {
            format!(
                "link source escapes workspace: {}",
                display_in_workspace(workspace_root, &rule.source)
            )
        })?;
        let mut candidate = workspace_root.to_path_buf();

        for component in relative_parent.components() {
            candidate.push(component.as_os_str());
            match fs::symlink_metadata(&candidate) {
                Ok(metadata) if metadata.file_type().is_symlink() => bail!(
                    "link source parent cannot be a symlink: {}",
                    display_in_workspace(workspace_root, &candidate)
                ),
                Ok(metadata) if metadata.is_dir() => {}
                Ok(_) => bail!(
                    "link source parent is not a directory: {}",
                    display_in_workspace(workspace_root, &candidate)
                ),
                Err(error) if error.kind() == ErrorKind::NotFound => {
                    missing_directories.insert(candidate.clone());
                }
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("failed to inspect {}", candidate.display()));
                }
            }
        }
    }

    let mut directories = missing_directories.into_iter().collect::<Vec<_>>();
    directories.sort_by(|left, right| {
        left.components()
            .count()
            .cmp(&right.components().count())
            .then_with(|| left.cmp(right))
    });
    Ok(directories)
}

fn paths_overlap(left: &Path, right: &Path) -> bool {
    left == right || left.starts_with(right) || right.starts_with(left)
}

fn create_planned_directory(path: &Path) -> Result<bool> {
    match fs::create_dir(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {
            let metadata = fs::symlink_metadata(path)
                .with_context(|| format!("failed to inspect {}", path.display()))?;
            if metadata.is_dir() && !metadata.file_type().is_symlink() {
                Ok(false)
            } else {
                bail!("link source parent is not a directory: {}", path.display())
            }
        }
        Err(error) => Err(error).with_context(|| format!("failed to create {}", path.display())),
    }
}

fn error_after_rollback(
    error: Error,
    created_links: &[CreatedLink],
    created_directories: &[PathBuf],
) -> Error {
    match rollback(created_links, created_directories) {
        Ok(()) => error,
        Err(rollback_error) => {
            anyhow!("{error:#}; rollback incomplete, manual cleanup required: {rollback_error:#}")
        }
    }
}

fn rollback(created_links: &[CreatedLink], created_directories: &[PathBuf]) -> Result<()> {
    let mut failures = Vec::new();

    for link in created_links.iter().rev() {
        if let Err(error) = remove_created_symlink(link) {
            failures.push(format!(
                "failed to remove {}: {error}",
                link.source.display()
            ));
        }
    }
    for directory in created_directories.iter().rev() {
        if let Err(error) = fs::remove_dir(directory) {
            failures.push(format!(
                "failed to remove directory {}: {error}",
                directory.display()
            ));
        }
    }

    if failures.is_empty() {
        Ok(())
    } else {
        bail!(failures.join("; "))
    }
}

fn remove_created_symlink(link: &CreatedLink) -> std::io::Result<()> {
    let metadata = fs::symlink_metadata(&link.source)?;
    if !metadata.file_type().is_symlink() {
        return Err(std::io::Error::new(
            ErrorKind::InvalidData,
            "path is no longer the symlink created by jw",
        ));
    }
    let target = fs::read_link(&link.source)?;
    if target != link.target {
        return Err(std::io::Error::new(
            ErrorKind::InvalidData,
            "symlink target changed after jw created it",
        ));
    }
    remove_symlink(&link.source)
}

fn same_existing_path(path_a: &Path, path_b: &Path) -> Result<bool> {
    Ok(resolve_existing_prefix(path_a)? == resolve_existing_prefix(path_b)?)
}

fn resolve_existing_prefix(path: &Path) -> Result<PathBuf> {
    let mut prefix = path.to_path_buf();
    let mut missing_suffix = Vec::new();

    loop {
        match fs::canonicalize(&prefix) {
            Ok(existing) => {
                let mut resolved = existing;
                for component in missing_suffix.iter().rev() {
                    resolved.push(component);
                }
                return Ok(normalize_lexical(&resolved));
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {
                let Some(name) = prefix.file_name() else {
                    return Ok(normalize_lexical(path));
                };
                missing_suffix.push(name.to_owned());
                let Some(parent) = prefix.parent() else {
                    return Ok(normalize_lexical(path));
                };
                prefix = parent.to_path_buf();
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to resolve {}", prefix.display()));
            }
        }
    }
}

fn normalize_lexical(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                let can_pop_normal = normalized
                    .components()
                    .next_back()
                    .is_some_and(|component| matches!(component, Component::Normal(_)));
                if can_pop_normal {
                    normalized.pop();
                } else if !path.is_absolute() {
                    normalized.push(component.as_os_str());
                }
            }
            Component::Normal(part) => normalized.push(part),
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

fn display_in_workspace(workspace_root: &Path, path: &Path) -> String {
    path.strip_prefix(workspace_root)
        .map(|rel| rel.display().to_string())
        .unwrap_or_else(|_| path.display().to_string())
}

#[cfg(unix)]
fn create_symlink(target: &Path, source: &Path) -> Result<()> {
    std::os::unix::fs::symlink(target, source).with_context(|| {
        format!(
            "failed to create symlink {} -> {}",
            source.display(),
            target.display()
        )
    })
}

#[cfg(unix)]
fn remove_symlink(path: &Path) -> std::io::Result<()> {
    fs::remove_file(path)
}

#[cfg(windows)]
fn create_symlink(target: &Path, source: &Path) -> Result<()> {
    let link_result = if target.exists() && target.is_dir() {
        std::os::windows::fs::symlink_dir(target, source)
    } else {
        std::os::windows::fs::symlink_file(target, source)
    };

    link_result.with_context(|| {
        format!(
            "failed to create symlink {} -> {}",
            source.display(),
            target.display()
        )
    })
}

#[cfg(windows)]
fn remove_symlink(path: &Path) -> std::io::Result<()> {
    if path.is_dir() {
        fs::remove_dir(path)
    } else {
        fs::remove_file(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_lexical_clamps_parent_traversal_at_absolute_root() {
        assert_eq!(
            normalize_lexical(Path::new("/../../target")),
            PathBuf::from("/target")
        );
        assert_eq!(
            normalize_lexical(Path::new("/foo/../../target")),
            PathBuf::from("/target")
        );
    }

    #[cfg(unix)]
    #[test]
    fn apply_rolls_back_links_and_directories_after_a_late_conflict() {
        let tempdir = tempfile::tempdir().expect("create temp directory");
        let workspace_root = tempdir.path().join("repo");
        let target = workspace_root.join("target");
        fs::create_dir_all(&target).expect("create target");

        let created_source = workspace_root.join("nested/created");
        let late_conflict = workspace_root.join("late-conflict");
        let rules = vec![
            LinkRule {
                workspace_root: workspace_root.clone(),
                source: created_source.clone(),
                target: target.clone(),
                required: true,
            },
            LinkRule {
                workspace_root: workspace_root.clone(),
                source: late_conflict.clone(),
                target,
                required: true,
            },
        ];
        let plan = LinkPlan::build(&workspace_root, rules).expect("build valid plan");

        fs::write(&late_conflict, "claimed after preflight").expect("create late conflict");
        let error = plan.apply().expect_err("late conflict must fail");

        assert!(error.to_string().contains("failed to create symlink"));
        assert!(!created_source.exists());
        assert!(!workspace_root.join("nested").exists());
        assert_eq!(
            fs::read_to_string(late_conflict).expect("read late conflict"),
            "claimed after preflight"
        );
    }

    #[test]
    fn rollback_failure_reports_manual_cleanup_and_preserves_unowned_path() {
        let tempdir = tempfile::tempdir().expect("create temp directory");
        let replaced_link = tempdir.path().join("replaced-link");
        fs::write(&replaced_link, "not jw's link").expect("create replacement file");
        let created_link = CreatedLink {
            source: replaced_link.clone(),
            target: tempdir.path().join("expected-target"),
        };

        let error = error_after_rollback(
            anyhow!("link creation failed"),
            std::slice::from_ref(&created_link),
            &[],
        );
        let message = error.to_string();

        assert!(message.contains("link creation failed"));
        assert!(message.contains("rollback incomplete, manual cleanup required"));
        assert!(message.contains("path is no longer the symlink created by jw"));
        assert_eq!(
            fs::read_to_string(replaced_link).expect("read replacement file"),
            "not jw's link"
        );
    }

    #[cfg(unix)]
    #[test]
    fn rollback_preserves_symlink_replaced_after_creation() {
        let tempdir = tempfile::tempdir().expect("create temp directory");
        let source = tempdir.path().join("link");
        let expected_target = tempdir.path().join("expected");
        let replacement_target = tempdir.path().join("replacement");
        fs::create_dir_all(&expected_target).expect("create expected target");
        fs::create_dir_all(&replacement_target).expect("create replacement target");
        create_symlink(&expected_target, &source).expect("create original link");
        let created_link = CreatedLink {
            source: source.clone(),
            target: expected_target,
        };

        remove_symlink(&source).expect("remove original link");
        create_symlink(&replacement_target, &source).expect("create replacement link");

        let error = rollback(std::slice::from_ref(&created_link), &[])
            .expect_err("changed link must not be removed");
        assert!(error.to_string().contains("symlink target changed"));
        assert_eq!(
            fs::read_link(&source).expect("read replacement link"),
            replacement_target
        );
    }
}
