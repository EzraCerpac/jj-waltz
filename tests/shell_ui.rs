#![cfg(unix)]

use assert_cmd::prelude::*;
use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::TempDir;

const MOCK_JW: &str = r#"#!/bin/sh
printf '%s\n' "$*" >> "$JW_UI_LOG"
if [ "$1" = shell ]; then
    exit 0
fi
if [ "$1" = --ui-path ]; then
    if [ -n "${JW_UI_STATUS:-}" ]; then
        exit "$JW_UI_STATUS"
    fi
    if [ -n "${JW_UI_TARGET:-}" ]; then
        printf '%s\n' "$JW_UI_TARGET"
    fi
    exit 0
fi
exit 0
"#;

fn shell_init(shell: &str) -> String {
    let adapter = match shell {
        "pwsh" => "powershell",
        _ => shell,
    };
    let output = Command::cargo_bin("jw")
        .expect("jw binary")
        .args(["shell", "init", adapter])
        .output()
        .expect("generate shell init script");
    assert!(
        output.status.success(),
        "jw shell init {adapter} for {shell} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("shell init is UTF-8")
}

fn mock_path(temp: &TempDir) -> (PathBuf, PathBuf) {
    let bin = temp.path().join("bin");
    fs::create_dir_all(&bin).expect("create mock bin directory");
    let mock = bin.join("jw");
    fs::write(&mock, MOCK_JW).expect("write mock jw");
    fs::set_permissions(&mock, fs::Permissions::from_mode(0o755)).expect("make mock executable");
    (bin, temp.path().join("jw-ui.log"))
}

fn path_env(bin: &Path) -> String {
    let mut paths = vec![bin.to_path_buf()];
    paths.extend(env::split_paths(&env::var_os("PATH").unwrap_or_default()));
    env::join_paths(paths)
        .expect("join PATH")
        .to_string_lossy()
        .into_owned()
}

fn run_shell(
    shell: &str,
    init: &str,
    command: &str,
    temp: &TempDir,
    target: Option<&Path>,
    status: Option<i32>,
) -> Output {
    let (bin, log) = mock_path(temp);
    let start = temp.path().join("start");
    fs::create_dir_all(&start).expect("create start directory");
    let mut child = Command::new(shell);
    child
        .current_dir(&start)
        .env("PATH", path_env(&bin))
        .env("JW_UI_LOG", &log)
        .env_remove("JW_UI_TARGET")
        .env_remove("JW_UI_STATUS");
    if let Some(target) = target {
        child.env("JW_UI_TARGET", target);
    }
    if let Some(status) = status {
        child.env("JW_UI_STATUS", status.to_string());
    }
    let body = format!("{init}\n{command}");
    if matches!(shell, "powershell" | "pwsh") {
        child.args(["-NoLogo", "-NoProfile", "-Command"]);
    } else if shell == "zsh" {
        child.args(["-f", "-c"]);
    } else if shell == "bash" {
        child.args(["--noprofile", "--norc", "-c"]);
    } else if shell == "fish" {
        child.args(["--no-config", "-c"]);
    } else if shell == "elvish" {
        child.args(["-norc", "-c"]);
    } else {
        child.arg("-c");
    }
    child.arg(body);
    child.output().expect("run shell wrapper")
}

fn print_working_directory(shell: &str) -> &'static str {
    match shell {
        "elvish" => "pwd",
        "powershell" | "pwsh" => "(Get-Location).Path",
        _ => "printf '%s\\n' \"$PWD\"",
    }
}

fn assert_target(shell: &str, command: &str) {
    let temp = tempfile::tempdir().expect("temporary directory");
    let target = temp.path().join("destination with spaces");
    fs::create_dir(&target).expect("create target directory");
    let init = shell_init(shell);
    let output = run_shell(
        shell,
        &init,
        &format!("{command}\n{}", print_working_directory(shell)),
        &temp,
        Some(&target),
        None,
    );
    assert!(
        output.status.success(),
        "{shell} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let actual = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    assert_eq!(
        fs::canonicalize(actual).expect("canonicalize actual"),
        fs::canonicalize(&target).expect("canonicalize target"),
        "{shell} did not change directory"
    );
    let log = fs::read_to_string(temp.path().join("jw-ui.log")).expect("read mock log");
    let expected = if command == "jw" {
        "--ui-path"
    } else {
        "--ui-path ui"
    };
    assert!(
        log.lines().any(|line| line == expected),
        "{shell} invoked the UI with {log:?}"
    );
}

#[test]
fn every_available_shell_adapter_switches_for_bare_and_ui_invocations() {
    for shell in ["bash", "zsh", "fish", "elvish", "powershell", "pwsh"] {
        if Command::new(shell).arg("--version").output().is_err() {
            continue;
        }
        assert_target(shell, "jw");
        assert_target(shell, "jw ui");
    }
}

#[test]
fn available_adapters_keep_cancellation_and_failure_in_the_caller() {
    for shell in ["bash", "zsh", "fish", "elvish", "powershell", "pwsh"] {
        if Command::new(shell).arg("--version").output().is_err() {
            continue;
        }
        let temp = tempfile::tempdir().expect("temporary directory");
        let start = temp.path().join("start");
        fs::create_dir(&start).expect("create start directory");
        let init = shell_init(shell);
        let output = run_shell(
            shell,
            &init,
            &format!("jw\n{}", print_working_directory(shell)),
            &temp,
            None,
            None,
        );
        assert!(output.status.success(), "{shell} cancellation failed");
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            fs::canonicalize(&start)
                .expect("canonicalize start")
                .to_string_lossy(),
            "{shell} changed directory after cancellation"
        );

        let failure_command = if matches!(shell, "powershell" | "pwsh") {
            // PowerShell's process exit code comes from `exit`; expose the
            // native `jw` command's code after exercising the adapter.
            "jw; exit $LASTEXITCODE"
        } else {
            "jw"
        };
        let output = run_shell(shell, &init, failure_command, &temp, None, Some(17));
        assert_eq!(
            output.status.code(),
            Some(17),
            "{shell} hid UI failure; stdout={:?}; stderr={:?}",
            output.stdout,
            output.stderr
        );
    }
}

#[test]
fn ui_wrapper_passes_help_through_to_the_binary() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let (bin, log) = mock_path(&temp);
    let init = shell_init("bash");
    let output = Command::new("bash")
        .current_dir(temp.path())
        .env("PATH", path_env(&bin))
        .env("JW_UI_LOG", &log)
        .arg("-c")
        .arg(format!("{init}\njw ui --help"))
        .output()
        .expect("run bash help wrapper");
    assert!(output.status.success());
    let log = fs::read_to_string(log).expect("read mock invocation log");
    assert!(log.lines().any(|line| line == "ui --help"));
    assert!(!log.lines().any(|line| line == "--ui-path ui"));
}
