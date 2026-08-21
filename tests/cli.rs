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

    repo.run_in(
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
    let docs_change = repo.current_change_id(&docs_root);
    repo.run_in(&repo.default_root, ["jj", "edit", docs_change.as_str()])
        .expect("edit docs change");

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

    let docs_change = repo.current_change_id(&docs_root);
    repo.run_in(&repo.default_root, ["jj", "edit", docs_change.as_str()])
        .expect("edit docs change");

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
fn list_infers_current_default_without_recorded_path() {
    skip_without_jj!();
    let repo = TestRepo::new().expect("create test repo");
    fs::remove_dir_all(repo.default_root.join(".jj/repo/workspace_store"))
        .expect("remove recorded workspace paths");

    let named_root = Command::new("jj")
        .current_dir(&repo.default_root)
        .args(["workspace", "root", "--name", "default"])
        .output()
        .expect("query named workspace root");
    assert!(!named_root.status.success());
    assert!(
        String::from_utf8_lossy(&named_root.stderr)
            .contains("Workspace has no recorded path: default")
    );

    let expected = format!("@ default\t{}", path_string(&repo.default_root));
    repo.cmd()
        .args(["list"])
        .assert()
        .success()
        .stdout(predicate::str::contains(expected))
        .stdout(predicate::str::contains("(missing)").not());
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

#[cfg(unix)]
#[test]
fn remove_reports_partial_progress_and_cleans_metadata_when_directory_remains() {
    use std::os::unix::fs::PermissionsExt;

    skip_without_jj!();
    let repo = TestRepo::new().expect("create test repo");
    let workspace_root = repo.default_root.with_extension("feature-a");
    repo.cmd()
        .args(["add", "--bookmark", "wip/feature-a", "feature-a"])
        .assert()
        .success();
    let canonical_workspace_root =
        fs::canonicalize(&workspace_root).expect("canonical workspace path");

    let parent = repo._tempdir.path();
    let original_permissions = fs::metadata(parent)
        .expect("workspace parent metadata")
        .permissions();
    let mut blocked_permissions = original_permissions.clone();
    blocked_permissions.set_mode(0o500);
    fs::set_permissions(parent, blocked_permissions).expect("block directory deletion");

    let output = repo.command_output(&["remove", "--delete-bookmark", "feature-a"]);

    fs::set_permissions(parent, original_permissions)
        .expect("restore workspace parent permissions");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(
            "partial removal: workspace feature-a was forgotten and bookmark wip/feature-a was deleted"
        ),
        "stderr: {stderr}"
    );
    assert!(
        stderr.contains(&format!(
            "workspace directory remains at {}",
            canonical_workspace_root.display()
        )),
        "stderr: {stderr}"
    );
    assert!(workspace_root.is_dir());
    assert!(!repo.workspace_names().contains(&"feature-a".to_owned()));
    assert!(!repo.bookmarks().contains(&"wip/feature-a".to_owned()));
    assert!(repo.metadata_record_paths().is_empty());
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
    repo.run_in(&feature_root, ["jj", "file", "track", "work.txt"])
        .expect("track workspace file");
    repo.run_in(&feature_root, ["jj", "commit", "-m", "workspace work"])
        .expect("commit workspace work");

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
fn remove_infers_only_conventional_local_bookmark_for_unmanaged_workspace() {
    skip_without_jj!();
    let repo = TestRepo::new().expect("create test repo");
    let workspace_root = repo.default_root.with_extension("legacy");

    repo.run_in(
        &repo.default_root,
        [
            "jj",
            "workspace",
            "add",
            "--name",
            "legacy",
            workspace_root.to_str().expect("UTF-8 workspace path"),
        ],
    )
    .expect("create unmanaged workspace");
    repo.run_jj(["bookmark", "create", "wip/legacy", "-r", "legacy@"]);
    repo.run_jj(["bookmark", "create", "unrelated", "-r", "legacy@"]);

    repo.cmd()
        .args(["remove", "--delete-bookmark", "legacy"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Deleted bookmark: wip/legacy"));

    let bookmarks = repo.bookmarks();
    assert!(!bookmarks.contains(&"wip/legacy".to_owned()));
    assert!(bookmarks.contains(&"unrelated".to_owned()));
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
    assert!(repo.metadata_record_paths().is_empty());
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
    assert!(repo.metadata_record_paths().is_empty());
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
    repo.run_in(&feature_a_root, ["jj", "file", "track", "feature.txt"])
        .expect("track file");
    repo.run_in(&feature_a_root, ["jj", "commit", "-m", "feature-a commit"])
        .expect("commit change");

    repo.cmd_at(&feature_a_root)
        .args(["switch", "default"])
        .assert()
        .success();

    repo.cmd().args(["switch", "feature-b"]).assert().success();
    let feature_b_data = repo.default_root.with_extension("feature-b").join("data");
    let metadata = fs::symlink_metadata(&feature_b_data).expect("metadata");
    assert!(metadata.file_type().is_symlink());
}

#[test]
fn plain_list_aliases_keep_legacy_bytes_and_ignore_semantic_config() {
    skip_without_jj!();
    let repo = TestRepo::new().expect("create test repo");
    let baseline = repo.command_output(&["list"]);
    assert!(baseline.status.success());
    assert!(baseline.stderr.is_empty());

    for alias in ["l", "ls"] {
        let output = repo.command_output(&[alias]);
        assert!(output.status.success());
        assert_eq!(output.stdout, baseline.stdout, "alias {alias}");
        assert_eq!(output.stderr, baseline.stderr, "alias {alias}");
    }

    repo.write_config("[trunk]\nrevset = \"\"\n");
    let metadata_root = repo.metadata_root();
    fs::create_dir_all(&metadata_root).expect("create metadata root");
    fs::write(metadata_root.join("manifest.json"), b"{not json\n").expect("write corrupt metadata");

    let output = repo.command_output(&["list"]);
    assert!(output.status.success());
    assert_eq!(output.stdout, baseline.stdout);
    assert_eq!(output.stderr, baseline.stderr);
}

#[test]
fn list_json_is_versioned_frozen_ansi_free_and_unmanaged() {
    skip_without_jj!();
    let repo = TestRepo::new().expect("create test repo");
    let output = repo.command_output(&["list", "--format=json"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    assert!(!output.stdout.contains(&0x1b));

    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid list JSON");
    assert_eq!(value["schema_version"], 1);
    assert_eq!(value["command"], "list");
    assert_eq!(value["repository"]["trunk"]["revset"], "trunk()");
    assert!(
        value["repository"]["operation_id"]
            .as_str()
            .is_some_and(|id| !id.is_empty())
    );
    assert_eq!(value["workspaces"][0]["management"], "unmanaged");
    assert_eq!(value["workspaces"][0]["working_copy_refreshed"], true);
}

#[test]
fn status_refresh_none_is_read_only_and_aliases_resolve() {
    skip_without_jj!();
    let repo = TestRepo::new().expect("create test repo");
    fs::write(repo.default_root.join("dirty.txt"), "not snapshotted\n")
        .expect("write dirty working-copy file");
    let before = repo.operation_id();

    for token in ["@", "default", "^"] {
        let output = repo.command_output(&["status", token, "--format=json", "--refresh=none"]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let value: serde_json::Value =
            serde_json::from_slice(&output.stdout).expect("valid status JSON");
        assert_eq!(value["command"], "status");
        assert_eq!(value["workspaces"][0]["name"], "default");
        assert_eq!(value["workspaces"][0]["working_copy_refreshed"], false);
        assert_eq!(value["workspaces"][0]["working_copy"]["state"], "unknown");
    }

    assert_eq!(repo.operation_id(), before);
}

#[test]
fn status_refresh_current_classifies_modified_working_copy() {
    skip_without_jj!();
    let repo = TestRepo::new().expect("create test repo");
    fs::write(repo.default_root.join("work.txt"), "new work\n").expect("write work");

    let output = repo.command_output(&["status", "--format=json"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("valid status JSON");
    assert_eq!(value["workspaces"][0]["working_copy_refreshed"], true);
    assert_eq!(value["workspaces"][0]["working_copy"]["state"], "modified");
    assert!(
        value["workspaces"][0]["working_copy"]["files"]
            .as_u64()
            .is_some_and(|files| files >= 1)
    );
}

#[test]
fn list_and_status_reject_non_exact_trunk_before_stdout() {
    skip_without_jj!();
    let repo = TestRepo::new().expect("create test repo");

    for revset in ["none()", "all()"] {
        repo.write_config(&format!("[trunk]\nrevset = \"{revset}\"\n"));
        for args in [
            &["list", "--format=json"][..],
            &["status", "--format=json", "--refresh=none"][..],
        ] {
            let output = repo.command_output(args);
            assert!(!output.status.success(), "{revset} {args:?}");
            assert!(output.stdout.is_empty(), "{revset} {args:?}");
            assert!(
                String::from_utf8_lossy(&output.stderr).contains("exactly one revision"),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
}

#[test]
fn doctor_serializes_bad_trunk_and_corrupt_metadata_before_failure() {
    skip_without_jj!();
    let repo = TestRepo::new().expect("create test repo");
    repo.write_config("[trunk]\nrevset = \"none()\"\n");

    let output = repo.command_output(&["doctor", "--format=json"]);
    assert!(!output.status.success());
    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("valid doctor JSON");
    assert_eq!(value["command"], "doctor");
    assert_eq!(value["healthy"], false);
    assert!(value["summary"]["errors"].as_u64().unwrap() >= 1);

    repo.write_config("[trunk]\nrevset = \"trunk()\"\n");
    let metadata_root = repo.metadata_root();
    fs::create_dir_all(&metadata_root).expect("create metadata root");
    let manifest = metadata_root.join("manifest.json");
    let corrupt = b"{still corrupt\n";
    fs::write(&manifest, corrupt).expect("write corrupt metadata");
    let output = repo.command_output(&["doctor", "--format=json"]);
    assert!(!output.status.success());
    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("valid corrupt-metadata report");
    assert_eq!(value["healthy"], false);
    assert!(
        value["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| { entry["code"] == "metadata-integrity" && entry["state"] == "failed" })
    );
    assert_eq!(
        fs::read(&manifest).expect("read unchanged manifest"),
        corrupt
    );
}

#[test]
fn doctor_serializes_configuration_load_failures_before_failure() {
    skip_without_jj!();

    for malformed in [true, false] {
        let repo = TestRepo::new().expect("create test repo");
        let config_path = repo.config_home.join("jj-waltz/config.toml");
        fs::create_dir_all(config_path.parent().expect("config parent"))
            .expect("create config parent");
        if malformed {
            fs::write(&config_path, "[trunk\nrevset = \"trunk()\"\n")
                .expect("write malformed config");
        } else {
            fs::create_dir(&config_path).expect("create unreadable config path");
        }

        let output = repo.command_output(&["doctor", "--format=json"]);
        assert!(!output.status.success());
        let value: serde_json::Value =
            serde_json::from_slice(&output.stdout).expect("valid doctor JSON");
        assert_eq!(value["command"], "doctor");
        assert_eq!(value["healthy"], false);
        assert!(
            value["diagnostics"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| { entry["code"] == "configuration" && entry["state"] == "failed" })
        );
        assert!(
            value["diagnostics"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| { entry["code"] == "trunk-revset" && entry["state"] == "skipped" })
        );
    }
}

#[test]
fn doctor_reports_managed_workspace_link_health_in_json_and_plain_output() {
    skip_without_jj!();
    let repo = TestRepo::new().expect("create test repo");
    fs::write(
        repo.default_root.join(".jwlinks.toml"),
        "[[link]]\nsource = \"node_modules\"\ntarget = \"../repo/missing\"\nrequired = false\n",
    )
    .expect("write links config");

    repo.cmd()
        .args(["add", "--no-links", "private", "skipped"])
        .assert()
        .success();
    fs::create_dir_all(
        repo.default_root
            .with_extension("private")
            .join("node_modules"),
    )
    .expect("create private link source");
    let operation = repo.operation_id();

    let output = repo.command_output(&["doctor", "--format=json"]);
    assert!(!output.status.success());
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("valid doctor JSON");
    assert_eq!(report["schema_version"], 1);
    assert_eq!(report["healthy"], false);
    let diagnostics = report["diagnostics"].as_array().expect("diagnostics array");
    assert!(diagnostics.iter().any(|entry| {
        entry["code"] == "workspace-link"
            && entry["subject"] == "private:node_modules"
            && entry["state"] == "failed"
            && entry["severity"] == "error"
    }));
    assert!(diagnostics.iter().any(|entry| {
        entry["code"] == "workspace-link"
            && entry["subject"] == "skipped:node_modules"
            && entry["state"] == "skipped"
            && entry["severity"] == "warning"
    }));

    repo.cmd()
        .arg("doctor")
        .assert()
        .failure()
        .stdout(predicate::str::contains(
            "FAIL workspace-link [private:node_modules]",
        ))
        .stdout(predicate::str::contains(
            "WARN workspace-link [skipped:node_modules]",
        ));
    assert_eq!(repo.operation_id(), operation);
}

#[test]
fn doctor_uses_current_root_when_default_has_no_recorded_path() {
    skip_without_jj!();
    let repo = TestRepo::new().expect("create test repo");
    repo.cmd()
        .args(["adopt", "default", "--base", "@-", "--no-bookmark"])
        .assert()
        .success();
    fs::write(
        repo.default_root.join(".jwlinks.toml"),
        "[[link]]\nsource = \"cache\"\ntarget = \"missing\"\nrequired = false\n",
    )
    .expect("write links config");
    fs::remove_dir_all(repo.default_root.join(".jj/repo/workspace_store"))
        .expect("remove recorded workspace paths");

    let output = repo.command_output(&["doctor", "--format=json"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("valid doctor JSON");
    assert_eq!(report["healthy"], true);
    assert!(
        report["diagnostics"]
            .as_array()
            .expect("diagnostics array")
            .iter()
            .any(|entry| {
                entry["code"] == "workspace-link"
                    && entry["subject"] == "default:cache"
                    && entry["state"] == "skipped"
                    && entry["severity"] == "warning"
            })
    );
}

#[test]
fn creation_metadata_records_exact_base_prebookmark_operation_and_removal_cleans_it() {
    skip_without_jj!();
    let repo = TestRepo::new().expect("create test repo");
    let expected_base = repo.revision_commit_id("@-");

    let output = repo.command_output(&[
        "add",
        "feature-a",
        "--no-links",
        "--bookmark",
        "wip/feature-a",
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let creation_operation = repo.operation_id();
    let status = repo.command_output(&["status", "feature-a", "--format=json", "--refresh=none"]);
    assert!(status.status.success());
    let value: serde_json::Value =
        serde_json::from_slice(&status.stdout).expect("valid managed status");
    let workspace = &value["workspaces"][0];
    assert_eq!(workspace["management"], "managed");
    assert_eq!(workspace["creation_base_commit_id"], expected_base);
    assert!(
        workspace["creation_operation_id"]
            .as_str()
            .is_some_and(|operation| !operation.is_empty() && operation != creation_operation)
    );
    assert_eq!(workspace["associated_bookmark"], "wip/feature-a");

    repo.cmd()
        .args(["remove", "feature-a", "--keep-bookmark"])
        .assert()
        .success();
    assert!(repo.metadata_record_paths().is_empty());
}

#[test]
fn merge_working_copy_requires_explicit_creation_base() {
    skip_without_jj!();
    let repo = TestRepo::new().expect("create test repo");
    repo.run_jj(["new", "root()", "-m", "left"]);
    let left = repo.revision_commit_id("@");
    repo.run_jj(["new", "root()", "-m", "right"]);
    let right = repo.revision_commit_id("@");
    repo.run_jj(["new", &left, &right, "-m", "merge"]);
    let merge = repo.revision_commit_id("@");
    let before = repo.operation_id();

    let implicit = repo.command_output(&["add", "implicit", "--no-links"]);
    assert!(!implicit.status.success());
    assert!(String::from_utf8_lossy(&implicit.stderr).contains("use --at @"));
    assert_eq!(repo.operation_id(), before);
    assert!(!repo.workspace_names().contains(&"implicit".to_owned()));

    let explicit = repo.command_output(&[
        "add",
        "explicit",
        "--at",
        "@",
        "--no-links",
        "--no-bookmark",
    ]);
    assert!(
        explicit.status.success(),
        "{}",
        String::from_utf8_lossy(&explicit.stderr)
    );
    let status = repo.command_output(&["status", "explicit", "--format=json", "--refresh=none"]);
    let value: serde_json::Value = serde_json::from_slice(&status.stdout).expect("status JSON");
    assert_eq!(value["workspaces"][0]["creation_base_commit_id"], merge);
}

#[test]
fn adopt_records_intent_without_changing_jj_state() {
    skip_without_jj!();
    let repo = TestRepo::new().expect("create test repo");
    let legacy_root = repo.default_root.with_extension("legacy");
    repo.run_jj([
        "workspace",
        "add",
        "--name",
        "legacy",
        legacy_root.to_str().unwrap(),
    ]);
    fs::write(legacy_root.join(".jj/jw-bookmark"), "broken\nmarker\n")
        .expect("write invalid legacy marker");
    let missing_before = repo.operation_id();
    let missing = repo.command_output(&[
        "adopt",
        "legacy",
        "--base",
        "legacy@-",
        "--bookmark",
        "missing",
    ]);
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("does not exist locally"));
    assert_eq!(repo.operation_id(), missing_before);

    repo.run_jj(["bookmark", "create", "declared", "-r", "legacy@"]);
    let operation = repo.operation_id();
    let revision = repo.revision_commit_id("legacy@");
    let bookmarks = repo.bookmarks();

    let output = repo.command_output(&[
        "adopt",
        "legacy",
        "--base",
        "legacy@-",
        "--bookmark",
        "declared",
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("stack analysis: deferred to milestone 1")
    );
    assert_eq!(repo.operation_id(), operation);
    assert_eq!(repo.revision_commit_id("legacy@"), revision);
    assert_eq!(repo.bookmarks(), bookmarks);

    let status = repo.command_output(&["status", "legacy", "--format=json", "--refresh=none"]);
    assert!(status.status.success());
    let value: serde_json::Value = serde_json::from_slice(&status.stdout).expect("status JSON");
    assert_eq!(value["workspaces"][0]["management"], "managed");
    assert_eq!(value["workspaces"][0]["associated_bookmark"], "declared");
}

#[test]
fn adopt_no_bookmark_ignores_stale_legacy_marker_without_changing_jj_state() {
    skip_without_jj!();
    let repo = TestRepo::new().expect("create test repo");
    let legacy_root = repo.default_root.with_extension("legacy");
    repo.run_jj([
        "workspace",
        "add",
        "--name",
        "legacy",
        legacy_root.to_str().unwrap(),
    ]);
    fs::write(legacy_root.join(".jj/jw-bookmark"), "wip/deleted\n")
        .expect("write stale legacy marker");
    let operation = repo.operation_id();
    let bookmarks = repo.bookmarks();

    let output = repo.command_output(&["adopt", "legacy", "--base", "legacy@-", "--no-bookmark"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(repo.operation_id(), operation);
    assert_eq!(repo.bookmarks(), bookmarks);

    let record = repo
        .metadata_record_paths()
        .into_iter()
        .next()
        .expect("managed metadata record");
    let metadata: serde_json::Value =
        serde_json::from_slice(&fs::read(record).expect("read metadata")).expect("metadata JSON");
    assert_eq!(
        metadata["metadata"]["associated_bookmark"],
        serde_json::Value::Null
    );
}

#[test]
fn adopt_rejects_bookmark_and_no_bookmark_together() {
    skip_without_jj!();
    let repo = TestRepo::new().expect("create test repo");

    repo.cmd()
        .args([
            "adopt",
            "default",
            "--base",
            "@-",
            "--bookmark",
            "wip/default",
            "--no-bookmark",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "cannot be used with '--no-bookmark'",
        ));
}

#[test]
fn repair_requires_explicit_bookmark_intent_and_literal_workspace_name() {
    skip_without_jj!();
    let repo = TestRepo::new().expect("create test repo");

    repo.cmd()
        .args(["repair", "default", "--base", "@-"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--bookmark"));
    repo.cmd()
        .args([
            "repair",
            "default",
            "--base",
            "@-",
            "--bookmark",
            "main",
            "--no-bookmark",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("cannot be used with"));
    repo.cmd()
        .args(["repair", "@", "--base", "@-", "--no-bookmark"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("literal workspace name"));

    repo.cmd()
        .args(["adopt", "default", "--base", "@-", "--no-bookmark"])
        .assert()
        .success();
    repo.cmd()
        .args(["repair", "default", "--base", "@-", "--no-bookmark"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Repaired workspace: default"));
}

#[test]
fn repair_replaces_only_base_and_bookmark_and_fixes_doctor_remedy() {
    skip_without_jj!();
    let repo = TestRepo::new().expect("create test repo");
    repo.cmd()
        .args(["add", "--no-links", "feature-a"])
        .assert()
        .success();
    let record_path = repo
        .metadata_record_paths()
        .into_iter()
        .next()
        .expect("managed metadata record");
    let mut record: serde_json::Value =
        serde_json::from_slice(&fs::read(&record_path).expect("read managed metadata record"))
            .expect("metadata JSON");
    let created_at = record["metadata"]["created_at_unix_ms"].clone();
    let creation_operation = record["metadata"]["creation_operation_id"].clone();
    record["metadata"]["creation_base_commit_id"] = "missing-base".into();
    record["metadata"]["associated_bookmark"] = "missing-bookmark".into();
    record["metadata"]["intended_remote"] = "origin".into();
    let mut bytes = serde_json::to_vec_pretty(&record).expect("serialize metadata");
    bytes.push(b'\n');
    fs::write(&record_path, bytes).expect("write invalid managed metadata");

    repo.cmd()
        .arg("doctor")
        .assert()
        .failure()
        .stdout(predicate::str::contains("jw repair feature-a"))
        .stdout(predicate::str::contains(
            "jw repair feature-a --base <exact-revset> --no-bookmark",
        ));

    let operation = repo.operation_id();
    let bookmarks = repo.bookmarks();
    let feature_root = repo.default_root.with_extension("feature-a");
    let commit = repo.revision_commit_id("feature-a@");
    let expected_base = repo.revision_commit_id("@-");
    repo.cmd()
        .args(["repair", "feature-a", "--base", "@-", "--no-bookmark"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Repaired workspace: feature-a"))
        .stdout(predicate::str::contains(
            "previous creation base: missing-base",
        ))
        .stdout(predicate::str::contains(format!(
            "creation base: {expected_base}"
        )))
        .stdout(predicate::str::contains(
            "previous bookmark: missing-bookmark",
        ))
        .stdout(predicate::str::contains("bookmark: (none)"));

    assert_eq!(repo.operation_id(), operation);
    assert_eq!(repo.bookmarks(), bookmarks);
    assert_eq!(repo.revision_commit_id("feature-a@"), commit);
    assert!(feature_root.is_dir());
    let repaired: serde_json::Value =
        serde_json::from_slice(&fs::read(&record_path).expect("read repaired metadata record"))
            .expect("repaired metadata JSON");
    assert_eq!(repaired["metadata"]["created_at_unix_ms"], created_at);
    assert_eq!(
        repaired["metadata"]["creation_operation_id"],
        creation_operation
    );
    assert_eq!(
        repaired["metadata"]["creation_base_commit_id"],
        expected_base
    );
    assert_eq!(
        repaired["metadata"]["associated_bookmark"],
        serde_json::Value::Null
    );
    assert_eq!(repaired["metadata"]["intended_remote"], "origin");
    repo.cmd().arg("doctor").assert().success();
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
        let config_home = tempdir.path().join("config");
        fs::create_dir_all(&config_home)?;

        run_in(
            tempdir.path(),
            &config_home,
            ["jj", "git", "init", default_root.to_str().unwrap()],
        )?;
        fs::write(default_root.join("README.md"), "hello\n")?;
        run_in(
            &default_root,
            &config_home,
            ["jj", "file", "track", "root:README.md"],
        )?;
        run_in(
            &default_root,
            &config_home,
            ["jj", "commit", "-m", "initial"],
        )?;

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

    fn command_output(&self, args: &[&str]) -> std::process::Output {
        self.cmd().args(args).output().expect("execute jw command")
    }

    fn run_jj<const N: usize>(&self, args: [&str; N]) {
        self.run_in(
            &self.default_root,
            std::iter::once("jj").chain(args).collect::<Vec<_>>(),
        )
        .expect("jj command succeeds");
    }

    fn run_in<I, S>(&self, cwd: &Path, args: I) -> anyhow::Result<()>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        run_in(cwd, &self.config_home, args)
    }

    fn jj_command_at(&self, cwd: &Path) -> Command {
        let mut command = Command::new("jj");
        command
            .current_dir(cwd)
            .env("XDG_CONFIG_HOME", &self.config_home);
        command
    }

    fn jj_stdout(&self, cwd: &Path, args: &[&str]) -> String {
        let output = self
            .jj_command_at(cwd)
            .args(args)
            .output()
            .expect("execute jj command");
        assert!(
            output.status.success(),
            "jj {args:?} failed\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .expect("UTF-8 jj output")
            .trim()
            .to_owned()
    }

    fn operation_id(&self) -> String {
        self.jj_stdout(
            &self.default_root,
            &[
                "--at-operation",
                "@",
                "--ignore-working-copy",
                "operation",
                "log",
                "--limit=1",
                "--no-graph",
                "-T",
                "id",
            ],
        )
    }

    fn revision_commit_id(&self, revset: &str) -> String {
        self.jj_stdout(
            &self.default_root,
            &[
                "--ignore-working-copy",
                "log",
                "-r",
                revset,
                "--no-graph",
                "-T",
                "commit_id",
            ],
        )
    }

    fn metadata_root(&self) -> PathBuf {
        let config_path = PathBuf::from(self.jj_stdout(
            &self.default_root,
            &["--ignore-working-copy", "config", "path", "--repo"],
        ));
        config_path
            .parent()
            .expect("repository config path has parent")
            .join("jj-waltz")
    }

    fn metadata_record_paths(&self) -> Vec<PathBuf> {
        let directory = self.metadata_root().join("workspaces");
        let Ok(entries) = fs::read_dir(directory) else {
            return Vec::new();
        };
        entries
            .map(|entry| entry.expect("read metadata entry").path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "json")
            })
            .collect()
    }

    fn write_config(&self, contents: &str) {
        let config_dir = self.config_home.join("jj-waltz");
        fs::create_dir_all(&config_dir).expect("create config dir");
        fs::write(config_dir.join("config.toml"), contents).expect("write config");
    }

    fn bookmarks(&self) -> Vec<String> {
        let output = self
            .jj_command_at(&self.default_root)
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
        let output = self
            .jj_command_at(&self.default_root)
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
        let current_root = self
            .jj_command_at(&self.default_root)
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

        let output = self
            .jj_command_at(&self.default_root)
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
                let output = self
                    .jj_command_at(&self.default_root)
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

    fn current_change_id(&self, cwd: &Path) -> String {
        let output = self
            .jj_command_at(cwd)
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

fn run_in<I, S>(cwd: &Path, config_home: &Path, args: I) -> anyhow::Result<()>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let values = args
        .into_iter()
        .map(|arg| arg.as_ref().to_owned())
        .collect::<Vec<_>>();
    let (program, rest) = values.split_first().expect("program");
    let output = Command::new(program)
        .current_dir(cwd)
        .env("XDG_CONFIG_HOME", config_home)
        .args(rest)
        .output()?;
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
