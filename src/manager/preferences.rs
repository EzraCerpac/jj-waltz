use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Default, Serialize, Deserialize)]
struct Preferences {
    #[serde(default)]
    delete_bookmarks: bool,
}

fn path() -> Result<PathBuf> {
    let root = std::env::var_os("XDG_STATE_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
        .context("cannot locate user state directory: set XDG_STATE_HOME or HOME")?;
    Ok(root.join("jj-waltz/ui.json"))
}

pub(crate) fn load_delete_bookmarks() -> Result<bool> {
    load(&path()?)
}

pub(crate) fn save_delete_bookmarks(value: bool) -> Result<()> {
    save(&path()?, value)
}

fn load(path: &Path) -> Result<bool> {
    let data = match fs::read(path) {
        Ok(data) => data,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error).context("failed to read UI preferences"),
    };
    let preferences: Preferences =
        serde_json::from_slice(&data).context("invalid UI preferences")?;
    Ok(preferences.delete_bookmarks)
}

fn save(path: &Path, delete_bookmarks: bool) -> Result<()> {
    let parent = path
        .parent()
        .context("UI preferences have no parent directory")?;
    fs::create_dir_all(parent).context("failed to create UI state directory")?;
    // Serialize writers, and replace a complete document instead of truncating live state.
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(parent.join("ui.lock"))?;
    fs2::FileExt::lock_exclusive(&lock)?;
    crate::metadata::write_json_atomic(path, &Preferences { delete_bookmarks })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preference_defaults_to_keep_and_round_trips_both_choices() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state/ui.json");
        assert!(!load(&path).unwrap());
        save(&path, true).unwrap();
        assert!(load(&path).unwrap());
        save(&path, false).unwrap();
        assert!(!load(&path).unwrap());
        fs::write(&path, "broken").unwrap();
        assert!(load(&path).is_err());
    }
}
