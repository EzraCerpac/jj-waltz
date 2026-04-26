use assert_cmd::prelude::*;
use predicates::prelude::*;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

fn jj_available() -> bool {
    Command::new("jj")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

macro_rules! skip_without_jj {
    () => {
        if !jj_available() {
            eprintln!("skipping test because `jj` is not installed");
            return;
        }
    };
}

#[test]
fn switch_creates_workspace_and_path_reports_it() {
    skip_without_jj!();
    let repo = TestRepo::new().expect("create test repo");

    repo.cmd()
        .args(["switch", "feature-a"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Created workspace: feature-a"));

    repo.cmd()
        .args(["path", "feature-a"])
        .assert()
        .success()
        .stdout(predicate::str::contains("feature-a"));
}

#[test]
fn switch_default_returns_existing_root() {
    skip_without_jj!();
    let repo = TestRepo::new().expect("create test repo");
    let test_root = repo.default_root.with_extension("test");
    repo.run_jj([
        "workspace",
        "add",
        "--name",
        "test",
        test_root.to_str().unwrap(),
    ]);

    Command::cargo_bin("jw")
        .expect("binary")
        .current_dir(&test_root)
        .args(["path", "default"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            repo.default_root.to_string_lossy().as_ref(),
        ));
}

#[test]
fn completions_command_generates_fish_script() {
    skip_without_jj!();
    Command::cargo_bin("jw")
        .expect("binary")
        .args(["shell", "completions", "fish"])
        .assert()
        .success()
        .stdout(predicate::str::contains("__jw_workspace_candidates"))
        .stdout(predicate::str::contains("-l keep-dir"))
        .stdout(predicate::str::contains("doctor 'Run environment checks'"))
        .stdout(predicate::str::contains(
            "switch 'Switch to or create a workspace'",
        ));
}

#[test]
fn completions_command_generates_zsh_script() {
    skip_without_jj!();
    Command::cargo_bin("jw")
        .expect("binary")
        .args(["shell", "completions", "zsh"])
        .assert()
        .success()
        .stdout(predicate::str::contains("_jw_workspace_candidates"))
        .stdout(predicate::str::contains(
            "--keep-dir[Forget the workspace but keep its directory]",
        ))
        .stdout(predicate::str::contains("doctor:Run environment checks"))
        .stdout(predicate::str::contains(
            "switch:Switch to or create a workspace",
        ));
}

#[test]
fn remove_deletes_workspace_directory_by_default() {
    skip_without_jj!();
    let repo = TestRepo::new().expect("create test repo");
    let workspace_root = repo.default_root.with_extension("feature-a");

    repo.cmd().args(["switch", "feature-a"]).assert().success();

    assert!(workspace_root.is_dir());

    repo.cmd()
        .args(["remove", "feature-a"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Deleted directory:"));

    assert!(!workspace_root.exists());
}

#[test]
fn completion_helper_lists_workspace_candidates() {
    skip_without_jj!();
    let repo = TestRepo::new().expect("create test repo");

    repo.cmd().args(["switch", "feature-a"]).assert().success();

    repo.cmd()
        .args(["shell", "complete-workspaces"])
        .assert()
        .success()
        .stdout(predicate::str::contains("feature-a\tExisting workspace"))
        .stdout(predicate::str::contains("@\tCurrent workspace"))
        .stdout(predicate::str::contains("^\tDefault workspace"));
}

#[test]
fn switch_print_path_does_not_overflow_stack() {
    skip_without_jj!();
    let repo = TestRepo::new().expect("create test repo");

    repo.cmd()
        .args(["switch", "solver-benchmark", "--print-path"])
        .assert()
        .success()
        .stdout(predicate::str::contains("solver-benchmark"));
}

#[test]
fn switch_print_path_preserves_nested_subdirectory() {
    skip_without_jj!();
    let repo = TestRepo::new().expect("create test repo");
    let nested = repo.default_root.join("src/nested");
    fs::create_dir_all(&nested).expect("create nested directory");

    Command::cargo_bin("jw")
        .expect("binary")
        .current_dir(&nested)
        .args(["switch", "feature-nested", "--print-path"])
        .assert()
        .success()
        .stdout(predicate::str::contains("feature-nested/src/nested"));
}

#[test]
fn list_supports_json_output() {
    skip_without_jj!();
    let repo = TestRepo::new().expect("create test repo");
    let output = repo
        .cmd()
        .args(["--json", "list"])
        .output()
        .expect("run list --json");
    assert!(output.status.success());
    let parsed: Value = serde_json::from_slice(&output.stdout).expect("parse json");
    let rows = parsed.as_array().expect("array output");
    assert!(!rows.is_empty());
    assert!(
        rows.iter()
            .any(|row| row.get("name") == Some(&Value::String("default".to_owned())))
    );
}

#[test]
fn current_supports_json_output() {
    skip_without_jj!();
    let repo = TestRepo::new().expect("create test repo");
    let output = repo
        .cmd()
        .args(["--json", "current"])
        .output()
        .expect("run current --json");
    assert!(output.status.success());
    let parsed: Value = serde_json::from_slice(&output.stdout).expect("parse json");
    assert_eq!(parsed["name"], Value::String("default".to_owned()));
}

#[test]
fn doctor_command_succeeds_in_initialized_repo() {
    skip_without_jj!();
    let repo = TestRepo::new().expect("create test repo");
    repo.cmd().args(["doctor"]).assert().success();
}

#[test]
fn switch_applies_workspace_links_for_data_directory() {
    skip_without_jj!();
    let repo = TestRepo::new().expect("create test repo");
    fs::create_dir_all(repo.default_root.join("data")).expect("create data directory");
    fs::write(
        repo.default_root.join(".jwlinks.toml"),
        "[[link]]\nsource = \"data\"\ntarget = \"../repo/data\"\nrequired = true\n",
    )
    .expect("write links config");

    repo.cmd()
        .args(["switch", "solver-benchmark"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Links: 1 created"));

    let workspace_data = repo
        .default_root
        .with_extension("solver-benchmark")
        .join("data");
    let metadata = fs::symlink_metadata(&workspace_data).expect("metadata");
    assert!(metadata.file_type().is_symlink());
}

#[test]
fn switch_uses_default_workspace_link_config() {
    skip_without_jj!();
    let repo = TestRepo::new().expect("create test repo");
    fs::create_dir_all(repo.default_root.join("data")).expect("create data directory");
    fs::write(
        repo.default_root.join(".jwlinks.toml"),
        "[[link]]\nsource = \"data\"\ntarget = \"../repo/data\"\nrequired = true\n",
    )
    .expect("write links config");

    repo.cmd()
        .args(["switch", "solver-benchmark"])
        .assert()
        .success();
    fs::remove_file(repo.default_root.join(".jwlinks.toml")).expect("remove default config");

    Command::cargo_bin("jw")
        .expect("binary")
        .current_dir(repo.default_root.with_extension("solver-benchmark"))
        .args(["switch", "default"])
        .assert()
        .success();

    repo.cmd()
        .args(["switch", "solver-benchmark"])
        .assert()
        .success();

    let workspace_data = repo
        .default_root
        .with_extension("solver-benchmark")
        .join("data");
    let metadata = fs::symlink_metadata(&workspace_data).expect("metadata");
    assert!(metadata.file_type().is_symlink());
}

#[test]
fn switch_accepts_existing_directory_when_it_matches_target() {
    skip_without_jj!();
    let repo = TestRepo::new().expect("create test repo");
    fs::create_dir_all(repo.default_root.join("data")).expect("create data directory");
    fs::write(
        repo.default_root.join(".jwlinks.toml"),
        "[[link]]\nsource = \"data\"\ntarget = \"../repo/data\"\nrequired = true\n",
    )
    .expect("write links config");

    repo.cmd().args(["switch", "default"]).assert().success();
}

#[test]
fn switch_fails_on_conflicting_existing_path() {
    skip_without_jj!();
    let repo = TestRepo::new().expect("create test repo");
    fs::create_dir_all(repo.default_root.join("data")).expect("create data directory");
    fs::write(
        repo.default_root.join(".jwlinks.toml"),
        "[[link]]\nsource = \"cache\"\ntarget = \"../repo/data\"\nrequired = true\n",
    )
    .expect("write links config");

    let target_root = repo.default_root.with_extension("solver-benchmark");
    repo.cmd()
        .args(["switch", "solver-benchmark", "--no-links"])
        .assert()
        .success();

    fs::create_dir_all(target_root.join("cache")).expect("create conflicting path");
    Command::cargo_bin("jw")
        .expect("binary")
        .current_dir(&target_root)
        .args(["switch", "default", "--no-links"])
        .assert()
        .success();

    repo.cmd()
        .args(["switch", "solver-benchmark"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("link conflict"));
}

#[test]
fn switch_reapplies_links_after_intermediate_commits_and_workspace_hops() {
    skip_without_jj!();
    let repo = TestRepo::new().expect("create test repo");
    fs::create_dir_all(repo.default_root.join("data")).expect("create data directory");
    fs::write(repo.default_root.join("data/blob.bin"), [7_u8; 8192]).expect("write data blob");
    fs::write(
        repo.default_root.join(".jwlinks.toml"),
        "[[link]]\nsource = \"data\"\ntarget = \"../repo/data\"\nrequired = true\n",
    )
    .expect("write links config");

    repo.cmd().args(["switch", "feature-a"]).assert().success();
    let feature_a_root = repo.default_root.with_extension("feature-a");
    fs::write(feature_a_root.join("feature.txt"), "feature-a\n").expect("write feature file");
    run_in(&feature_a_root, ["jj", "file", "track", "feature.txt"]).expect("track file");
    run_in(&feature_a_root, ["jj", "commit", "-m", "feature-a commit"]).expect("commit change");

    Command::cargo_bin("jw")
        .expect("binary")
        .current_dir(&feature_a_root)
        .args(["switch", "default"])
        .assert()
        .success();

    repo.cmd().args(["switch", "feature-b"]).assert().success();
    let feature_b_data = repo.default_root.with_extension("feature-b").join("data");
    let metadata = fs::symlink_metadata(&feature_b_data).expect("metadata");
    assert!(metadata.file_type().is_symlink());
}

struct TestRepo {
    _tempdir: TempDir,
    default_root: PathBuf,
}

impl TestRepo {
    fn new() -> anyhow::Result<Self> {
        let tempdir = tempfile::tempdir()?;
        let default_root = tempdir.path().join("repo");

        run_in(
            tempdir.path(),
            ["jj", "git", "init", default_root.to_str().unwrap()],
        )?;
        fs::write(default_root.join("README.md"), "hello\n")?;
        run_in(&default_root, ["jj", "file", "track", "root:README.md"])?;
        run_in(&default_root, ["jj", "commit", "-m", "initial"])?;

        Ok(Self {
            _tempdir: tempdir,
            default_root,
        })
    }

    fn cmd(&self) -> Command {
        let mut cmd = Command::cargo_bin("jw").expect("binary");
        cmd.current_dir(&self.default_root);
        cmd
    }

    fn run_jj<const N: usize>(&self, args: [&str; N]) {
        run_in(
            &self.default_root,
            std::iter::once("jj").chain(args).collect::<Vec<_>>(),
        )
        .expect("jj command succeeds");
    }
}

fn run_in<I, S>(cwd: &Path, args: I) -> anyhow::Result<()>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let values = args
        .into_iter()
        .map(|arg| arg.as_ref().to_owned())
        .collect::<Vec<_>>();
    let (program, rest) = values.split_first().expect("program");
    let output = Command::new(program).current_dir(cwd).args(rest).output()?;
    if output.status.success() {
        Ok(())
    } else {
        anyhow::bail!(
            "command failed: {}\nstdout: {}\nstderr: {}",
            values.join(" "),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    }
}
