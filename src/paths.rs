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
            let mut result = match prefix.kind() {
                Prefix::VerbatimDisk(drive) => PathBuf::from(format!("{}:", drive as char)),
                Prefix::VerbatimUNC(server, share) => {
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
}
