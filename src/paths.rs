use std::borrow::Cow;
use std::path::Path;

/// Rust's canonical Windows paths use a verbatim prefix that Git for Windows
/// cannot accept as a worktree destination or repository argument.
pub(crate) fn external_command_path(path: &Path) -> Cow<'_, Path> {
    #[cfg(windows)]
    {
        use std::ffi::OsString;
        use std::path::{Component, PathBuf, Prefix};

        let mut components = path.components();
        if let Some(Component::Prefix(prefix)) = components.next() {
            // Removing the verbatim prefix must not let Win32 normalize a name
            // into another checkout or a DOS device. Retain unsupported spellings
            // so external commands fail instead of operating on an alias.
            if components.clone().any(|component| match component {
                Component::Normal(name) => !ordinary_windows_name(name),
                Component::CurDir | Component::ParentDir => true,
                _ => false,
            }) {
                return Cow::Borrowed(path);
            }
            let mut result = match prefix.kind() {
                Prefix::VerbatimDisk(drive) => PathBuf::from(format!("{}:", drive as char)),
                Prefix::VerbatimUNC(server, share) => {
                    if !ordinary_windows_name(server) || !ordinary_windows_name(share) {
                        return Cow::Borrowed(path);
                    }
                    let mut prefix = OsString::from(r"\\");
                    prefix.push(server);
                    prefix.push(r"\");
                    prefix.push(share);
                    PathBuf::from(prefix)
                }
                _ => return Cow::Borrowed(path),
            };
            for component in components {
                result.push(component.as_os_str());
            }
            return Cow::Owned(result);
        }
    }
    Cow::Borrowed(path)
}

#[cfg(windows)]
fn ordinary_windows_name(name: &std::ffi::OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    if name.ends_with(['.', ' ']) || name.chars().any(|c| c < ' ' || "<>:\"/|?*".contains(c)) {
        return false;
    }
    let stem = name
        .split('.')
        .next()
        .unwrap_or(name)
        .trim_end()
        .to_ascii_uppercase();
    !matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) && !stem
        .strip_prefix("COM")
        .or_else(|| stem.strip_prefix("LPT"))
        .is_some_and(|suffix| {
            matches!(
                suffix,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        })
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn git_arguments_preserve_drive_and_unc_paths() {
        for (input, expected) in [
            (r"\\?\C:\repo space\workspace", r"C:\repo space\workspace"),
            (r"\\?\C:\", r"C:\"),
            (r"\\?\UNC\server\share\répo", r"\\server\share\répo"),
            (r"C:\ordinary\repo", r"C:\ordinary\repo"),
            (r"\\server\share\repo", r"\\server\share\repo"),
            (r"\\?\Volume{volume}\repo", r"\\?\Volume{volume}\repo"),
        ] {
            assert_eq!(
                external_command_path(Path::new(input)).as_ref(),
                Path::new(expected)
            );
        }
    }

    #[test]
    fn git_arguments_retain_normalization_sensitive_paths() {
        for input in [
            r"\\?\C:\repo.\workspace",
            r"\\?\C:\repo \workspace",
            r"\\?\UNC\server\share\repo.",
            r"\\?\UNC\server\share\repo ",
            r"\\?\C:\repo\..\workspace",
            r"\\?\C:\repo\CON.txt",
            r"\\?\C:\repo\LPT1",
            r"\\?\C:\repo\COM².txt",
        ] {
            let result = external_command_path(Path::new(input));
            assert!(matches!(result, Cow::Borrowed(_)));
            assert_eq!(result.as_ref(), Path::new(input));
        }
    }
}
