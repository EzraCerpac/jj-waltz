//! No command may launch more JJ or Git processes because a repository has more workspaces.
//!
//! Each fixture holds managed workspaces with bookmarks, one unmanaged workspace, and one
//! workspace whose checkout was deleted. Two fixtures that differ only in how many managed
//! workspaces they hold must cost every command the same number of processes.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

struct Fixture {
    temp: TempDir,
    root: PathBuf,
    shims: PathBuf,
}

impl Fixture {
    fn new(managed: usize) -> Option<Self> {
        let search = std::env::var_os("PATH")?;
        let find = |program: &str| {
            std::env::split_paths(&search)
                .map(|directory| directory.join(program))
                .find(|path| path.is_file())
        };
        let (real_jj, real_git) = (find("jj")?, find("git")?);
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        fs::create_dir(&root).unwrap();
        fs::write(
            temp.path().join("jj.toml"),
            "[user]\nname='Scaling Test'\nemail='test@example.test'\n",
        )
        .unwrap();
        let shims = temp.path().join("bin");
        fs::create_dir(&shims).unwrap();
        for (name, real) in [("jj", &real_jj), ("git", &real_git)] {
            let shim = shims.join(name);
            fs::write(
                &shim,
                format!(
                    "#!/bin/sh\nif [ -n \"$JW_SPAWN_LOG\" ]; then echo {name} >> \"$JW_SPAWN_LOG\"; fi\nexec {} \"$@\"\n",
                    shell_quote(real)
                ),
            )
            .unwrap();
            fs::set_permissions(&shim, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let fixture = Self { temp, root, shims };
        fixture.run("jj", &["git", "init"]);
        // JJ without `WorkspaceRef.root()` (0.39) can only answer one root per process.
        let roots_template = fixture
            .command(&fixture.shims.join("jj"))
            .args(["workspace", "list", "-T", "self.root()"])
            .output()
            .unwrap();
        if !roots_template.status.success() {
            return None;
        }
        fs::write(fixture.root.join("tracked"), "base\n").unwrap();
        fixture.run("jj", &["commit", "-m", "base"]);
        fixture.run("jj", &["bookmark", "create", "main", "-r", "@-"]);
        for index in 0..managed {
            let name = format!("managed-{index}");
            let bookmark = format!("wip/{name}");
            fixture.jw_ok(&["add", &name, "--no-colocate", "--no-links", "-b", &bookmark]);
        }
        fixture.jw_ok(&["add", "gone", "--no-colocate", "--no-links"]);
        fs::remove_dir_all(fixture.root.with_extension("gone")).unwrap();
        let plain = fixture.root.with_extension("plain");
        fixture.run(
            "jj",
            &[
                "workspace",
                "add",
                "--config",
                "git.colocate=false",
                "--name",
                "plain",
                plain.to_str().unwrap(),
            ],
        );
        Some(fixture)
    }

    fn command(&self, program: &Path) -> Command {
        let mut command = Command::new(program);
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("JJ_") || key.to_string_lossy().starts_with("GIT_")
            {
                command.env_remove(key);
            }
        }
        let search = std::env::var_os("PATH").unwrap();
        command
            .current_dir(&self.root)
            .env("HOME", self.temp.path())
            .env("JJ_CONFIG", self.temp.path().join("jj.toml"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("XDG_CONFIG_HOME", self.temp.path().join("config"))
            .env("XDG_STATE_HOME", self.temp.path().join("state"))
            .env(
                "PATH",
                std::env::join_paths(
                    std::iter::once(self.shims.clone()).chain(std::env::split_paths(&search)),
                )
                .unwrap(),
            );
        command
    }

    fn run(&self, program: &str, args: &[&str]) {
        let output = self
            .command(&self.shims.join(program))
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{program} {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn jw_ok(&self, args: &[&str]) {
        let output = self
            .command(Path::new(env!("CARGO_BIN_EXE_jw")))
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "jw {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// Run jw and count the JJ and Git processes it launched. Exit status is irrelevant: doctor
    /// reports the deleted checkout as unhealthy.
    fn spawns(&self, args: &[&str]) -> (usize, usize) {
        let log = self.temp.path().join("spawns.log");
        let _ = fs::remove_file(&log);
        self.command(Path::new(env!("CARGO_BIN_EXE_jw")))
            .args(args)
            .env("JW_SPAWN_LOG", &log)
            .output()
            .unwrap();
        let log = fs::read_to_string(&log).unwrap_or_default();
        (
            log.lines().filter(|line| *line == "jj").count(),
            log.lines().filter(|line| *line == "git").count(),
        )
    }
}

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_str().unwrap().replace('\'', r"'\''"))
}

#[test]
fn command_cost_does_not_grow_with_workspace_count() {
    let (Some(small), Some(large)) = (Fixture::new(2), Fixture::new(8)) else {
        return;
    };
    // Order matters: later commands mutate both fixtures identically.
    let commands: &[&[&str]] = &[
        &["list"],
        &["list", "--format=json"],
        &["list", "--format=json", "--refresh=none"],
        &["status", "--format=json"],
        &["status", "managed-0", "--format=json"],
        &["doctor", "--format=json"],
        &["path", "managed-1"],
        &["current"],
        &["switch", "managed-1", "--print-path"],
        &["adopt", "plain", "--base", "main", "--no-bookmark"],
        &["repair", "plain", "--base", "main", "--no-bookmark"],
        &["add", "fresh", "--no-colocate", "--no-links"],
        &["remove", "managed-0", "--delete-bookmark"],
        &["remove", "managed-1", "--keep-dir", "--keep-bookmark"],
        &["prune"],
    ];
    for args in commands {
        assert_eq!(
            small.spawns(args),
            large.spawns(args),
            "jw {} launched more JJ or Git processes with more workspaces",
            args.join(" ")
        );
    }
}
