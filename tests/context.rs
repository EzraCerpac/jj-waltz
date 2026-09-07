use assert_cmd::prelude::*;
use serde_json::Value;
use std::fs;
use std::path::Path;
use std::process::Command;

fn run(program: &str, cwd: &Path, args: &[&str]) {
    let output = Command::new(program)
        .current_dir(cwd)
        .env("XDG_CONFIG_HOME", cwd.join("config"))
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .args(args)
        .output()
        .expect("run command");
    assert!(
        output.status.success(),
        "{program} {args:?} failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn jj_operation_id(repo: &Path, config: &Path) -> String {
    let output = Command::new("jj")
        .current_dir(repo)
        .env("XDG_CONFIG_HOME", config)
        .args([
            "--at-operation",
            "@",
            "--ignore-working-copy",
            "operation",
            "log",
            "--limit=1",
            "--no-graph",
            "-T",
            "id",
        ])
        .output()
        .expect("read JJ operation");
    assert!(output.status.success());
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn jj_available() -> bool {
    Command::new("jj")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

#[test]
fn context_reports_non_repository_as_successful_json() {
    let temp = tempfile::tempdir().expect("tempdir");
    let output = Command::cargo_bin("jw")
        .expect("binary")
        .current_dir(temp.path())
        .args(["context", ".", "--format", "json"])
        .output()
        .expect("run jw");
    assert!(output.status.success());
    let value: Value = serde_json::from_slice(&output.stdout).expect("JSON context report");
    assert_eq!(value["schema_version"], 1);
    assert_eq!(value["command"], "context");
    assert!(value["git"]["checkout_root"].is_null());
    assert!(value["jj"]["workspace_root"].is_null());
}

#[test]
fn context_reports_git_topology_and_jj_identity_without_refreshing() {
    if !jj_available() {
        eprintln!("skipping test because `jj` is not installed");
        return;
    }
    let temp = tempfile::tempdir().expect("tempdir");
    let config = temp.path().join("config");
    fs::create_dir_all(&config).expect("config dir");
    let repo = temp.path().join("repo");
    run("jj", temp.path(), &["git", "init", repo.to_str().unwrap()]);

    let before = jj_operation_id(&repo, &config);
    let output = Command::cargo_bin("jw")
        .expect("binary")
        .current_dir(&repo)
        .env("XDG_CONFIG_HOME", &config)
        .args(["context", "--format", "json"])
        .output()
        .expect("run jw");
    assert!(output.status.success());
    let value: Value = serde_json::from_slice(&output.stdout).expect("JSON context report");
    assert_eq!(value["jj"]["workspace_name"], "default");
    assert!(value["jj"]["repository_path"].as_str().is_some());
    assert_eq!(value["git"]["linked_worktree"], false);
    let after = jj_operation_id(&repo, &config);
    assert_eq!(before, after, "context must not update JJ operation state");
}

#[test]
fn context_finds_primary_jj_checkout_for_linked_git_worktree() {
    if !jj_available() {
        eprintln!("skipping test because `jj` is not installed");
        return;
    }
    let temp = tempfile::tempdir().expect("tempdir");
    let config = temp.path().join("config");
    fs::create_dir_all(&config).expect("config dir");
    let repo = temp.path().join("repo");
    run("jj", temp.path(), &["git", "init", repo.to_str().unwrap()]);
    run("git", &repo, &["commit", "--allow-empty", "-m", "initial"]);
    let linked = temp.path().join("linked");
    run("git", &repo, &["worktree", "add", linked.to_str().unwrap()]);

    let output = Command::cargo_bin("jw")
        .expect("binary")
        .current_dir(&linked)
        .env("XDG_CONFIG_HOME", &config)
        .args(["context", "--format", "json"])
        .output()
        .expect("run jw");
    assert!(output.status.success());
    let value: Value = serde_json::from_slice(&output.stdout).expect("JSON context report");
    assert_eq!(value["git"]["linked_worktree"], true);
    assert_eq!(value["jj"]["workspace_root"], Value::Null);
    assert_eq!(
        value["jj"]["primary_checkout"].as_str(),
        Some(
            fs::canonicalize(&repo)
                .expect("canonical repo")
                .to_str()
                .unwrap(),
        )
    );
}

#[test]
fn workspace_command_explains_git_only_checkout() {
    let temp = tempfile::tempdir().expect("tempdir");
    run("git", temp.path(), &["init"]);
    let output = Command::cargo_bin("jw")
        .expect("binary")
        .current_dir(temp.path())
        .args(["list"])
        .output()
        .expect("run jw");
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("Git checkout without a JJ workspace")
    );
}
