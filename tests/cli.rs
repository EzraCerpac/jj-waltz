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
        .args(["completions", "fish"])
        .assert()
        .success()
        .stdout(predicate::str::contains("complete -c jw"));
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
