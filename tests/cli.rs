use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::{Value, json};

#[test]
fn root_help_lists_core_commands() {
    let mut cmd = Command::cargo_bin("skillctl").expect("binary exists");

    cmd.arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("init"))
        .stdout(predicate::str::contains("install"))
        .stdout(predicate::str::contains("update"))
        .stdout(predicate::str::contains("doctor"));
}

#[test]
fn init_help_is_available() {
    let mut cmd = Command::cargo_bin("skillctl").expect("binary exists");

    cmd.args(["init", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Initialize"));
}

#[test]
fn install_help_is_available() {
    let mut cmd = Command::cargo_bin("skillctl").expect("binary exists");

    cmd.args(["install", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Install"));
}

#[test]
fn bare_invocation_prints_help() {
    let mut cmd = Command::cargo_bin("skillctl").expect("binary exists");

    cmd.assert()
        .success()
        .stdout(predicate::str::contains("Usage:"))
        .stdout(predicate::str::contains("skillctl"))
        .stderr(predicate::str::is_empty());
}

#[test]
fn init_dispatches_to_runtime_instead_of_falling_back_to_help() {
    let mut cmd = Command::cargo_bin("skillctl").expect("binary exists");

    cmd.arg("init")
        .assert()
        .failure()
        .code(1)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "command 'init' is not implemented yet",
        ));
}

#[test]
fn json_output_uses_stable_response_contract_for_command_errors() {
    let mut cmd = Command::cargo_bin("skillctl").expect("binary exists");

    let assert = cmd
        .args(["--json", "install", "https://example.com/skills.git"])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::is_empty());

    let output = assert.get_output();
    let body: Value = serde_json::from_slice(&output.stdout).expect("stdout is valid json");

    assert_eq!(
        body,
        json!({
            "ok": false,
            "command": "install",
            "warnings": [],
            "errors": ["command 'install' is not implemented yet"],
            "data": {}
        })
    );
}

#[test]
fn global_execution_flags_are_accepted_before_the_subcommand() {
    let mut cmd = Command::cargo_bin("skillctl").expect("binary exists");

    let assert = cmd
        .args([
            "--json",
            "--no-input",
            "--name",
            "release-notes",
            "--scope",
            "workspace",
            "--target",
            "codex",
            "--cwd",
            ".",
            "install",
            "../shared-skills",
        ])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::is_empty());

    let output = assert.get_output();
    let body: Value = serde_json::from_slice(&output.stdout).expect("stdout is valid json");

    assert_eq!(body["command"], "install");
    assert_eq!(body["ok"], false);
}

#[test]
fn install_alias_help_is_available() {
    let mut cmd = Command::cargo_bin("skillctl").expect("binary exists");

    cmd.args(["i", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Install"));
}
