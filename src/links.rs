use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};

const LINKS_FILE: &str = ".jwlinks.toml";
const LINKS_LOCAL_FILE: &str = ".jwlinks.local.toml";

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct LinkApplyReport {
    pub linked: usize,
    pub satisfied: usize,
    pub skipped_missing_target: usize,
}

impl LinkApplyReport {
    pub fn has_entries(&self) -> bool {
        self.linked > 0 || self.satisfied > 0 || self.skipped_missing_target > 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LinkRule {
    source: PathBuf,
    target: PathBuf,
    required: bool,
}

#[derive(Debug, Deserialize)]
struct LinksFile {
    #[serde(default)]
    link: Vec<LinkRuleRaw>,
}

#[derive(Debug, Deserialize, Clone)]
struct LinkRuleRaw {
    source: String,
    target: String,
    #[serde(default)]
    required: bool,
}

pub fn apply_workspace_links(workspace_root: &Path) -> Result<LinkApplyReport> {
    let rules = load_rules(workspace_root)?;
    let mut report = LinkApplyReport::default();

    for rule in rules {
        apply_rule(workspace_root, &rule, &mut report)?;
    }

    Ok(report)
}

fn load_rules(workspace_root: &Path) -> Result<Vec<LinkRule>> {
    let mut combined: Vec<LinkRuleRaw> = Vec::new();

    for file_name in [LINKS_FILE, LINKS_LOCAL_FILE] {
        let path = workspace_root.join(file_name);
        if !path.exists() {
            continue;
        }

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

    combined
        .into_iter()
        .map(|raw| normalize_rule(workspace_root, raw))
        .collect()
}

fn normalize_rule(workspace_root: &Path, raw: LinkRuleRaw) -> Result<LinkRule> {
    let source_rel = PathBuf::from(raw.source.trim());
    if source_rel.as_os_str().is_empty() {
        bail!("link source cannot be empty")
    }
    if source_rel.is_absolute() {
        bail!("link source must be relative: {}", source_rel.display())
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
        source: workspace_root.join(source_rel),
        target,
        required: raw.required,
    })
}

fn apply_rule(workspace_root: &Path, rule: &LinkRule, report: &mut LinkApplyReport) -> Result<()> {
    if !rule.target.exists() {
        if rule.required {
            bail!(
                "required link target is missing for {}: {}",
                display_in_workspace(workspace_root, &rule.source),
                rule.target.display()
            )
        }
        report.skipped_missing_target += 1;
        return Ok(());
    }

    if !rule.source.exists() {
        if let Some(parent) = rule.source.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        create_symlink(&rule.target, &rule.source)?;
        report.linked += 1;
        return Ok(());
    }

    let metadata = fs::symlink_metadata(&rule.source)
        .with_context(|| format!("failed to inspect {}", rule.source.display()))?;
    if metadata.file_type().is_symlink() {
        let existing = fs::read_link(&rule.source)
            .with_context(|| format!("failed to read symlink {}", rule.source.display()))?;
        let existing_abs = if existing.is_absolute() {
            existing
        } else {
            rule.source
                .parent()
                .unwrap_or(workspace_root)
                .join(existing)
        };
        if same_existing_path(&existing_abs, &rule.target)? {
            report.satisfied += 1;
            return Ok(());
        }

        bail!(
            "link conflict at {}: existing symlink does not point to {}",
            display_in_workspace(workspace_root, &rule.source),
            rule.target.display()
        )
    }

    if same_existing_path(&rule.source, &rule.target)? {
        report.satisfied += 1;
        return Ok(());
    }

    bail!(
        "link conflict at {}: path exists and is not a symlink to {}",
        display_in_workspace(workspace_root, &rule.source),
        rule.target.display()
    )
}

fn same_existing_path(path_a: &Path, path_b: &Path) -> Result<bool> {
    if path_a.exists() && path_b.exists() {
        let left = path_a
            .canonicalize()
            .with_context(|| format!("failed to resolve {}", path_a.display()))?;
        let right = path_b
            .canonicalize()
            .with_context(|| format!("failed to resolve {}", path_b.display()))?;
        return Ok(left == right);
    }

    Ok(path_a == path_b)
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
