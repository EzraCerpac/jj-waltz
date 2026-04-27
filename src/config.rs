use anyhow::{Context, Result};
use serde::Deserialize;
use std::env;
use std::fs;
use std::path::PathBuf;

const CONFIG_DIR: &str = "jj-waltz";
const CONFIG_FILE: &str = "config.toml";
const DEFAULT_BOOKMARK_TEMPLATE: &str = "{workspace}";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub workspace: WorkspaceConfig,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceConfig {
    pub create_bookmark: bool,
    pub bookmark_template: String,
}

#[derive(Debug, Deserialize, Default)]
struct RawConfig {
    workspace: Option<RawWorkspaceConfig>,
}

#[derive(Debug, Deserialize, Default)]
struct RawWorkspaceConfig {
    #[serde(default)]
    create_bookmark: bool,
    bookmark_template: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            workspace: WorkspaceConfig {
                create_bookmark: false,
                bookmark_template: DEFAULT_BOOKMARK_TEMPLATE.to_owned(),
            },
        }
    }
}

impl Config {
    pub fn load() -> Result<Self> {
        let Some(path) = config_path()? else {
            return Ok(Self::default());
        };
        if !path.exists() {
            return Ok(Self::default());
        }

        let contents = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let raw: RawConfig = toml::from_str(&contents)
            .with_context(|| format!("failed to parse {}", path.display()))?;

        Ok(raw.into())
    }
}

impl From<RawConfig> for Config {
    fn from(raw: RawConfig) -> Self {
        let defaults = Config::default();
        let Some(workspace) = raw.workspace else {
            return defaults;
        };

        Self {
            workspace: WorkspaceConfig {
                create_bookmark: workspace.create_bookmark,
                bookmark_template: workspace
                    .bookmark_template
                    .unwrap_or(defaults.workspace.bookmark_template),
            },
        }
    }
}

pub fn bookmark_from_template(template: &str, workspace: &str) -> String {
    template.replace("{workspace}", workspace)
}

fn config_path() -> Result<Option<PathBuf>> {
    let config_home = if let Some(value) = env::var_os("XDG_CONFIG_HOME") {
        PathBuf::from(value)
    } else if let Some(value) = env::var_os("HOME") {
        PathBuf::from(value).join(".config")
    } else {
        return Ok(None);
    };

    Ok(Some(config_home.join(CONFIG_DIR).join(CONFIG_FILE)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_do_not_create_bookmarks() {
        let config = Config::default();
        assert!(!config.workspace.create_bookmark);
        assert_eq!(config.workspace.bookmark_template, "{workspace}");
    }

    #[test]
    fn template_replaces_workspace_token() {
        assert_eq!(
            bookmark_from_template("wip/{workspace}", "feature-a"),
            "wip/feature-a"
        );
    }
}
