//! Every fixture is temporary and has isolated JJ/Git/user configuration.
use assert_cmd::prelude::*;
use predicates::prelude::*;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::TempDir;

struct Fixture {
    temp: TempDir,
    root: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        fs::create_dir(&root).unwrap();
        fs::write(
            temp.path().join("jj.toml"),
            "[user]\nname='Colocation Test'\nemail='test@example.test'\n",
        )
        .unwrap();
        fs::write(
            temp.path().join("gitconfig"),
            "[user]\nname=Colocation Test\nemail=test@example.test\n",
        )
        .unwrap();
        let f = Self { temp, root };
        f.run("git", &f.root, &["init"]);
        fs::write(f.root.join("tracked"), "base\n").unwrap();
        f.run("git", &f.root, &["add", "."]);
        f.run("git", &f.root, &["commit", "-m", "base"]);
        f.run("jj", &f.root, &["git", "init", "--colocate"]);
        f
    }
    fn command(&self, program: &str, cwd: &Path) -> Command {
        let mut command = Command::new(program);
        command.current_dir(cwd);
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("JJ_") || key.to_string_lossy().starts_with("GIT_")
            {
                command.env_remove(key);
            }
        }
        command
            .env("HOME", self.temp.path())
            .env("JJ_CONFIG", self.temp.path().join("jj.toml"))
            .env("GIT_CONFIG_GLOBAL", self.temp.path().join("gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("XDG_CONFIG_HOME", self.temp.path().join("config"))
            .env("XDG_CACHE_HOME", self.temp.path().join("cache"))
            .env("XDG_STATE_HOME", self.temp.path().join("state"));
        command
    }
    fn run(&self, program: &str, cwd: &Path, args: &[&str]) -> Output {
        let output = self.command(program, cwd).args(args).output().unwrap();
        assert!(
            output.status.success(),
            "{program} {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }
    fn jw(&self) -> Command {
        self.command(env!("CARGO_BIN_EXE_jw"), &self.root)
    }
    fn path(&self, name: &str) -> PathBuf {
        self.root.with_extension(name)
    }
    fn supports(&self) -> bool {
        String::from_utf8_lossy(
            &self
                .run("jj", &self.root, &["workspace", "add", "--help"])
                .stdout,
        )
        .contains("--colocate")
    }
    fn config(&self, text: &str) {
        let path = self.temp.path().join("config/jj-waltz");
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("config.toml"), text).unwrap();
    }
    fn metadata_root(&self) -> PathBuf {
        let out = self.run("jj", &self.root, &["config", "path", "--repo"]);
        PathBuf::from(String::from_utf8(out.stdout).unwrap().trim())
            .parent()
            .unwrap()
            .join("jj-waltz")
    }
    fn record(&self, name: &str) -> (PathBuf, serde_json::Value) {
        let out = self.run("jj", &self.root, &["config", "path", "--repo"]);
        let config = PathBuf::from(String::from_utf8(out.stdout).unwrap().trim());
        for entry in fs::read_dir(config.parent().unwrap().join("jj-waltz/workspaces")).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|ext| ext == "json") {
                let value: serde_json::Value =
                    serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
                if value["metadata"]["workspace_name"] == name {
                    return (path, value);
                }
            }
        }
        panic!("no record for {name}")
    }
    fn registrations(&self) -> Vec<u8> {
        self.run(
            "git",
            &self.root,
            &["worktree", "list", "--porcelain", "-z"],
        )
        .stdout
    }
    fn names(&self) -> String {
        String::from_utf8(
            self.run(
                "jj",
                &self.root,
                &["workspace", "list", "-T", "name ++ '\\n'"],
            )
            .stdout,
        )
        .unwrap()
    }
    fn stale_registration(&self) -> PathBuf {
        let path = self.temp.path().join("unrelated stale\ncheckout");
        self.run(
            "git",
            &self.root,
            &[
                "worktree",
                "add",
                "--detach",
                path.to_str().unwrap(),
                "HEAD",
            ],
        );
        let out = self.run("git", &path, &["rev-parse", "--absolute-git-dir"]);
        let admin = PathBuf::from(String::from_utf8(out.stdout).unwrap().trim());
        fs::remove_dir_all(path).unwrap();
        admin
    }
    #[cfg(unix)]
    fn shim(&self, script: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let bin = self.temp.path().join("bin");
        fs::create_dir_all(&bin).unwrap();
        let shim = bin.join("jj");
        fs::write(&shim, script).unwrap();
        fs::set_permissions(shim, fs::Permissions::from_mode(0o755)).unwrap();
        bin
    }
    #[cfg(unix)]
    fn with_shim(&self, command: &mut Command, script: &str) {
        let bin = self.shim(script);
        let search = std::env::var_os("PATH").unwrap();
        let real = std::env::split_paths(&search)
            .map(|p| p.join("jj"))
            .find(|p| p.is_file())
            .unwrap();
        command.env("JW_REAL_JJ", real).env(
            "PATH",
            std::env::join_paths(std::iter::once(bin).chain(std::env::split_paths(&search)))
                .unwrap(),
        );
    }
}

#[test]
fn default_and_cli_config_precedence_and_existing_switch() {
    let f = Fixture::new();
    f.jw().args(["add", "plain"]).assert().success();
    assert!(!f.path("plain").join(".git").exists());
    if !f.supports() {
        return;
    }
    f.config("[workspace]\ncolocate=true\n");
    f.jw().args(["add", "configured"]).assert().success();
    assert!(f.path("configured").join(".git").is_file());
    f.jw()
        .args(["add", "off", "--no-colocate"])
        .assert()
        .success();
    assert!(!f.path("off").join(".git").exists());
    f.jw()
        .args(["switch", "configured-switch", "--print-path"])
        .assert()
        .success();
    assert!(f.path("configured-switch").join(".git").is_file());
    f.jw()
        .args(["switch", "switch-off", "--no-colocate", "--print-path"])
        .assert()
        .success();
    assert!(!f.path("switch-off").join(".git").exists());
    f.config("[workspace]\ncolocate=false\n");
    f.jw()
        .args(["switch", "first", "last", "--colocate", "--print-path"])
        .assert()
        .success();
    for name in ["first", "last"] {
        let path = f.path(name);
        assert!(path.join(".git").is_file());
        assert_eq!(
            f.run("git", &path, &["show", "HEAD:tracked"]).stdout,
            b"base\n"
        );
        let (_, record) = f.record(name);
        assert_eq!(record["schema_version"], 3);
        assert!(record["metadata"].get("external_owner").is_none());
        assert!(record["metadata"]["owned_git_worktree"].is_object());
    }
    // Existing workspaces are never converted, and do not need capability support.
    f.jw()
        .args(["switch", "plain", "--colocate", "--print-path"])
        .assert()
        .success();
    assert!(!f.path("plain").join(".git").exists());
    f.jw()
        .args(["add", "conflict", "--colocate", "--no-colocate"])
        .assert()
        .failure();
    f.config("[workspace]\ncopy_on_write=true\n");
    f.jw()
        .args(["add", "cow", "--colocate"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("cannot be combined"));
    assert!(!f.path("cow").exists());
}

#[test]
fn unsupported_colocation_fails_before_mutation() {
    let f = Fixture::new();
    if f.supports() {
        return;
    }
    let before = f.names();
    let registrations = f.registrations();
    f.jw()
        .args(["add", "unsupported", "--colocate"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("requires JJ"));
    f.config("[workspace]\ncolocate=true\n");
    f.jw()
        .args(["switch", "unsupported", "--print-path"])
        .assert()
        .failure();
    assert_eq!(f.names(), before);
    assert_eq!(f.registrations(), registrations);
    assert!(!f.path("unsupported").exists());
    f.jw()
        .args(["add", "override", "--no-colocate"])
        .assert()
        .success();
    f.jw()
        .args(["switch", "override", "--colocate", "--print-path"])
        .assert()
        .success();
}

#[test]
fn owned_cleanup_removes_only_target_and_keep_dir_preserves_files() {
    let f = Fixture::new();
    if !f.supports() {
        return;
    }
    let stale = f.stale_registration();
    let external = f.temp.path().join("external");
    f.run(
        "git",
        &f.root,
        &[
            "worktree",
            "add",
            "--detach",
            external.to_str().unwrap(),
            "HEAD",
        ],
    );
    let before = f.registrations();
    for (name, keep_dir) in [("removed", false), ("kept", true), ("missing", false)] {
        f.jw().args(["add", name, "--colocate"]).assert().success();
        let (record_path, record) = f.record(name);
        let admin = PathBuf::from(
            record["metadata"]["owned_git_worktree"]["topology"]["git_dir"]
                .as_str()
                .unwrap(),
        );
        if name == "missing" {
            fs::remove_dir_all(f.path(name)).unwrap();
        }
        let mut cmd = f.jw();
        cmd.args(["remove", name, "--keep-bookmark"]);
        if keep_dir {
            cmd.arg("--keep-dir");
        }
        cmd.assert().success();
        assert!(!admin.exists());
        assert!(!record_path.exists());
        assert!(!f.names().lines().any(|line| line == name));
        assert_eq!(f.registrations(), before);
        if keep_dir {
            assert_eq!(fs::read(f.path(name).join("tracked")).unwrap(), b"base\n");
            assert!(!f.path(name).join(".git").exists());
        } else {
            assert!(!f.path(name).exists());
        }
        assert!(stale.exists());
        assert!(external.join(".git").exists());
    }
}

#[test]
fn replaced_topology_or_marker_and_missing_metadata_are_protected() {
    let f = Fixture::new();
    if !f.supports() {
        return;
    }
    f.jw()
        .args(["add", "owned", "--colocate"])
        .assert()
        .success();
    let path = f.path("owned");
    let (record_path, record) = f.record("owned");
    let admin = PathBuf::from(
        record["metadata"]["owned_git_worktree"]["topology"]["git_dir"]
            .as_str()
            .unwrap(),
    );
    let before = f.registrations();
    let link = fs::read(path.join(".git")).unwrap();
    fs::write(
        path.join(".git"),
        format!("gitdir: {}\n", f.root.join(".git").display()),
    )
    .unwrap();
    f.jw()
        .args(["remove", "owned", "--keep-dir", "--keep-bookmark"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("topology changed"));
    fs::write(path.join(".git"), link).unwrap();
    let marker = fs::read(admin.join("jw-owner")).unwrap();
    fs::write(admin.join("jw-owner"), "replacement").unwrap();
    f.jw()
        .args(["remove", "owned", "--keep-bookmark"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("marker changed"));
    fs::write(admin.join("jw-owner"), marker).unwrap();
    // Unreadable, missing, and old records may never infer jw ownership.
    fs::write(&record_path, b"{broken").unwrap();
    f.jw()
        .args(["remove", "owned", "--keep-bookmark"])
        .assert()
        .failure();
    fs::remove_file(record_path).unwrap();
    f.jw()
        .args(["remove", "owned", "--keep-bookmark"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("externally owned"));
    assert_eq!(f.registrations(), before);
    assert!(path.exists());
    fs::remove_dir_all(path).unwrap();
    f.jw()
        .args(["prune"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "external Git worktree registration",
        ));
    assert!(admin.exists());
}

#[cfg(unix)]
#[test]
fn creation_failures_roll_back_owned_links_and_leave_unrelated_registrations() {
    let f = Fixture::new();
    if !f.supports() {
        return;
    }
    let stale = f.stale_registration();
    let before = f.registrations();
    // Fail before add, after add, capturing the operation, creating the bookmark,
    // recording its legacy marker, and persisting lifecycle metadata.
    for stage in [
        "before-add",
        "after-add",
        "operation",
        "bookmark",
        "marker",
        "metadata",
    ] {
        let script = r#"#!/bin/sh
case "$*" in
  *"workspace add"*)
    case "$*" in *--help*) exec "$JW_REAL_JJ" "$@";; esac
    if [ "$JW_FAIL_STAGE" = before-add ]; then echo injected >&2; exit 1; fi
    "$JW_REAL_JJ" "$@" || exit $?
    if [ "$JW_FAIL_STAGE" = after-add ]; then echo injected >&2; exit 1; fi
    if [ "$JW_FAIL_STAGE" = operation ]; then touch "$JW_FAIL_FILE"; fi
    if [ "$JW_FAIL_STAGE" = metadata ]; then mkdir -p "$JW_METADATA_ROOT"; printf broken > "$JW_METADATA_ROOT/manifest.json"; fi
    exit 0;;
  *"bookmark create"*)
    if [ "$JW_FAIL_STAGE" = bookmark ]; then echo injected >&2; exit 1; fi
    "$JW_REAL_JJ" "$@" || exit $?
    if [ "$JW_FAIL_STAGE" = marker ]; then mkdir .jj/jw-bookmark; fi
    exit 0;;
  *"operation log"*) if [ -f "$JW_FAIL_FILE" ]; then rm "$JW_FAIL_FILE"; echo injected >&2; exit 1; fi;;
esac
exec "$JW_REAL_JJ" "$@"
"#;
        let mut cmd = f.jw();
        f.with_shim(&mut cmd, script);
        cmd.env("JW_FAIL_STAGE", stage)
            .env("JW_FAIL_FILE", f.temp.path().join("fail"))
            .env("JW_METADATA_ROOT", f.metadata_root())
            .args([
                "add",
                stage,
                "--colocate",
                "--bookmark",
                &format!("wip/{stage}"),
            ])
            .assert()
            .failure();
        assert!(!f.path(stage).exists(), "{stage} checkout leaked");
        assert!(!f.names().lines().any(|name| name == stage));
        assert_eq!(f.registrations(), before, "{stage} registration leaked");
        assert!(stale.exists());
        if stage == "metadata" {
            fs::remove_dir_all(f.metadata_root()).unwrap();
        }
    }
}

#[cfg(unix)]
#[test]
fn unverifiable_partial_creation_is_retained_with_explicit_diagnostic() {
    let f = Fixture::new();
    if !f.supports() {
        return;
    }
    let stale = f.stale_registration();
    let mut cmd = f.jw();
    f.with_shim(
        &mut cmd,
        r#"#!/bin/sh
case "$*" in *"workspace add"*)
    case "$*" in *--help*) exec "$JW_REAL_JJ" "$@";; esac
    git worktree add --detach "$JW_PARTIAL_PATH" HEAD || exit $?
    echo injected-partial-add >&2; exit 1;;
esac
exec "$JW_REAL_JJ" "$@"
"#,
    );
    cmd.env("JW_PARTIAL_PATH", f.path("partial"))
        .args(["add", "partial", "--colocate"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unregistered checkout retained"));
    assert!(f.path("partial").join(".git").exists());
    assert!(stale.exists());
    assert!(!f.names().lines().any(|name| name == "partial"));
}

#[test]
fn link_failure_rolls_back_created_git_worktrees() {
    let f = Fixture::new();
    if !f.supports() {
        return;
    }
    let stale = f.stale_registration();
    let before = f.registrations();
    fs::write(f.root.join(".jwlinks.toml"), "not valid toml [").unwrap();
    f.jw()
        .args(["switch", "linkfail", "--colocate", "--print-path"])
        .assert()
        .failure();
    assert!(!f.path("linkfail").exists());
    assert_eq!(f.registrations(), before);
    assert!(stale.exists());
}

#[cfg(unix)]
#[test]
fn forget_failure_restores_gitlink_and_retains_record() {
    let f = Fixture::new();
    if !f.supports() {
        return;
    }
    f.jw()
        .args(["add", "owned", "--colocate"])
        .assert()
        .success();
    let link = fs::read(f.path("owned").join(".git")).unwrap();
    let (record, _) = f.record("owned");
    let registrations = f.registrations();
    let mut cmd = f.jw();
    f.with_shim(&mut cmd, "#!/bin/sh\ncase \"$*\" in *\"workspace forget\"*) echo injected >&2; exit 1;; esac\nexec \"$JW_REAL_JJ\" \"$@\"\n");
    cmd.args(["remove", "owned", "--keep-bookmark"])
        .assert()
        .failure();
    assert_eq!(fs::read(f.path("owned").join(".git")).unwrap(), link);
    assert!(record.exists());
    assert_eq!(f.registrations(), registrations);
}

#[cfg(unix)]
#[test]
fn unverifiable_forget_retains_detached_link_and_metadata() {
    let f = Fixture::new();
    if !f.supports() {
        return;
    }
    let stale = f.stale_registration();
    f.jw()
        .args(["add", "owned", "--colocate"])
        .assert()
        .success();
    let (record, value) = f.record("owned");
    let link = fs::read(f.path("owned").join(".git")).unwrap();
    let admin = PathBuf::from(
        value["metadata"]["owned_git_worktree"]["topology"]["git_dir"]
            .as_str()
            .unwrap(),
    );
    let mut cmd = f.jw();
    cmd.env("JW_FORGOTTEN", f.temp.path().join("forgotten"));
    f.with_shim(&mut cmd, "#!/bin/sh\ncase \"$*\" in *\"workspace forget\"*) \"$JW_REAL_JJ\" \"$@\" || exit $?; touch \"$JW_FORGOTTEN\"; exit 0;; *\"workspace list\"*) if test -f \"$JW_FORGOTTEN\"; then echo injected-list-error >&2; exit 1; fi;; esac\nexec \"$JW_REAL_JJ\" \"$@\"\n");
    cmd.args(["remove", "owned", "--keep-bookmark"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("detached Git link retained at"));
    assert!(!f.names().lines().any(|name| name == "owned"));
    assert!(!f.path("owned").join(".git").exists());
    assert_eq!(
        fs::read(f.path("owned").join(".jj/jw-detached-git")).unwrap(),
        link
    );
    assert!(record.exists());
    assert!(admin.exists());
    assert!(stale.exists());
}

#[cfg(unix)]
#[test]
fn nonzero_status_after_forget_is_verified_and_cleanup_completes() {
    let f = Fixture::new();
    if !f.supports() {
        return;
    }
    let stale = f.stale_registration();
    let before = f.registrations();
    f.jw()
        .args(["add", "owned", "--colocate"])
        .assert()
        .success();
    let (record, _) = f.record("owned");
    let mut cmd = f.jw();
    f.with_shim(&mut cmd, "#!/bin/sh\ncase \"$*\" in *\"workspace forget\"*) \"$JW_REAL_JJ\" \"$@\" || exit $?; echo injected-after-forget >&2; exit 1;; esac\nexec \"$JW_REAL_JJ\" \"$@\"\n");
    cmd.args(["remove", "owned", "--keep-bookmark"])
        .assert()
        .success()
        .stderr(predicate::str::contains(
            "verified workspace owned was forgotten",
        ));
    assert!(!f.path("owned").exists());
    assert!(!record.exists());
    assert_eq!(f.registrations(), before);
    assert!(stale.exists());
}

#[test]
fn prune_owned_missing_checkout_preserves_unrelated_stale_registration() {
    let f = Fixture::new();
    if !f.supports() {
        return;
    }
    let stale = f.stale_registration();
    let before = f.registrations();
    f.jw()
        .args(["add", "gone", "--colocate"])
        .assert()
        .success();
    let (record, _) = f.record("gone");
    fs::remove_dir_all(f.path("gone")).unwrap();
    f.jw().args(["prune"]).assert().success();
    assert!(!record.exists());
    assert_eq!(f.registrations(), before);
    assert!(stale.exists());
}

#[test]
fn externally_created_colocated_workspace_stays_protected_after_adoption() {
    let f = Fixture::new();
    if !f.supports() {
        return;
    }
    f.run(
        "jj",
        &f.root,
        &[
            "workspace",
            "add",
            "--colocate",
            "--name",
            "foreign",
            f.path("foreign").to_str().unwrap(),
        ],
    );
    f.jw()
        .args([
            "adopt",
            "foreign",
            "--base",
            "parents(foreign@)",
            "--no-bookmark",
        ])
        .assert()
        .success();
    let (record_path, record) = f.record("foreign");
    assert!(record["metadata"]["external_owner"].is_object());
    assert!(record["metadata"].get("owned_git_worktree").is_none());
    let before = f.registrations();
    for keep in [false, true] {
        let mut cmd = f.jw();
        cmd.args(["remove", "foreign", "--keep-bookmark"]);
        if keep {
            cmd.arg("--keep-dir");
        }
        cmd.assert()
            .failure()
            .stderr(predicate::str::contains("externally owned"));
        assert!(f.path("foreign").join(".git").exists());
        assert!(record_path.exists());
        assert_eq!(f.registrations(), before);
    }
    fs::remove_dir_all(f.path("foreign")).unwrap();
    let stale_before = f.registrations();
    f.jw()
        .args(["prune"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("externally owned"));
    assert_eq!(f.registrations(), stale_before);
}

#[test]
fn locked_owned_registration_is_preserved() {
    let f = Fixture::new();
    if !f.supports() {
        return;
    }
    f.jw()
        .args(["add", "locked", "--colocate"])
        .assert()
        .success();
    f.run(
        "git",
        &f.root,
        &["worktree", "lock", f.path("locked").to_str().unwrap()],
    );
    let before = f.registrations();
    let (record, _) = f.record("locked");
    f.jw()
        .args(["remove", "locked", "--keep-bookmark"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("locked"));
    assert!(f.path("locked").join(".git").exists());
    assert!(record.exists());
    assert_eq!(f.registrations(), before);
}

#[cfg(unix)]
#[test]
fn capability_probe_precedes_creation_even_on_a_new_version() {
    let f = Fixture::new();
    let before = f.registrations();
    let names = f.names();
    for configured in [false, true] {
        f.config(if configured {
            "[workspace]\ncolocate=true\n"
        } else {
            "[workspace]\ncolocate=false\n"
        });
        let mut cmd = f.jw();
        f.with_shim(&mut cmd, "#!/bin/sh\ncase \"$*\" in *\"workspace add\"*--help*) \"$JW_REAL_JJ\" \"$@\" | sed '/--colocate/d'; exit $?;; esac\nexec \"$JW_REAL_JJ\" \"$@\"\n");
        if configured {
            cmd.args(["switch", "unsupported", "--print-path"]);
        } else {
            cmd.args(["add", "unsupported", "--colocate"]);
        }
        cmd.assert()
            .failure()
            .stderr(predicate::str::contains("requires JJ"));
        assert!(!f.path("unsupported").exists());
        assert_eq!(f.registrations(), before);
        assert_eq!(f.names(), names);
    }
}

#[test]
fn non_git_backend_rejects_colocation_before_creation() {
    let f = Fixture::new();
    if !f.supports() {
        return;
    }
    let local = f.temp.path().join("local");
    let help = f.run("jj", &f.root, &["debug", "--help"]);
    let init = if String::from_utf8_lossy(&help.stdout).contains("init-simple") {
        "init-simple"
    } else {
        "init-local"
    };
    f.run("jj", &f.root, &["debug", init, local.to_str().unwrap()]);
    f.command(env!("CARGO_BIN_EXE_jw"), &local)
        .args(["add", "unsupported", "--colocate"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Git-backed"));
    assert!(!local.with_extension("unsupported").exists());
}

#[test]
fn keep_dir_can_disconnect_the_current_owned_workspace() {
    let f = Fixture::new();
    if !f.supports() {
        return;
    }
    let before = f.registrations();
    f.jw()
        .args(["add", "current", "--colocate"])
        .assert()
        .success();
    let (record, _) = f.record("current");
    f.command(env!("CARGO_BIN_EXE_jw"), &f.path("current"))
        .args(["remove", "current", "--keep-dir", "--keep-bookmark"])
        .assert()
        .success();
    assert_eq!(
        fs::read(f.path("current").join("tracked")).unwrap(),
        b"base\n"
    );
    assert!(!f.path("current").join(".git").exists());
    assert!(!record.exists());
    assert_eq!(f.registrations(), before);
}
