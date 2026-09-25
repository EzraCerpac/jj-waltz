use assert_cmd::prelude::*;
use predicates::prelude::*;
use std::process::Command;

#[test]
fn nonterminal_bare_invocation_shows_help_without_discovering_repository() {
    let temp = tempfile::tempdir().unwrap();
    Command::cargo_bin("jw")
        .unwrap()
        .current_dir(temp.path())
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Open the interactive workspace manager",
        ));
}

#[test]
fn explicit_ui_requires_a_terminal() {
    Command::cargo_bin("jw")
        .unwrap()
        .arg("ui")
        .assert()
        .failure()
        .stdout("")
        .stderr(predicate::str::contains("requires a terminal"));
}

#[test]
fn shell_path_mode_never_emits_help_as_a_destination() {
    Command::cargo_bin("jw")
        .unwrap()
        .arg("--ui-path")
        .assert()
        .success()
        .stdout("")
        .stderr(predicate::str::contains("Usage:"));
}

#[test]
fn ui_help_does_not_require_a_terminal() {
    Command::cargo_bin("jw")
        .unwrap()
        .args(["ui", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("interactive workspace manager"));
}
