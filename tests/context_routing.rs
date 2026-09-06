use assert_cmd::prelude::*;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::TempDir;

struct Fixture {
    _temp: TempDir,
    root: PathBuf,
    config: PathBuf,
    user_config: PathBuf,
    external: PathBuf,
    start: String,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().canonicalize().unwrap();
        let config = base.join("config");
        fs::create_dir(&config).unwrap();
        let user_config = config.join("jj.toml");
        fs::write(
            &user_config,
            "[user]\nname='Test'\nemail='test@example.com'\n",
        )
        .unwrap();
        let root = base.join("repo");
        let external = base.join(".codex/worktrees/task/repo");
        let mut fixture = Self {
            _temp: temp,
            root,
            config,
            user_config,
            external,
            start: String::new(),
        };
        fixture.run(
            "jj",
            &base,
            &["git", "init", fixture.root.to_str().unwrap()],
        );
        fs::write(fixture.root.join("tracked.txt"), "initial\n").unwrap();
        fixture.run("jj", &fixture.root, &["commit", "-m", "initial"]);
        fixture.start = fixture.run(
            "jj",
            &fixture.root,
            &["log", "--no-graph", "-r", "@-", "-T", "commit_id"],
        );
        fixture.run(
            "git",
            &fixture.root,
            &[
                "worktree",
                "add",
                "--detach",
                fixture.external.to_str().unwrap(),
                &fixture.start,
            ],
        );
        fixture
    }

    fn command(&self, program: &str, cwd: &Path) -> Command {
        let mut command = if program == "jw" {
            Command::cargo_bin("jw").unwrap()
        } else {
            Command::new(program)
        };
        command
            .current_dir(cwd)
            .env("XDG_CONFIG_HOME", &self.config)
            .env("JJ_CONFIG", &self.user_config)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", self.config.join("no-git-config"));
        command
    }

    fn run(&self, program: &str, cwd: &Path, args: &[&str]) -> String {
        let output = self.command(program, cwd).args(args).output().unwrap();
        assert!(
            output.status.success(),
            "{program} {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    fn operation(&self) -> String {
        self.run(
            "jj",
            &self.root,
            &[
                "--ignore-working-copy",
                "--at-op=@",
                "op",
                "log",
                "--no-graph",
                "-n",
                "1",
                "-T",
                "id",
            ],
        )
    }

    fn context(&self, cwd: &Path) -> Value {
        serde_json::from_str(&self.run("jw", cwd, &["context", "--format=json"])).unwrap()
    }

    fn git_status(&self) -> String {
        self.run(
            "git",
            &self.external,
            &[
                "--no-optional-locks",
                "status",
                "--porcelain=v1",
                "--untracked-files=all",
            ],
        )
    }
}

#[test]
fn discovery_preserves_dirty_detached_checkout_and_operation() {
    let fixture = Fixture::new();
    fs::write(fixture.external.join("tracked.txt"), "user edit\n").unwrap();
    fs::write(fixture.external.join("untracked.txt"), "keep me\n").unwrap();
    fs::create_dir(fixture.external.join("nested")).unwrap();
    let status = fixture.git_status();
    let operation = fixture.operation();
    let report = fixture.context(&fixture.external.join("nested"));
    assert_eq!(report["git"]["head_commit"], fixture.start);
    assert!(report["git"]["head_ref"].is_null());
    assert_eq!(
        report["jj"]["primary_checkout"],
        fixture.root.to_str().unwrap()
    );
    assert!(report["jj"]["workspace_root"].is_null());
    assert_eq!(fixture.git_status(), status);
    assert_eq!(fixture.operation(), operation);
    assert!(!fixture.external.join(".jj").exists());
    assert_eq!(
        fs::read_to_string(fixture.external.join("tracked.txt")).unwrap(),
        "user edit\n"
    );
}

#[test]
fn explicit_route_preserves_start_and_external_worktree() {
    let fixture = Fixture::new();
    assert!(fixture.git_status().is_empty());
    let report = fixture.context(&fixture.external);
    let start = report["git"]["head_commit"].as_str().unwrap();
    let resolved = fixture.run(
        "jj",
        &fixture.root,
        &[
            "--ignore-working-copy",
            "--at-op=@",
            "log",
            "--no-graph",
            "-r",
            start,
            "-T",
            "commit_id",
        ],
    );
    assert_eq!(resolved, fixture.start);
    fixture.run("jw", &fixture.root, &["add", "agent-task", "--at", start]);
    let workspace = PathBuf::from(fixture.run("jw", &fixture.root, &["path", "agent-task"]));
    assert_eq!(
        fixture.run(
            "jj",
            &workspace,
            &[
                "--ignore-working-copy",
                "log",
                "--no-graph",
                "-r",
                "@-",
                "-T",
                "commit_id"
            ]
        ),
        fixture.start
    );
    let executed = fixture.run(
        "jw",
        &fixture.root,
        &["switch", "agent-task", "--execute", "jj root"],
    );
    assert!(executed.lines().any(|line| Path::new(line) == workspace));
    fixture.run(
        "jw",
        &fixture.root,
        &["remove", "agent-task", "--keep-bookmark"],
    );
    assert!(!workspace.exists());
    assert!(fixture.external.is_dir());
    assert_eq!(
        fixture.run("git", &fixture.external, &["rev-parse", "HEAD"]),
        fixture.start
    );
    assert!(fixture.git_status().is_empty());
}

#[test]
fn unavailable_start_stops_before_workspace_creation() {
    let fixture = Fixture::new();
    let before = fixture.operation();
    let output: Output = fixture
        .command("jj", &fixture.root)
        .args([
            "--ignore-working-copy",
            "--at-op=@",
            "log",
            "-r",
            "1111111111111111111111111111111111111111",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert_eq!(fixture.operation(), before);
    assert!(fixture.external.is_dir());
    assert!(!fixture.root.with_extension("agent-task").exists());
}

#[test]
fn secondary_workspace_has_its_own_jj_identity() {
    let fixture = Fixture::new();
    fixture.run(
        "jw",
        &fixture.root,
        &["add", "secondary", "--at", &fixture.start],
    );
    let workspace = PathBuf::from(fixture.run("jw", &fixture.root, &["path", "secondary"]));
    let report = fixture.context(&workspace);
    assert_eq!(report["jj"]["workspace_name"], "secondary");
    assert_eq!(report["jj"]["workspace_root"], workspace.to_str().unwrap());
    assert_eq!(
        report["jj"]["primary_checkout"],
        fixture.root.to_str().unwrap()
    );
    assert!(report["git"]["checkout_root"].is_null());
}

#[test]
fn git_only_unborn_and_broken_paths_report_diagnostics() {
    let fixture = Fixture::new();
    let plain = fixture.config.join("plain");
    fs::create_dir(&plain).unwrap();
    fixture.run("git", &plain, &["init"]);
    let report = fixture.context(&plain);
    assert_eq!(report["git"]["checkout_root"], plain.to_str().unwrap());
    assert!(report["jj"]["workspace_root"].is_null());
    assert!(report["jj"]["primary_checkout"].is_null());
    assert!(
        report["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["code"] == "git_head_unavailable")
    );

    let broken = fixture.config.join("broken");
    fs::create_dir(&broken).unwrap();
    fs::write(broken.join(".git"), "gitdir: missing\n").unwrap();
    fs::create_dir(broken.join(".jj")).unwrap();
    let report = fixture.context(&broken);
    let diagnostics = report["diagnostics"].as_array().unwrap();
    assert!(
        diagnostics
            .iter()
            .any(|d| d["code"] == "git_metadata_invalid")
    );
    assert!(
        diagnostics
            .iter()
            .any(|d| d["code"] == "jj_metadata_invalid")
    );

    let absent = fixture.config.join("absent");
    let report: Value = serde_json::from_str(&fixture.run(
        "jw",
        &fixture.root,
        &["context", absent.to_str().unwrap(), "--format=json"],
    ))
    .unwrap();
    assert!(
        report["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["code"] == "path_unavailable")
    );
}

#[test]
fn missing_tools_remain_machine_readable() {
    let fixture = Fixture::new();
    let empty_path = fixture.config.join("empty-path");
    fs::create_dir(&empty_path).unwrap();
    let output = fixture
        .command("jw", &fixture.external)
        .env("PATH", &empty_path)
        .args(["context", "--format=json"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    let diagnostics = report["diagnostics"].as_array().unwrap();
    assert!(diagnostics.iter().any(|d| d["code"] == "git_unavailable"));
    assert!(diagnostics.iter().any(|d| d["code"] == "jj_unavailable"));
}

#[test]
fn explicit_path_overrides_inherited_git_directory() {
    let fixture = Fixture::new();
    let output = fixture
        .command("jw", &fixture.root)
        .env("GIT_DIR", fixture.root.join(".git"))
        .env("GIT_WORK_TREE", &fixture.root)
        .args([
            "context",
            fixture.external.to_str().unwrap(),
            "--format=json",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        report["git"]["checkout_root"],
        fixture.external.to_str().unwrap()
    );
    assert_eq!(report["git"]["linked_worktree"], true);
}
