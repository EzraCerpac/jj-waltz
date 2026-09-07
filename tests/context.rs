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
    if !jj_available() {
        eprintln!("skipping test because `jj` is not installed");
        return;
    }
    let temp = tempfile::tempdir().expect("tempdir");
    run("git", temp.path(), &["init"]);
    for command in ["list", "root", "current"] {
        let output = Command::cargo_bin("jw")
            .expect("binary")
            .current_dir(temp.path())
            .args([command])
            .output()
            .expect("run jw");
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("Git checkout without a JJ workspace")
        );
    }
}

#[test]
fn context_reports_bare_git_topology() {
    let temp = tempfile::tempdir().unwrap();
    run("git", temp.path(), &["init", "--bare"]);
    let output = Command::cargo_bin("jw")
        .unwrap()
        .current_dir(temp.path())
        .args(["context", "--format", "json"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    let root = temp.path().canonicalize().unwrap();
    assert_eq!(value["git"]["git_dir"].as_str(), root.to_str());
    assert_eq!(value["git"]["common_dir"].as_str(), root.to_str());
    assert!(value["git"]["checkout_root"].is_null());
    assert!(
        value["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["code"] == "bare_repository")
    );
}

#[test]
#[cfg(unix)]
fn missing_jj_does_not_suggest_workspace_routing() {
    let temp = tempfile::tempdir().unwrap();
    run("git", temp.path(), &["init"]);
    let git = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|dir| dir.join("git"))
        .find(|path| path.is_file())
        .unwrap();
    std::os::unix::fs::symlink(git, temp.path().join("git")).unwrap();
    let output = Command::cargo_bin("jw")
        .unwrap()
        .current_dir(temp.path())
        .env("PATH", temp.path())
        .args(["list"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("failed to execute jj"));
    assert!(!error.contains("Git checkout without a JJ workspace"));
}

#[test]
#[cfg(unix)]
fn context_preserves_whitespace_paths_and_jj_associations() {
    if !jj_available() {
        return;
    }
    for suffix in [" ", "\nline", "\r"] {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().canonicalize().unwrap();
        let repo = base.join(format!("repo{suffix}"));
        let secondary = base.join(format!("secondary{suffix}"));
        let linked = base.join(format!("linked{suffix}"));
        run("jj", &base, &["git", "init", repo.to_str().unwrap()]);
        run("git", &repo, &["commit", "--allow-empty", "-m", "initial"]);
        run(
            "jj",
            &repo,
            &[
                "workspace",
                "add",
                "--name",
                "secondary",
                secondary.to_str().unwrap(),
            ],
        );
        run(
            "git",
            &repo,
            &["worktree", "add", "--detach", linked.to_str().unwrap()],
        );
        for path in [&repo, &secondary, &linked] {
            let output = Command::cargo_bin("jw")
                .unwrap()
                .current_dir(path)
                .args(["context", "--format=json"])
                .output()
                .unwrap();
            assert!(output.status.success());
            let value: Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(value["jj"]["primary_checkout"].as_str(), repo.to_str());
            if path == &linked {
                assert!(value["jj"]["workspace_root"].is_null());
                assert_eq!(value["git"]["linked_worktree"], true);
            } else {
                assert_eq!(value["jj"]["workspace_root"].as_str(), path.to_str());
                assert_eq!(
                    value["jj"]["repository_path"].as_str(),
                    repo.join(".jj/repo").to_str()
                );
            }
            if path != &secondary {
                assert_eq!(value["git"]["checkout_root"].as_str(), path.to_str());
                assert_eq!(
                    value["git"]["common_dir"].as_str(),
                    repo.join(".git").to_str()
                );
            }
        }
    }
}

#[test]
fn explicit_nested_path_ignores_git_discovery_ceiling() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    run("git", &root, &["init"]);
    let nested = root.join("nested");
    fs::create_dir(&nested).unwrap();
    let output = Command::cargo_bin("jw")
        .unwrap()
        .current_dir(&root)
        .env("GIT_CEILING_DIRECTORIES", &root)
        .args(["context", nested.to_str().unwrap(), "--format=json"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["git"]["checkout_root"].as_str(), root.to_str());
    assert_eq!(
        value["git"]["common_dir"].as_str(),
        root.join(".git").to_str()
    );
}
