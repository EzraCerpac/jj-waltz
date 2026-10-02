//! Set JW_TEST_PINNED_JJ to the PR 9943 build to run real adopted-worktree tests.
use assert_cmd::prelude::*;
use predicates::prelude::*;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::TempDir;

struct Fixture {
    temp: TempDir,
    root: PathBuf,
    linked: PathBuf,
    jj: PathBuf,
    bin: PathBuf,
}

impl Fixture {
    fn new(jj: PathBuf) -> Self {
        let temp = tempfile::Builder::new()
            .prefix("jw-external-owner-")
            .tempdir_in("/tmp")
            .unwrap();
        let root = temp.path().join("primary");
        let linked = temp.path().join("app-checkout");
        let bin = temp.path().join("bin");
        fs::create_dir_all(&bin).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&jj, bin.join("jj")).unwrap();
        fs::write(
            temp.path().join("jj.toml"),
            "[user]\nname='Owner Test'\nemail='owner@example.test'\n",
        )
        .unwrap();
        fs::write(
            temp.path().join("gitconfig"),
            "[user]\nname = Owner Test\nemail = owner@example.test\n",
        )
        .unwrap();
        fs::create_dir(&root).unwrap();
        let fixture = Self {
            temp,
            root,
            linked,
            jj,
            bin,
        };
        fixture.run("git", &fixture.root, &["init"]);
        fs::write(fixture.root.join("tracked"), "base\n").unwrap();
        fixture.run("git", &fixture.root, &["add", "."]);
        fixture.run("git", &fixture.root, &["commit", "-m", "base"]);
        fixture.run_jj(&fixture.root, &["git", "init", "--colocate"]);
        fixture.run(
            "git",
            &fixture.root,
            &[
                "worktree",
                "add",
                "--detach",
                fixture.linked.to_str().unwrap(),
                "HEAD",
            ],
        );
        fixture.run_jj(
            &fixture.linked,
            &["git", "worktree", "adopt", "--name", "app"],
        );
        fixture
    }

    fn command(&self, program: impl AsRef<std::ffi::OsStr>, cwd: &Path) -> Command {
        let mut cmd = Command::new(program);
        cmd.current_dir(cwd);
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("JJ_") || key.to_string_lossy().starts_with("GIT_")
            {
                cmd.env_remove(key);
            }
        }
        let path = std::env::join_paths(
            std::iter::once(self.bin.clone())
                .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
        )
        .unwrap();
        cmd.env("PATH", path)
            .env("HOME", self.temp.path())
            .env("JJ_CONFIG", self.temp.path().join("jj.toml"))
            .env("GIT_CONFIG_GLOBAL", self.temp.path().join("gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("XDG_CONFIG_HOME", self.temp.path().join("config"))
            .env("XDG_CACHE_HOME", self.temp.path().join("cache"))
            .env("XDG_STATE_HOME", self.temp.path().join("state"));
        cmd
    }

    fn run(&self, program: impl AsRef<std::ffi::OsStr>, cwd: &Path, args: &[&str]) -> Output {
        let output = self.command(program, cwd).args(args).output().unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    fn run_jj(&self, cwd: &Path, args: &[&str]) -> Output {
        self.run(&self.jj, cwd, args)
    }
    fn jw(&self) -> Command {
        self.command(env!("CARGO_BIN_EXE_jw"), &self.root)
    }
    fn record_path(&self) -> PathBuf {
        fs::read_dir({
            let output = self.run_jj(&self.root, &["config", "path", "--repo"]);
            PathBuf::from(String::from_utf8(output.stdout).unwrap().trim())
                .parent()
                .unwrap()
                .join("jj-waltz/workspaces")
        })
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.extension().is_some_and(|ext| ext == "json"))
        .unwrap()
    }
    fn git_dir(&self) -> PathBuf {
        let out = self.run("git", &self.linked, &["rev-parse", "--absolute-git-dir"]);
        PathBuf::from(String::from_utf8(out.stdout).unwrap().trim())
    }
    fn graph(&self) -> Vec<u8> {
        self.run_jj(
            &self.root,
            &[
                "--ignore-working-copy",
                "log",
                "--no-graph",
                "-r",
                "all()",
                "-T",
                "commit_id ++ '\n'",
            ],
        )
        .stdout
    }
}

fn pinned() -> Option<PathBuf> {
    let path = std::env::var_os("JW_TEST_PINNED_JJ").map(PathBuf::from)?;
    let output = Command::new(&path).arg("--version").output().unwrap();
    assert!(
        String::from_utf8_lossy(&output.stdout)
            .contains("ede10cda453017def68e672a5715220ddf10c09b"),
        "test requires the pinned PR 9943 binary"
    );
    Some(path)
}

#[test]
fn adopted_external_checkout_is_protected_without_and_with_metadata() {
    let Some(jj) = pinned() else { return };
    let f = Fixture::new(jj);
    let git_dir = f.git_dir();
    let graph = f.graph();
    // Missing jw metadata must never make a live linked checkout removable.
    for flags in [
        vec!["remove", "app", "--keep-bookmark"],
        vec!["remove", "app", "--keep-dir", "--keep-bookmark"],
    ] {
        f.jw()
            .args(flags)
            .assert()
            .failure()
            .stderr(predicate::str::contains("externally owned"));
    }
    assert!(git_dir.exists());
    f.jw()
        .args(["adopt", "app", "--base", "parents(@)", "--no-bookmark"])
        .assert()
        .success();
    let record = f.record_path();
    let before: serde_json::Value = serde_json::from_slice(&fs::read(&record).unwrap()).unwrap();
    assert_eq!(before["schema_version"], 2);
    assert_eq!(
        before["metadata"]["external_owner"]["git_dir"],
        git_dir.to_str().unwrap()
    );
    f.jw()
        .args(["repair", "app", "--base", "parents(app@)", "--no-bookmark"])
        .assert()
        .success();
    let repaired: serde_json::Value = serde_json::from_slice(&fs::read(&record).unwrap()).unwrap();
    assert_eq!(
        repaired["metadata"]["external_owner"],
        before["metadata"]["external_owner"]
    );
    // Persisted ownership protects even after .git topology was damaged.
    fs::remove_file(f.linked.join(".git")).unwrap();
    f.jw()
        .args(["remove", "app", "--keep-dir", "--keep-bookmark"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("externally owned"));
    assert!(git_dir.exists());
    assert_eq!(f.graph(), graph);
}

#[test]
fn stale_external_checkout_requires_owner_cleanup_then_explicit_reconciliation() {
    let Some(jj) = pinned() else { return };
    let f = Fixture::new(jj);
    f.run_jj(&f.root, &["bookmark", "create", "wip/app", "-r", "app@"]);
    f.jw()
        .args([
            "adopt",
            "app",
            "--base",
            "parents(app@)",
            "--bookmark",
            "wip/app",
        ])
        .assert()
        .success();
    let git_dir = f.git_dir();
    f.jw()
        .args(["reconcile-external", "app"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("still exists"));
    fs::remove_dir_all(&f.linked).unwrap();
    f.jw()
        .args(["prune"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("externally owned"));
    f.jw()
        .args(["reconcile-external", "app"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("still exists"));
    // Also prove missing metadata cannot bypass a still-registered missing checkout.
    let record = f.record_path();
    let bytes = fs::read(&record).unwrap();
    fs::remove_file(&record).unwrap();
    f.jw()
        .args(["prune"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Git worktree registration"));
    fs::write(&record, bytes).unwrap();
    f.jw()
        .args(["doctor", "--format=json"])
        .assert()
        .failure()
        .stdout(predicate::str::contains("reconcile-external app"));
    // Simulate owning application's completed Git cleanup.
    f.run("git", &f.root, &["worktree", "prune"]);
    assert!(!git_dir.exists());
    let graph = f.graph();
    f.jw()
        .args(["reconcile-external", "app"])
        .assert()
        .success();
    assert!(!record.exists());
    assert_eq!(f.graph(), graph);
    let bookmarks = f.run_jj(&f.root, &["bookmark", "list", "-T", "name ++ '\n'"]);
    assert!(String::from_utf8_lossy(&bookmarks.stdout).contains("wip/app"));
}

#[test]
fn pinned_native_forget_removes_git_registration_and_undo_cannot_restore_it() {
    let Some(jj) = pinned() else { return };
    let f = Fixture::new(jj);
    let git_dir = f.git_dir();
    f.run_jj(&f.root, &["workspace", "forget", "app"]);
    assert!(!git_dir.exists());
    assert!(!f.linked.join(".git").exists());
    f.run_jj(&f.root, &["undo"]);
    assert!(!git_dir.exists());
    assert!(!f.linked.join(".git").exists());
    let list = f.run_jj(&f.root, &["workspace", "list"]);
    assert!(String::from_utf8_lossy(&list.stdout).contains("app:"));
}

#[test]
fn jw_creation_under_pinned_jj_remains_an_ordinary_removable_workspace() {
    let Some(jj) = pinned() else { return };
    let f = Fixture::new(jj);
    f.jw()
        .args(["add", "ordinary", "--at", "parents(@)"])
        .assert()
        .success();
    let path_output = f.jw().args(["path", "ordinary"]).output().unwrap();
    let path = PathBuf::from(String::from_utf8(path_output.stdout).unwrap().trim());
    assert!(!path.join(".git").exists());
    f.jw()
        .args(["remove", "ordinary", "--keep-bookmark"])
        .assert()
        .success();
    assert!(!path.exists());
    assert!(f.git_dir().exists());
}
