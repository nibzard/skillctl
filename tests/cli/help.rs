use super::*;

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
        .stdout(predicate::str::contains(
            "Create the default .agents workspace layout for skillctl.",
        ));
}

#[test]
fn install_help_is_available() {
    let mut cmd = Command::cargo_bin("skillctl").expect("binary exists");

    cmd.args(["install", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Usage: skillctl install [OPTIONS] <SOURCE>",
        ));
}

#[test]
fn root_help_describes_the_operating_model_and_current_mcp_surface() {
    let mut cmd = Command::cargo_bin("skillctl").expect("binary exists");

    cmd.arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Canonical workspace skills live in .agents/skills.",
        ))
        .stdout(predicate::str::contains(
            "Local history and telemetry consent live in ~/.skillctl/state.db.",
        ))
        .stdout(predicate::str::contains(
            "'tui' opens a read-only dashboard over the same state and inspection model as the CLI.",
        ));
}

#[test]
fn install_help_includes_examples_and_non_interactive_guidance() {
    let mut cmd = Command::cargo_bin("skillctl").expect("binary exists");

    cmd.args(["install", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Non-interactive installs never guess",
        ))
        .stdout(predicate::str::contains(
            "skillctl install ../shared-skills --interactive",
        ))
        .stdout(predicate::str::contains("~/.skillctl/store/imports"));
}

#[test]
fn update_doctor_and_explain_help_include_recovery_guidance() {
    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .args(["update", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Recommended follow-up actions:"))
        .stdout(predicate::str::contains("create-overlay:"))
        .stdout(predicate::str::contains("skillctl history <skill>"));

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .args(["doctor", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Start here when:"))
        .stdout(predicate::str::contains("a generated copy looks stale"))
        .stdout(predicate::str::contains("trust warnings"));

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .args(["explain", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("same-name conflicts"))
        .stdout(predicate::str::contains(
            "skillctl explain ai-sdk --target codex",
        ))
        .stdout(predicate::str::contains("skillctl path <skill>"));
}

#[test]
fn tui_and_mcp_help_point_to_current_cli_equivalents() {
    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .args(["tui", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "The terminal inspection UI is a read-only dashboard",
        ))
        .stdout(predicate::str::contains(
            "Opening it does not bootstrap bundled skills",
        ))
        .stdout(predicate::str::contains("installed versions"))
        .stdout(predicate::str::contains(
            "refresh update state: skillctl update [skill]",
        ));

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .args(["mcp", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "The MCP server exposes the same lifecycle operations and JSON envelopes as the CLI.",
        ))
        .stdout(predicate::str::contains(
            "skills_override_create -> skillctl override <skill> --json",
        ));
}

#[test]
fn tui_renders_a_read_only_dashboard_without_writing_history() {
    let workspace = TestWorkspace::new();
    workspace.write_skill_source("shared-skills", "release-notes");

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .env("SOURCE_DATE_EPOCH", "1770000000")
        .args([
            "--no-input",
            "--name",
            "release-notes",
            "install",
            "shared-skills",
        ])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .env("SOURCE_DATE_EPOCH", "1770000060")
        .args(["override", "release-notes"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .env("SOURCE_DATE_EPOCH", "1770000120")
        .args(["update", "release-notes"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    let connection = Connection::open(workspace.home_path().join(".skillctl/state.db"))
        .expect("state database opens");
    let history_before: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM history_events WHERE skill_id = ?1",
            params!["release-notes"],
            |row| row.get(0),
        )
        .expect("history query succeeds");

    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["tui"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    let stdout = String::from_utf8(assert.get_output().stdout.clone()).expect("stdout is utf-8");
    assert!(
        stdout.contains("skillctl terminal UI"),
        "stdout was {stdout}"
    );
    assert!(stdout.contains("release-notes"), "stdout was {stdout}");
    assert!(
        stdout.contains("update: local-source"),
        "stdout was {stdout}"
    );
    assert!(
        stdout.contains(".agents/overlays/release-notes"),
        "stdout was {stdout}"
    );
    assert!(stdout.contains("Recent history"), "stdout was {stdout}");
    assert!(
        stdout.contains("skillctl --scope workspace update release-notes"),
        "stdout was {stdout}"
    );
    assert!(
        stdout.contains("skillctl --scope workspace history release-notes"),
        "stdout was {stdout}"
    );

    let history_after: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM history_events WHERE skill_id = ?1",
            params!["release-notes"],
            |row| row.get(0),
        )
        .expect("history query succeeds");
    assert_eq!(history_before, history_after);
}

#[test]
fn tui_json_output_aggregates_skill_state_for_the_selected_skill() {
    let workspace = TestWorkspace::new();
    workspace.write_skill_source("shared-skills", "release-notes");

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .env("SOURCE_DATE_EPOCH", "1770000000")
        .args([
            "--no-input",
            "--name",
            "release-notes",
            "install",
            "shared-skills",
        ])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .env("SOURCE_DATE_EPOCH", "1770000060")
        .args(["override", "release-notes"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .env("SOURCE_DATE_EPOCH", "1770000120")
        .args(["update", "release-notes"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--json", "--name", "release-notes", "tui"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    let body: Value = serde_json::from_slice(&assert.get_output().stdout).expect("stdout is json");
    assert_eq!(body["command"], "tui");
    assert_eq!(body["ok"], true);
    assert_eq!(body["data"]["skills"][0]["skill"], "release-notes");
    assert_eq!(body["data"]["skills"][0]["scope"], "workspace");
    assert_eq!(
        body["data"]["skills"][0]["update"]["outcome"],
        "local-source"
    );
    assert_eq!(body["data"]["skills"][0]["overlay"]["present"], true);
    assert_eq!(
        body["data"]["skills"][0]["actions"]["history"],
        "skillctl --scope workspace history release-notes"
    );
    assert_eq!(
        body["data"]["skills"][0]["actions"]["update"],
        "skillctl --scope workspace update release-notes"
    );
    assert_eq!(body["data"]["history"]["skill"], "release-notes");
    assert!(
        body["data"]["history"]["entries"]
            .as_array()
            .expect("entries array exists")
            .iter()
            .any(|entry| entry["kind"] == "overlay-created"),
        "unexpected history payload: {body:#?}",
    );
}

#[test]
fn tui_reports_manifest_configured_overlay_roots() {
    let workspace = TestWorkspace::new();
    workspace.write_manifest(concat!(
        "version: 1\n",
        "\n",
        "layout:\n",
        "  overlays_dir: .agents/custom-overlays\n",
        "targets:\n",
        "  - claude-code\n",
    ));
    workspace.write_skill_source("shared-skills", "release-notes");

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .env("SOURCE_DATE_EPOCH", "1770000000")
        .args([
            "--no-input",
            "--name",
            "release-notes",
            "install",
            "shared-skills",
        ])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .env("SOURCE_DATE_EPOCH", "1770000060")
        .args(["override", "release-notes"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .env("SOURCE_DATE_EPOCH", "1770000120")
        .args(["disable", "release-notes"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--json", "--name", "release-notes", "tui"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    let body: Value = serde_json::from_slice(&assert.get_output().stdout).expect("stdout is json");
    assert_eq!(
        body["data"]["skills"][0]["overlay"]["path"],
        ".agents/custom-overlays/release-notes"
    );
    assert_eq!(body["data"]["skills"][0]["overlay"]["present"], true);

    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["tui"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    let stdout = String::from_utf8(assert.get_output().stdout.clone()).expect("stdout is utf-8");
    assert!(
        stdout.contains(".agents/custom-overlays/release-notes"),
        "stdout was {stdout}"
    );
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
fn init_dispatches_to_runtime_and_bootstraps_the_workspace() {
    let workspace = TestWorkspace::new();
    let mut cmd = Command::cargo_bin("skillctl").expect("binary exists");

    cmd.current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .arg("init")
        .assert()
        .success()
        .stdout(predicate::str::contains("skillctl workspace"))
        .stderr(predicate::str::is_empty());
}

#[test]
fn json_output_uses_stable_response_contract_for_command_errors() {
    let home = tempfile::tempdir().expect("tempdir exists");
    let mut cmd = Command::cargo_bin("skillctl").expect("binary exists");

    let assert = cmd
        .env("HOME", home.path())
        .args(["--json", "install", "./missing-source"])
        .assert()
        .failure()
        .code(3)
        .stderr(predicate::str::is_empty());

    let output = assert.get_output();
    let body: Value = serde_json::from_slice(&output.stdout).expect("stdout is valid json");

    assert_eq!(
        body,
        json!({
            "ok": false,
            "command": "install",
            "warnings": [],
            "errors": ["install source './missing-source' is invalid: source must be a Git URL, existing local directory, or existing local archive"],
            "data": {}
        })
    );
}

#[test]
fn json_output_normalizes_parse_failures_into_the_response_envelope() {
    let mut cmd = Command::cargo_bin("skillctl").expect("binary exists");

    let assert = cmd
        .args(["--json", "install"])
        .assert()
        .failure()
        .code(3)
        .stderr(predicate::str::is_empty());

    let output = assert.get_output();
    let body: Value = serde_json::from_slice(&output.stdout).expect("stdout is valid json");
    let error = body["errors"][0].as_str().expect("error message exists");

    assert_eq!(body["command"], "install");
    assert_eq!(body["ok"], false);
    assert_eq!(body["warnings"], json!([]));
    assert_eq!(body["data"], json!({}));
    assert!(error.contains("required arguments were not provided"));
    assert!(error.contains("Usage: skillctl install <SOURCE>"));
}

#[test]
fn global_execution_flags_are_accepted_before_the_subcommand() {
    let workspace = TestWorkspace::new();
    workspace.write_skill_source("shared-skills", "release-notes");
    let mut cmd = Command::cargo_bin("skillctl").expect("binary exists");

    let assert = cmd
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
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
            "shared-skills",
        ])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    let output = assert.get_output();
    let body: Value = serde_json::from_slice(&output.stdout).expect("stdout is valid json");

    assert_eq!(body["command"], "install");
    assert_eq!(body["ok"], true);
    assert_eq!(body["data"]["source"]["type"], "local-path");
    assert_eq!(body["data"]["selected"][0]["name"], "release-notes");
    assert_eq!(body["data"]["installed"][0]["scope"], "workspace");
    assert!(
        workspace
            .path()
            .join(".agents/skills/release-notes/SKILL.md")
            .is_file()
    );
}

#[test]
fn global_execution_flags_are_accepted_after_the_subcommand() {
    let workspace = TestWorkspace::new();
    workspace.write_skill_source("shared-skills", "release-notes");
    let mut cmd = Command::cargo_bin("skillctl").expect("binary exists");

    let assert = cmd
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args([
            "install",
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
            "shared-skills",
        ])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    let output = assert.get_output();
    let body: Value = serde_json::from_slice(&output.stdout).expect("stdout is valid json");

    assert_eq!(body["command"], "install");
    assert_eq!(body["ok"], true);
    assert_eq!(body["data"]["selected"][0]["name"], "release-notes");
    assert_eq!(body["data"]["installed"][0]["scope"], "workspace");
}

#[test]
fn quiet_mode_suppresses_success_summaries() {
    let workspace = TestWorkspace::new();

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--quiet", "init"])
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::is_empty());

    assert!(workspace.path().join(".agents/skills").is_dir());
    assert!(workspace.path().join(".agents/overlays").is_dir());
}

#[test]
fn verbose_mode_renders_structured_data_in_human_output() {
    let workspace = TestWorkspace::new();
    fs::create_dir_all(workspace.path().join(".git/info")).expect("git info directory exists");

    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--verbose", "init"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    let stdout = String::from_utf8(assert.get_output().stdout.clone()).expect("stdout is utf-8");
    assert!(stdout.contains("Initialized skillctl workspace"));
    assert!(stdout.contains("\"created\""));
    assert!(stdout.contains("\"git_exclude\""));
}

#[test]
fn install_alias_help_is_available() {
    let mut cmd = Command::cargo_bin("skillctl").expect("binary exists");

    cmd.args(["i", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Usage: skillctl install [OPTIONS] <SOURCE>",
        ));
}
