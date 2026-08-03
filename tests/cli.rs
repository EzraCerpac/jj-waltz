use assert_cmd::prelude::*;
use predicates::prelude::*;
use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

struct ShellCase {
    name: &'static str,
    program: &'static str,
    args: &'static [&'static str],
    execute_args: &'static [&'static str],
}

const POWERSHELL_SWITCH_TEST_ARGS: &[&str] = &[
    "-NoLogo",
    "-NoProfile",
    "-Command",
    "jw shell init powershell | Out-String | Invoke-Expression; jw '^' | Out-Null; (Get-Location).Path; jw '-' | Out-Null; (Get-Location).Path",
];

const POWERSHELL_EXECUTE_TEST_ARGS: &[&str] = &[
    "-NoLogo",
    "-NoProfile",
    "-Command",
    "jw shell init powershell | Out-String | Invoke-Expression; jw switch feature-a '--execute=pwd'; jw switch feature-a '-xpwd'",
];

fn jj_available() -> bool {
    Command::new("jj")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

fn command_available(command: &str) -> bool {
    Command::new(command)
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
fn add_creates_multiple_workspaces_without_switching() {
    skip_without_jj!();
    let repo = TestRepo::new().expect("create test repo");

    repo.cmd()
        .args(["add", "feature-a", "feature-b"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Created workspace: feature-a"))
        .stdout(predicate::str::contains("Created workspace: feature-b"));

    assert!(repo.default_root.with_extension("feature-a").is_dir());
    assert!(repo.default_root.with_extension("feature-b").is_dir());
    assert_eq!(repo.current_workspace_name(), "default");
}

#[test]
fn add_rolls_back_earlier_workspaces_when_later_token_resolution_fails() {
    skip_without_jj!();
    let repo = TestRepo::new().expect("create test repo");

    repo.cmd()
        .args(["add", "feature-a", "-"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no previous workspace recorded"));

    assert!(!repo.workspace_names().contains(&"feature-a".to_owned()));
    assert!(!repo.default_root.with_extension("feature-a").exists());
}

#[test]
fn switch_rolls_back_intermediate_workspaces_when_final_token_resolution_fails() {
    skip_without_jj!();
    let repo = TestRepo::new().expect("create test repo");

    repo.cmd()
        .args(["switch", "feature-a", "-"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no previous workspace recorded"));

    assert!(!repo.workspace_names().contains(&"feature-a".to_owned()));
    assert!(!repo.default_root.with_extension("feature-a").exists());
}

#[test]
fn switch_can_create_multiple_workspaces_and_print_final_path() {
    skip_without_jj!();
    let repo = TestRepo::new().expect("create test repo");
    let feature_a = repo.default_root.with_extension("feature-a");
    let feature_b = repo.default_root.with_extension("feature-b");

    repo.cmd()
        .args(["switch", "feature-a", "feature-b", "--print-path"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            feature_b.to_string_lossy().as_ref(),
        ));

    assert!(feature_a.is_dir());
    assert!(feature_b.is_dir());
}

#[test]
fn switch_without_config_does_not_create_bookmark() {
    skip_without_jj!();
    let repo = TestRepo::new().expect("create test repo");

    repo.cmd()
        .args(["switch", "feature-a"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Created workspace: feature-a"))
        .stdout(predicate::str::contains("bookmark:").not());

    assert!(!repo.bookmarks().contains(&"feature-a".to_owned()));
}

#[test]
fn switch_config_can_create_bookmark_named_like_workspace() {
    skip_without_jj!();
    let repo = TestRepo::new().expect("create test repo");
    repo.write_config("[workspace]\ncreate_bookmark = true\n");

    repo.cmd()
        .args(["switch", "feature-a"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Created workspace: feature-a"))
        .stdout(predicate::str::contains("bookmark: feature-a"));

    assert!(repo.bookmarks().contains(&"feature-a".to_owned()));
}

#[test]
fn switch_config_can_template_auto_bookmark_name() {
    skip_without_jj!();
    let repo = TestRepo::new().expect("create test repo");
    repo.write_config(
        "[workspace]\ncreate_bookmark = true\nbookmark_template = \"wip/{workspace}\"\n",
    );

    repo.cmd()
        .args(["switch", "feature-a"])
        .assert()
        .success()
        .stdout(predicate::str::contains("bookmark: wip/feature-a"));

    assert!(repo.bookmarks().contains(&"wip/feature-a".to_owned()));
}

#[test]
fn switch_explicit_bookmark_overrides_config_default() {
    skip_without_jj!();
    let repo = TestRepo::new().expect("create test repo");
    repo.write_config(
        "[workspace]\ncreate_bookmark = true\nbookmark_template = \"wip/{workspace}\"\n",
    );

    repo.cmd()
        .args(["switch", "--bookmark", "custom-name", "feature-a"])
        .assert()
        .success()
        .stdout(predicate::str::contains("bookmark: custom-name"));

    let bookmarks = repo.bookmarks();
    assert!(bookmarks.contains(&"custom-name".to_owned()));
    assert!(!bookmarks.contains(&"wip/feature-a".to_owned()));
}

#[test]
fn switch_existing_workspace_does_not_echo_config_bookmark() {
    skip_without_jj!();
    let repo = TestRepo::new().expect("create test repo");
    repo.write_config("[workspace]\ncreate_bookmark = true\n");

    repo.cmd()
        .args(["switch", "feature-a"])
        .assert()
        .success()
        .stdout(predicate::str::contains("bookmark: feature-a"));

    repo.cmd()
        .args(["switch", "feature-a"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Switched workspace: feature-a"))
        .stdout(predicate::str::contains("bookmark:").not());
}

#[test]
fn switch_no_bookmark_suppresses_config_default() {
    skip_without_jj!();
    let repo = TestRepo::new().expect("create test repo");
    repo.write_config("[workspace]\ncreate_bookmark = true\n");

    repo.cmd()
        .args(["switch", "--no-bookmark", "feature-a"])
        .assert()
        .success()
        .stdout(predicate::str::contains("bookmark:").not());

    assert!(!repo.bookmarks().contains(&"feature-a".to_owned()));
}

#[test]
fn switch_rejects_bookmark_and_no_bookmark_together() {
    skip_without_jj!();
    let repo = TestRepo::new().expect("create test repo");

    repo.cmd()
        .args([
            "switch",
            "--bookmark",
            "custom-name",
            "--no-bookmark",
            "feature-a",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "--bookmark and --no-bookmark cannot be used together",
        ));
}

#[test]
fn switch_rejects_explicit_bookmark_for_batch() {
    skip_without_jj!();
    let repo = TestRepo::new().expect("create test repo");

    repo.cmd()
        .args([
            "switch",
            "--bookmark",
            "custom-name",
            "feature-a",
            "feature-b",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "--bookmark can only be used with a single workspace",
        ));
}

#[test]
fn add_rejects_explicit_bookmark_for_batch() {
    skip_without_jj!();
    let repo = TestRepo::new().expect("create test repo");

    repo.cmd()
        .args(["add", "--bookmark", "custom-name", "feature-a", "feature-b"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "--bookmark can only be used with a single workspace",
        ));
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

    repo.cmd_at(&test_root)
        .args(["path", "default"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            repo.default_root.to_string_lossy().as_ref(),
        ));
}

#[test]
fn default_shorthand_switches_to_default_workspace() {
    skip_without_jj!();
    let repo = TestRepo::new().expect("create test repo");
    let feature_root = repo.default_root.with_extension("feature-a");
    repo.cmd().args(["switch", "feature-a"]).assert().success();

    repo.cmd_at(&feature_root)
        .args(["^", "--print-path"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            repo.default_root.to_string_lossy().as_ref(),
        ));
}

#[test]
fn previous_shorthand_switches_to_previous_workspace() {
    skip_without_jj!();
    let repo = TestRepo::new().expect("create test repo");
    let feature_root = repo.default_root.with_extension("feature-a");
    repo.cmd().args(["switch", "feature-a"]).assert().success();

    repo.cmd_at(&feature_root)
        .args(["-", "--print-path"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            repo.default_root.to_string_lossy().as_ref(),
        ));
}

#[test]
fn current_uses_workspace_root_when_targets_share_working_copy() {
    skip_without_jj!();
    let repo = TestRepo::new().expect("create test repo");
    let docs_root = repo.default_root.with_extension("docs");

    run_in(
        &repo.default_root,
        [
            "jj",
            "workspace",
            "add",
            "--name",
            "docs",
            docs_root.to_str().unwrap(),
        ],
    )
    .expect("add docs workspace");
    let docs_change = current_change_id(&docs_root);
    run_in(&repo.default_root, ["jj", "edit", docs_change.as_str()]).expect("edit docs change");

    repo.cmd_at(&repo.default_root)
        .args(["current"])
        .assert()
        .success()
        .stdout("default\n");
    repo.cmd_at(&docs_root)
        .args(["current"])
        .assert()
        .success()
        .stdout("docs\n");
}

#[test]
fn previous_shorthand_survives_shared_working_copy_target() {
    skip_without_jj!();
    let repo = TestRepo::new().expect("create test repo");
    let docs_root = repo.default_root.with_extension("docs");
    repo.cmd().args(["switch", "docs"]).assert().success();

    let docs_change = current_change_id(&docs_root);
    run_in(&repo.default_root, ["jj", "edit", docs_change.as_str()]).expect("edit docs change");

    repo.cmd_at(&docs_root)
        .args(["switch", "default", "--print-path"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            repo.default_root.to_string_lossy().as_ref(),
        ));
    repo.cmd_at(&repo.default_root)
        .args(["-", "--print-path"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            docs_root.to_string_lossy().as_ref(),
        ));
}

#[test]
fn previous_shorthand_rejects_multiline_state_record() {
    skip_without_jj!();
    let repo = TestRepo::new().expect("create test repo");
    fs::write(
        repo.default_root.join(".jj").join("jw-prev-workspace"),
        "default\ndocs\n",
    )
    .expect("write corrupt previous record");

    repo.cmd()
        .args(["-", "--print-path"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "previous workspace record is invalid",
        ));
}

#[test]
fn installed_shell_init_adapters_switch_default_and_previous_shorthands() {
    skip_without_jj!();

    let cases = [
        ShellCase {
            name: "bash",
            program: "bash",
            args: &[
                "--noprofile",
                "--norc",
                "-c",
                "eval \"$(jw shell init bash)\"; jw ^ >/dev/null; pwd; jw - >/dev/null; pwd",
            ],
            execute_args: &[
                "--noprofile",
                "--norc",
                "-c",
                "eval \"$(jw shell init bash)\"; jw switch feature-a --execute=pwd; jw switch feature-a -xpwd",
            ],
        },
        ShellCase {
            name: "elvish",
            program: "elvish",
            args: &[
                "-norc",
                "-c",
                "eval (jw shell init elvish | slurp); jw '^' > /dev/null; pwd; jw '-' > /dev/null; pwd",
            ],
            execute_args: &[
                "-norc",
                "-c",
                "eval (jw shell init elvish | slurp); jw switch feature-a '--execute=pwd'; jw switch feature-a '-xpwd'",
            ],
        },
        ShellCase {
            name: "fish",
            program: "fish",
            args: &[
                "--no-config",
                "-c",
                "jw shell init fish | source; jw ^ >/dev/null; pwd; jw - >/dev/null; pwd",
            ],
            execute_args: &[
                "--no-config",
                "-c",
                "jw shell init fish | source; jw switch feature-a --execute=pwd; jw switch feature-a -xpwd",
            ],
        },
        ShellCase {
            name: "powershell",
            program: "powershell",
            args: POWERSHELL_SWITCH_TEST_ARGS,
            execute_args: POWERSHELL_EXECUTE_TEST_ARGS,
        },
        ShellCase {
            name: "powershell",
            program: "pwsh",
            args: POWERSHELL_SWITCH_TEST_ARGS,
            execute_args: POWERSHELL_EXECUTE_TEST_ARGS,
        },
        ShellCase {
            name: "zsh",
            program: "zsh",
            args: &[
                "-f",
                "-c",
                "eval \"$(jw shell init zsh)\"; jw ^ >/dev/null; pwd; jw - >/dev/null; pwd",
            ],
            execute_args: &[
                "-f",
                "-c",
                "eval \"$(jw shell init zsh)\"; jw switch feature-a --execute=pwd; jw switch feature-a -xpwd",
            ],
        },
    ];

    for case in cases {
        if !command_available(case.program) {
            eprintln!(
                "skipping {} because `{}` is not installed",
                case.name, case.program
            );
            continue;
        }

        let repo = TestRepo::new().expect("create test repo");
        let feature_root = repo.default_root.with_extension("feature-a");
        repo.cmd().args(["switch", "feature-a"]).assert().success();

        let output = Command::new(case.program)
            .current_dir(&feature_root)
            .env("PATH", test_binary_path())
            .env("XDG_CONFIG_HOME", &repo.config_home)
            .args(case.args)
            .output()
            .unwrap_or_else(|error| panic!("run {} shell integration: {error}", case.name));

        assert!(
            output.status.success(),
            "{} shell integration failed\nstdout: {}\nstderr: {}",
            case.name,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let lines = String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(ToOwned::to_owned)
            .collect::<Vec<_>>();
        assert_eq!(
            lines,
            vec![path_string(&repo.default_root), path_string(&feature_root)],
            "{} shell integration returned wrong directories",
            case.name
        );

        let execute = Command::new(case.program)
            .current_dir(&repo.default_root)
            .env("PATH", test_binary_path())
            .env("XDG_CONFIG_HOME", &repo.config_home)
            .args(case.execute_args)
            .output()
            .unwrap_or_else(|error| {
                panic!("run {} attached execute integration: {error}", case.name)
            });
        assert!(
            execute.status.success(),
            "{} attached execute integration failed\nstdout: {}\nstderr: {}",
            case.name,
            String::from_utf8_lossy(&execute.stdout),
            String::from_utf8_lossy(&execute.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&execute.stdout)
                .lines()
                .map(ToOwned::to_owned)
                .collect::<Vec<_>>(),
            vec![path_string(&feature_root), path_string(&feature_root)],
            "{} shell adapter did not pass attached execute forms through",
            case.name
        );
    }
}

#[test]
fn list_accepts_ls_alias() {
    skip_without_jj!();
    let repo = TestRepo::new().expect("create test repo");

    repo.cmd()
        .args(["ls"])
        .assert()
        .success()
        .stdout(predicate::str::contains("default"));
}

#[test]
fn list_reports_missing_checkout_without_hiding_other_root_errors() {
    skip_without_jj!();
    let repo = TestRepo::new().expect("create test repo");
    repo.cmd().args(["add", "feature-a"]).assert().success();
    let workspace = repo.default_root.with_extension("feature-a");
    fs::rename(
        &workspace,
        repo.default_root.with_extension("feature-a.gone"),
    )
    .expect("move checkout away");

    repo.cmd()
        .args(["list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("feature-a\t(missing)"));
}

#[test]
fn completions_command_generates_clap_registration_for_every_shell() {
    for shell in ["bash", "elvish", "fish", "powershell", "zsh"] {
        Command::cargo_bin("jw")
            .expect("binary")
            .args(["shell", "completions", shell])
            .assert()
            .success()
            .stdout(predicate::str::contains("_JW_COMPLETE"));

        Command::cargo_bin("jw")
            .expect("binary")
            .args(["completions", shell])
            .assert()
            .success()
            .stdout(predicate::str::contains("_JW_COMPLETE"));
    }
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
fn remove_prompts_before_deleting_associated_bookmark() {
    skip_without_jj!();
    let repo = TestRepo::new().expect("create test repo");

    repo.cmd()
        .args(["switch", "--bookmark", "custom-marker", "feature-a"])
        .assert()
        .success();
    let feature_root = repo.default_root.with_extension("feature-a");
    fs::write(feature_root.join("work.txt"), "work\n").expect("write workspace file");
    run_in(&feature_root, ["jj", "file", "track", "work.txt"]).expect("track workspace file");
    run_in(&feature_root, ["jj", "commit", "-m", "workspace work"]).expect("commit workspace work");

    let mut remove = repo.cmd();
    remove.args(["remove", "feature-a"]);
    assert_cmd::Command::from_std(remove)
        .write_stdin("y\n")
        .assert()
        .success()
        .stderr(predicate::str::contains(
            "Delete associated bookmark 'custom-marker'? [y/N]",
        ))
        .stdout(predicate::str::contains("Deleted bookmark: custom-marker"));

    assert!(!repo.bookmarks().contains(&"custom-marker".to_owned()));
}

#[test]
fn remove_keeps_associated_bookmark_when_prompt_is_declined() {
    skip_without_jj!();
    let repo = TestRepo::new().expect("create test repo");

    repo.cmd()
        .args(["switch", "--bookmark", "wip/feature-a", "feature-a"])
        .assert()
        .success();

    let mut remove = repo.cmd();
    remove.args(["remove", "feature-a"]);
    assert_cmd::Command::from_std(remove)
        .write_stdin("n\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("Deleted bookmark:").not());

    assert!(repo.bookmarks().contains(&"wip/feature-a".to_owned()));
}

#[test]
fn remove_bookmark_flags_support_noninteractive_callers() {
    skip_without_jj!();
    let repo = TestRepo::new().expect("create test repo");

    repo.cmd()
        .args(["add", "--bookmark", "wip/feature-a", "feature-a"])
        .assert()
        .success();
    repo.cmd()
        .args(["remove", "--keep-bookmark", "feature-a"])
        .assert()
        .success();
    assert!(repo.bookmarks().contains(&"wip/feature-a".to_owned()));

    repo.cmd()
        .args(["add", "--bookmark", "wip/feature-b", "feature-b"])
        .assert()
        .success();
    repo.cmd()
        .args(["remove", "--delete-bookmark", "feature-b"])
        .assert()
        .success();
    assert!(!repo.bookmarks().contains(&"wip/feature-b".to_owned()));
}

#[test]
fn remove_does_not_treat_unrelated_bookmark_at_same_commit_as_associated() {
    skip_without_jj!();
    let repo = TestRepo::new().expect("create test repo");

    repo.cmd().args(["add", "feature-a"]).assert().success();
    repo.run_jj(["bookmark", "create", "unrelated", "-r", "feature-a@"]);
    repo.cmd()
        .args(["remove", "--delete-bookmark", "feature-a"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Deleted bookmark:").not());

    assert!(repo.bookmarks().contains(&"unrelated".to_owned()));
}

#[test]
fn remove_deletes_multiple_workspace_directories_by_default() {
    skip_without_jj!();
    let repo = TestRepo::new().expect("create test repo");
    let feature_a = repo.default_root.with_extension("feature-a");
    let feature_b = repo.default_root.with_extension("feature-b");

    repo.cmd()
        .args(["add", "feature-a", "feature-b"])
        .assert()
        .success();

    repo.cmd()
        .args(["remove", "feature-a", "feature-b"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Forgot workspace: feature-a"))
        .stdout(predicate::str::contains("Forgot workspace: feature-b"));

    assert!(!feature_a.exists());
    assert!(!feature_b.exists());
}

#[test]
fn remove_keep_dir_preserves_multiple_workspace_directories() {
    skip_without_jj!();
    let repo = TestRepo::new().expect("create test repo");
    let feature_a = repo.default_root.with_extension("feature-a");
    let feature_b = repo.default_root.with_extension("feature-b");

    repo.cmd()
        .args(["add", "feature-a", "feature-b"])
        .assert()
        .success();

    repo.cmd()
        .args(["remove", "--keep-dir", "feature-a", "feature-b"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Deleted directory:").not());

    assert!(feature_a.is_dir());
    assert!(feature_b.is_dir());
}

#[test]
fn remove_batch_validates_every_workspace_before_mutating() {
    skip_without_jj!();
    let repo = TestRepo::new().expect("create test repo");
    let feature_a = repo.default_root.with_extension("feature-a");
    let feature_b = repo.default_root.with_extension("feature-b");

    repo.cmd()
        .args(["add", "feature-a", "feature-b"])
        .assert()
        .success();

    repo.cmd()
        .args(["remove", "feature-a", "missing", "feature-b"])
        .assert()
        .failure()
        .stdout(predicate::str::contains("Forgot workspace:").not())
        .stdout(predicate::str::contains("Forgot workspace: feature-b").not())
        .stderr(predicate::str::contains(
            "failed to remove workspace missing",
        ));

    assert!(feature_a.is_dir());
    assert!(feature_b.is_dir());
}

#[test]
fn remove_batch_rejects_duplicate_workspace_before_mutating() {
    skip_without_jj!();
    let repo = TestRepo::new().expect("create test repo");
    let feature = repo.default_root.with_extension("feature-a");
    repo.cmd().args(["add", "feature-a"]).assert().success();

    repo.cmd()
        .args(["remove", "feature-a", "feature-a"])
        .assert()
        .failure()
        .stdout(predicate::str::contains("Forgot workspace:").not())
        .stderr(predicate::str::contains(
            "workspace listed more than once: feature-a",
        ));

    assert!(feature.is_dir());
    assert!(repo.workspace_names().contains(&"feature-a".to_owned()));
}

#[test]
fn clap_completion_combines_cli_metadata_and_workspace_candidates() {
    skip_without_jj!();
    let repo = TestRepo::new().expect("create test repo");

    repo.cmd().args(["switch", "feature-a"]).assert().success();

    repo.cmd()
        .env("_JW_COMPLETE", "fish")
        .args(["--", "jw", "sw"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "switch\tSwitch to or create a workspace",
        ));

    repo.cmd()
        .env("_JW_COMPLETE", "fish")
        .args(["--", "jw", "switch", "fea"])
        .assert()
        .success()
        .stdout(predicate::str::contains("feature-a\tExisting workspace"));

    repo.cmd()
        .env("_JW_COMPLETE", "fish")
        .args(["--", "jw", "switch", ""])
        .assert()
        .success()
        .stdout(predicate::str::contains("@\tCurrent workspace"))
        .stdout(predicate::str::contains("-\tPrevious workspace"))
        .stdout(predicate::str::contains("^\tDefault workspace"));

    repo.cmd()
        .env("_JW_COMPLETE", "fish")
        .args(["--", "jw", "remove", "--k"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "--keep-dir\tForget the workspace but keep its directory",
        ));

    repo.cmd()
        .env("_JW_COMPLETE", "fish")
        .args(["--", "jw", "-"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "-\tSwitch to or create a workspace",
        ));
}

#[test]
fn clap_completion_reports_workspace_discovery_errors() {
    skip_without_jj!();
    let directory = TempDir::new().expect("create temporary directory");

    Command::cargo_bin("jw")
        .expect("binary")
        .current_dir(directory.path())
        .env("_JW_COMPLETE", "fish")
        .args(["--", "jw", "switch", ""])
        .assert()
        .success()
        .stderr(predicate::str::contains(
            "jw: failed to complete workspaces",
        ));
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
fn add_rolls_back_workspace_when_required_link_is_missing() {
    skip_without_jj!();
    let repo = TestRepo::new().expect("create test repo");
    let workspace_root = repo.default_root.with_extension("feature-a");
    fs::write(
        repo.default_root.join(".jwlinks.toml"),
        "[[link]]\nsource = \"data\"\ntarget = \"../repo/missing\"\nrequired = true\n",
    )
    .expect("write links config");

    repo.cmd()
        .args(["add", "feature-a"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("required link target is missing"));

    assert!(!workspace_root.exists());
    assert!(!repo.workspace_names().contains(&"feature-a".to_owned()));
}

#[test]
fn switch_rolls_back_workspace_before_recording_previous_on_link_failure() {
    skip_without_jj!();
    let repo = TestRepo::new().expect("create test repo");
    fs::write(
        repo.default_root.join(".jwlinks.toml"),
        "[[link]]\nsource = \"data\"\ntarget = \"../repo/missing\"\nrequired = true\n",
    )
    .expect("write links config");

    repo.cmd().args(["switch", "feature-a"]).assert().failure();

    assert!(!repo.workspace_names().contains(&"feature-a".to_owned()));
    repo.cmd()
        .arg("-")
        .assert()
        .failure()
        .stderr(predicate::str::contains("no previous workspace recorded"));
}

#[test]
fn switch_rolls_back_links_when_workspace_state_cannot_be_recorded() {
    skip_without_jj!();
    let repo = TestRepo::new().expect("create test repo");
    fs::create_dir_all(repo.default_root.join("data")).expect("create link target");
    fs::write(
        repo.default_root.join(".jwlinks.toml"),
        "[[link]]\nsource = \"nested/data\"\ntarget = \"../repo/data\"\nrequired = true\n",
    )
    .expect("write links config");
    repo.cmd()
        .args(["add", "--no-links", "feature-a"])
        .assert()
        .success();
    let feature_root = repo.default_root.with_extension("feature-a");
    fs::create_dir(feature_root.join(".jj/jw-prev-workspace")).expect("block target state file");

    repo.cmd()
        .args(["switch", "feature-a"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "failed to inspect workspace state",
        ));

    assert!(!repo.default_root.join(".jj/jw-prev-workspace").exists());
    assert!(!feature_root.join("nested/data").exists());
    assert!(!feature_root.join("nested").exists());
}

#[cfg(unix)]
#[test]
fn switch_restores_source_state_when_target_state_write_fails() {
    use std::os::unix::fs::PermissionsExt;

    skip_without_jj!();
    let repo = TestRepo::new().expect("create test repo");
    repo.cmd().args(["add", "feature-a"]).assert().success();
    let source_state = repo.default_root.join(".jj/jw-prev-workspace");
    let target_state = repo
        .default_root
        .with_extension("feature-a")
        .join(".jj/jw-prev-workspace");
    fs::write(&source_state, "older\n").expect("write source state");
    fs::write(&target_state, "target-old\n").expect("write target state");
    let mut permissions = fs::metadata(&target_state)
        .expect("target state metadata")
        .permissions();
    permissions.set_mode(0o444);
    fs::set_permissions(&target_state, permissions).expect("make target state readonly");

    repo.cmd()
        .args(["switch", "--no-links", "feature-a"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("failed to record workspace state"));

    let mut permissions = fs::metadata(&target_state)
        .expect("target state metadata")
        .permissions();
    permissions.set_mode(0o644);
    fs::set_permissions(&target_state, permissions).expect("restore target permissions");
    assert_eq!(
        fs::read_to_string(source_state).expect("read restored source state"),
        "older\n"
    );
}

#[test]
fn add_cleans_workspace_when_bookmark_creation_fails() {
    skip_without_jj!();
    let repo = TestRepo::new().expect("create test repo");
    let workspace_root = repo.default_root.with_extension("feature-a");

    repo.cmd()
        .args(["add", "--bookmark", "", "feature-a"])
        .assert()
        .failure();

    assert!(!workspace_root.exists());
    assert!(!repo.workspace_names().contains(&"feature-a".to_owned()));
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

    repo.cmd_at(&repo.default_root.with_extension("solver-benchmark"))
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
fn links_apply_uses_default_workspace_config_from_non_default_workspace() {
    skip_without_jj!();
    let repo = TestRepo::new().expect("create test repo");
    repo.cmd()
        .args(["switch", "feature-a", "--no-links"])
        .assert()
        .success();

    fs::create_dir_all(repo.default_root.join("data")).expect("create data directory");
    fs::write(
        repo.default_root.join(".jwlinks.local.toml"),
        "[[link]]\nsource = \"data\"\ntarget = \"../repo/data\"\nrequired = true\n",
    )
    .expect("write default workspace links config");

    let feature_root = repo.default_root.with_extension("feature-a");
    repo.cmd_at(&feature_root)
        .args(["links", "apply"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Links: 1 created"));

    let metadata = fs::symlink_metadata(feature_root.join("data")).expect("link metadata");
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
    repo.cmd_at(&target_root)
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

    repo.cmd_at(&feature_a_root)
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
    config_home: PathBuf,
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
        let config_home = tempdir.path().join("config");

        Ok(Self {
            _tempdir: tempdir,
            default_root,
            config_home,
        })
    }

    fn cmd(&self) -> Command {
        self.cmd_at(&self.default_root)
    }

    fn cmd_at(&self, cwd: &Path) -> Command {
        let mut cmd = Command::cargo_bin("jw").expect("binary");
        cmd.current_dir(cwd)
            .env("XDG_CONFIG_HOME", &self.config_home);
        cmd
    }

    fn run_jj<const N: usize>(&self, args: [&str; N]) {
        run_in(
            &self.default_root,
            std::iter::once("jj").chain(args).collect::<Vec<_>>(),
        )
        .expect("jj command succeeds");
    }

    fn write_config(&self, contents: &str) {
        let config_dir = self.config_home.join("jj-waltz");
        fs::create_dir_all(&config_dir).expect("create config dir");
        fs::write(config_dir.join("config.toml"), contents).expect("write config");
    }

    fn bookmarks(&self) -> Vec<String> {
        let output = Command::new("jj")
            .current_dir(&self.default_root)
            .args(["bookmark", "list", "-T", "name ++ \"\\n\"", "--color=never"])
            .output()
            .expect("list bookmarks");
        assert!(
            output.status.success(),
            "jj bookmark list failed\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(ToOwned::to_owned)
            .collect()
    }

    fn workspace_names(&self) -> Vec<String> {
        let output = Command::new("jj")
            .current_dir(&self.default_root)
            .args([
                "workspace",
                "list",
                "-T",
                "name ++ \"\\n\"",
                "--color=never",
            ])
            .output()
            .expect("list workspaces");
        assert!(
            output.status.success(),
            "jj workspace list failed\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(ToOwned::to_owned)
            .collect()
    }

    fn current_workspace_name(&self) -> String {
        let current_root = Command::new("jj")
            .current_dir(&self.default_root)
            .args(["workspace", "root"])
            .output()
            .expect("current workspace root");
        assert!(
            current_root.status.success(),
            "jj workspace root failed\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&current_root.stdout),
            String::from_utf8_lossy(&current_root.stderr)
        );
        let current_root = fs::canonicalize(String::from_utf8_lossy(&current_root.stdout).trim())
            .expect("canonicalize current root");

        let output = Command::new("jj")
            .current_dir(&self.default_root)
            .args([
                "workspace",
                "list",
                "-T",
                "name ++ \"\\n\"",
                "--color=never",
            ])
            .output()
            .expect("list workspaces");
        assert!(
            output.status.success(),
            "jj workspace list failed\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let names = String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(ToOwned::to_owned)
            .collect::<Vec<_>>();

        names
            .into_iter()
            .find(|name| {
                let output = Command::new("jj")
                    .current_dir(&self.default_root)
                    .args(["workspace", "root", "--name", name])
                    .output()
                    .expect("workspace root by name");
                if !output.status.success() {
                    return false;
                }
                let Ok(root) = fs::canonicalize(String::from_utf8_lossy(&output.stdout).trim())
                else {
                    return false;
                };
                root == current_root
            })
            .expect("current workspace name")
    }
}

fn current_change_id(cwd: &Path) -> String {
    let output = Command::new("jj")
        .current_dir(cwd)
        .args(["log", "--no-graph", "-r", "@", "-T", "change_id.short()"])
        .output()
        .expect("current change id");
    assert!(
        output.status.success(),
        "jj log failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn test_binary_path() -> OsString {
    let binary = assert_cmd::cargo::cargo_bin("jw");
    let binary_dir = binary.parent().expect("binary has parent");
    let mut paths = vec![binary_dir.to_path_buf()];
    paths.extend(env::split_paths(&env::var_os("PATH").unwrap_or_default()));
    env::join_paths(paths).expect("join PATH")
}

fn path_string(path: &Path) -> String {
    fs::canonicalize(path)
        .expect("canonicalize path")
        .to_string_lossy()
        .into_owned()
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
