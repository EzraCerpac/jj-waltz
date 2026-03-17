use assert_cmd::prelude::*;
use predicates::prelude::*;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

#[test]
fn switch_creates_workspace_and_path_reports_it() {
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
    Command::cargo_bin("jw")
        .expect("binary")
        .args(["shell", "completions", "fish"])
        .assert()
        .success()
        .stdout(predicate::str::contains("__jw_workspace_candidates"))
        .stdout(predicate::str::contains("-l keep-dir"))
        .stdout(predicate::str::contains(
            "switch 'Switch to or create a workspace'",
        ));
}

#[test]
fn completions_command_generates_zsh_script() {
    Command::cargo_bin("jw")
        .expect("binary")
        .args(["shell", "completions", "zsh"])
        .assert()
        .success()
        .stdout(predicate::str::contains("_jw_workspace_candidates"))
        .stdout(predicate::str::contains(
            "--keep-dir[Forget the workspace but keep its directory]",
        ))
        .stdout(predicate::str::contains(
            "switch:Switch to or create a workspace",
        ));
}

#[test]
fn remove_deletes_workspace_directory_by_default() {
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
    let repo = TestRepo::new().expect("create test repo");

    repo.cmd()
        .args(["switch", "solver-benchmark", "--print-path"])
        .assert()
        .success()
        .stdout(predicate::str::contains("solver-benchmark"));
}

#[test]
fn switch_applies_workspace_links_for_data_directory() {
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
    let repo = TestRepo::new().expect("create test repo");
    fs::create_dir_all(repo.default_root.join("data")).expect("create data directory");
    fs::write(
        repo.default_root.join(".jwlinks.toml"),
        "[[link]]\nsource = \"cache\"\ntarget = \"../repo/data\"\nrequired = true\n",
    )
    .expect("write links config");

    let target_root = repo.default_root.with_extension("solver-benchmark");
    fs::create_dir_all(target_root.join("cache")).expect("create conflicting path");

    repo.cmd()
        .args(["switch", "solver-benchmark"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("link conflict"));
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
