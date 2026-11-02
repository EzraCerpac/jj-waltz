use anyhow::{Context, Result};
use serde::Deserialize;
use std::env;
use std::fs;
use std::path::PathBuf;

const CONFIG_DIR: &str = "jj-waltz";
const CONFIG_FILE: &str = "config.toml";
const DEFAULT_BOOKMARK_TEMPLATE: &str = "{workspace}";
const DEFAULT_TRUNK_REVSET: &str = "trunk()";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub default_command: DefaultCommand,
    pub workspace: WorkspaceConfig,
    pub trunk: TrunkConfig,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DefaultCommand {
    #[default]
    Ui,
    List,
    Help,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceConfig {
    pub create_bookmark: bool,
    /// None inherits the verified primary workspace; Some(false) explicitly opts out.
    pub colocate: Option<bool>,
    pub bookmark_template: String,
    pub copy_on_write: CopyOnWrite,
}

/// How new workspaces receive their files.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CopyOnWrite {
    /// JJ writes every file.
    #[default]
    Off,
    /// Clone only the files JJ tracks in the current workspace.
    Tracked,
    /// Clone everything in the current workspace, including ignored and untracked
    /// files such as `node_modules` and `.env`.
    All,
}

impl CopyOnWrite {
    pub fn enabled(self) -> bool {
        self != Self::Off
    }
}

/// `copy_on_write = true` predates scopes and means `"tracked"`.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawCopyOnWrite {
    Enabled(bool),
    Scope(CopyOnWrite),
}

impl From<RawCopyOnWrite> for CopyOnWrite {
    fn from(raw: RawCopyOnWrite) -> Self {
        match raw {
            RawCopyOnWrite::Enabled(true) => Self::Tracked,
            RawCopyOnWrite::Enabled(false) => Self::Off,
            RawCopyOnWrite::Scope(scope) => scope,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrunkConfig {
    pub revset: String,
}

#[derive(Debug, Deserialize, Default)]
struct RawConfig {
    default_command: Option<DefaultCommand>,
    workspace: Option<RawWorkspaceConfig>,
    trunk: Option<RawTrunkConfig>,
}

#[derive(Debug, Deserialize, Default)]
struct RawWorkspaceConfig {
    #[serde(default)]
    create_bookmark: bool,
    colocate: Option<bool>,
    bookmark_template: Option<String>,
    copy_on_write: Option<RawCopyOnWrite>,
}

#[derive(Debug, Deserialize, Default)]
struct RawTrunkConfig {
    revset: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            default_command: DefaultCommand::Ui,
            workspace: WorkspaceConfig {
                create_bookmark: false,
                colocate: None,
                bookmark_template: DEFAULT_BOOKMARK_TEMPLATE.to_owned(),
                copy_on_write: CopyOnWrite::Off,
            },
            trunk: TrunkConfig {
                revset: DEFAULT_TRUNK_REVSET.to_owned(),
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
        let workspace = raw.workspace.unwrap_or_default();
        let trunk = raw.trunk.unwrap_or_default();

        Self {
            default_command: raw.default_command.unwrap_or_default(),
            workspace: WorkspaceConfig {
                create_bookmark: workspace.create_bookmark,
                colocate: workspace.colocate,
                bookmark_template: workspace
                    .bookmark_template
                    .unwrap_or(defaults.workspace.bookmark_template),
                copy_on_write: workspace
                    .copy_on_write
                    .map_or(CopyOnWrite::Off, CopyOnWrite::from),
            },
            trunk: TrunkConfig {
                revset: trunk.revset.unwrap_or(defaults.trunk.revset),
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
        assert_eq!(config.default_command, DefaultCommand::Ui);
        assert!(!config.workspace.create_bookmark);
        assert_eq!(config.workspace.bookmark_template, "{workspace}");
        assert_eq!(config.workspace.copy_on_write, CopyOnWrite::Off);
        assert_eq!(config.trunk.revset, "trunk()");
    }

    #[test]
    fn bare_command_is_explicitly_configurable() {
        for (value, expected) in [
            ("ui", DefaultCommand::Ui),
            ("list", DefaultCommand::List),
            ("help", DefaultCommand::Help),
        ] {
            let raw: RawConfig = toml::from_str(&format!("default_command = {value:?}")).unwrap();
            assert_eq!(Config::from(raw).default_command, expected);
        }
        assert!(toml::from_str::<RawConfig>("default_command = 'remove'").is_err());
    }

    #[test]
    fn parses_copy_on_write_opt_in() {
        let raw: RawConfig = toml::from_str(
            r#"
                [workspace]
                copy_on_write = true
            "#,
        )
        .unwrap();

        let config = Config::from(raw);
        assert_eq!(config.workspace.copy_on_write, CopyOnWrite::Tracked);
        assert!(!config.workspace.create_bookmark);
    }

    #[test]
    fn parses_copy_on_write_scopes() {
        for (value, expected) in [
            ("false", CopyOnWrite::Off),
            ("true", CopyOnWrite::Tracked),
            ("\"off\"", CopyOnWrite::Off),
            ("\"tracked\"", CopyOnWrite::Tracked),
            ("\"all\"", CopyOnWrite::All),
        ] {
            let raw: RawConfig =
                toml::from_str(&format!("[workspace]\ncopy_on_write = {value}\n")).unwrap();
            assert_eq!(
                Config::from(raw).workspace.copy_on_write,
                expected,
                "{value}"
            );
        }
        assert!(toml::from_str::<RawConfig>("[workspace]\ncopy_on_write = \"some\"\n").is_err());
    }

    #[test]
    fn colocation_preserves_unset_and_explicit_false() {
        for (text, expected) in [
            ("", None),
            ("[workspace]", None),
            ("[workspace]\ncolocate = false", Some(false)),
            ("[workspace]\ncolocate = true", Some(true)),
        ] {
            let raw: RawConfig = toml::from_str(text).unwrap();
            assert_eq!(Config::from(raw).workspace.colocate, expected);
        }
    }

    #[test]
    fn parses_trunk_revset_without_workspace_section() {
        let raw: RawConfig = toml::from_str(
            r#"
                [trunk]
                revset = "main@origin"
            "#,
        )
        .unwrap();

        let config = Config::from(raw);
        assert_eq!(config.trunk.revset, "main@origin");
        assert!(!config.workspace.create_bookmark);
        assert_eq!(config.workspace.bookmark_template, "{workspace}");
    }

    #[test]
    fn empty_trunk_section_uses_default_revset() {
        let raw: RawConfig = toml::from_str("[trunk]").unwrap();

        assert_eq!(Config::from(raw).trunk.revset, "trunk()");
    }

    #[test]
    fn template_replaces_workspace_token() {
        assert_eq!(
            bookmark_from_template("wip/{workspace}", "feature-a"),
            "wip/feature-a"
        );
    }
}
