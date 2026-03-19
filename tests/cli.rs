use assert_cmd::Command;
use predicates::prelude::*;
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use serde_yaml::Value as YamlValue;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::Command as ProcessCommand,
    time::{SystemTime, UNIX_EPOCH},
};

const MINIMAL_LOCKFILE: &str = concat!(
    "version: 1\n",
    "\n",
    "state:\n",
    "  manifest_version: 1\n",
    "  local_state_version: 1\n",
);

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
        .arg("init")
        .assert()
        .success()
        .stdout(predicate::str::contains("skillctl workspace"))
        .stderr(predicate::str::is_empty());
}

#[test]
fn json_output_uses_stable_response_contract_for_command_errors() {
    let mut cmd = Command::cargo_bin("skillctl").expect("binary exists");

    let assert = cmd
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

#[test]
fn install_requires_explicit_selection_when_non_interactive() {
    let workspace = TestWorkspace::new();
    workspace.write_skill_source("shared-skills", "release-notes");

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--no-input", "install", "shared-skills"])
        .assert()
        .failure()
        .code(5)
        .stderr(predicate::str::contains(
            "interactive input is required for command 'install'",
        ));
}

#[test]
fn install_rejects_ambiguous_exact_name_selection() {
    let workspace = TestWorkspace::new();
    workspace.write_skill_source_at(
        "shared-skills",
        ".agents/skills/release-notes",
        "release-notes",
        "Canonical release notes helper.",
    );
    workspace.write_skill_source_at(
        "shared-skills",
        "skills/release-notes",
        "release-notes",
        "Packaged release notes helper.",
    );

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args([
            "--no-input",
            "--name",
            "release-notes",
            "install",
            "shared-skills",
        ])
        .assert()
        .failure()
        .code(3)
        .stderr(predicate::str::contains(
            "exact skill name 'release-notes' is ambiguous",
        ));
}

#[test]
fn install_interactively_selects_a_candidate_and_scope() {
    let workspace = TestWorkspace::new();
    workspace.write_skill_source("shared-skills", "release-notes");
    workspace.write_skill_source("shared-skills", "bug-triage");

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--interactive", "install", "shared-skills"])
        .write_stdin("1\n1\n")
        .assert()
        .success()
        .stderr(predicate::str::is_empty())
        .stdout(predicate::str::contains("Select skills"))
        .stdout(predicate::str::contains("Select scope"))
        .stdout(predicate::str::contains("Installed 1 skill"));

    let manifest = fs::read_to_string(workspace.path().join(".agents/skillctl.yaml"))
        .expect("manifest exists");
    assert!(
        manifest.contains("id: bug-triage"),
        "manifest was {manifest}"
    );
    assert!(
        workspace.path().join(".agents/skills/bug-triage").is_dir(),
        "selected skill should be materialized into the workspace root",
    );
    assert!(
        !workspace
            .path()
            .join(".agents/skills/release-notes")
            .exists(),
        "unselected skill should not be installed",
    );
}

#[test]
fn install_updates_manifest_lockfile_store_state_and_projection_records() {
    let workspace = TestWorkspace::new();
    workspace.write_manifest(concat!(
        "version: 1\n",
        "\n",
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
        .stderr(predicate::str::is_empty())
        .stdout(predicate::str::contains("Installed 1 skill"))
        .stdout(predicate::str::contains(
            "Materialized 1 generated projection",
        ));

    let manifest = fs::read_to_string(workspace.path().join(".agents/skillctl.yaml"))
        .expect("manifest exists");
    assert!(
        manifest.contains("id: release-notes"),
        "manifest was {manifest}"
    );
    assert!(
        manifest.contains("type: local-path"),
        "manifest should record the installed source kind: {manifest}",
    );

    let lockfile = fs::read_to_string(workspace.path().join(".agents/skillctl.lock"))
        .expect("lockfile exists");
    assert!(
        lockfile.contains("imports:\n  release-notes:"),
        "lockfile was {lockfile}",
    );
    assert!(
        lockfile.contains("type: local-path"),
        "lockfile should record the installed source kind: {lockfile}",
    );

    assert!(
        workspace
            .home_path()
            .join(".skillctl/store/imports/release-notes/.agents/skills/release-notes/SKILL.md")
            .is_file(),
        "stored import checkout should exist",
    );
    assert!(
        workspace
            .path()
            .join(".claude/skills/release-notes/.skillctl-projection.json")
            .is_file(),
        "generated projection metadata should exist",
    );

    let connection = Connection::open(workspace.home_path().join(".skillctl/state.db"))
        .expect("state database opens");
    let install_row: (String, String, String) = connection
        .query_row(
            "SELECT scope, skill_id, source_kind FROM install_records \
             WHERE scope = ?1 AND skill_id = ?2",
            params!["workspace", "release-notes"],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("install record exists");
    assert_eq!(
        install_row,
        (
            "workspace".to_string(),
            "release-notes".to_string(),
            "local-path".to_string()
        )
    );

    let projection_target: String = connection
        .query_row(
            "SELECT target FROM projection_records WHERE skill_id = ?1",
            params!["release-notes"],
            |row| row.get(0),
        )
        .expect("projection record exists");
    assert_eq!(projection_target, "claude-code");

    let history_kinds: Vec<String> = connection
        .prepare(
            "SELECT kind FROM history_events WHERE skill_id = ?1 ORDER BY occurred_at ASC, id ASC",
        )
        .expect("statement prepares")
        .query_map(params!["release-notes"], |row| row.get(0))
        .expect("history query succeeds")
        .collect::<Result<_, _>>()
        .expect("history rows decode");
    assert_eq!(
        history_kinds,
        vec!["install".to_string(), "projection".to_string()]
    );
}

#[test]
fn install_rolls_back_when_the_transaction_fails_after_state_write() {
    let workspace = TestWorkspace::new();
    workspace.write_manifest(concat!(
        "version: 1\n",
        "\n",
        "targets:\n",
        "  - claude-code\n",
    ));
    workspace.write_lockfile(MINIMAL_LOCKFILE);
    workspace.write_skill_source("shared-skills", "release-notes");
    initialize_runtime_state(&workspace);

    let snapshot = workspace.snapshot_paths(&[
        ".agents/skillctl.yaml",
        ".agents/skillctl.lock",
        "home/.skillctl/state.db",
        "home/.skillctl/store/imports/release-notes",
        ".claude/skills",
    ]);

    assert_transaction_rolled_back(
        &workspace,
        "install:after-state",
        "1770000000",
        &[
            "--no-input",
            "--name",
            "release-notes",
            "install",
            "shared-skills",
        ],
        &snapshot,
    );
}

#[test]
fn install_shows_a_first_run_telemetry_notice_and_persists_default_consent() {
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
        .stderr(predicate::str::is_empty())
        .stdout(predicate::str::contains(
            "Telemetry is enabled for public-source install and update events.",
        ))
        .stdout(predicate::str::contains("skillctl telemetry disable"));

    let connection = Connection::open(workspace.home_path().join(".skillctl/state.db"))
        .expect("state database opens");
    let telemetry_row: (String, Option<String>, String) = connection
        .query_row(
            "SELECT consent, notice_seen_at, updated_at FROM telemetry_settings",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("telemetry settings exist");
    assert_eq!(telemetry_row.0, "enabled");
    assert_eq!(telemetry_row.1.as_deref(), Some("2026-02-02T02:40:00Z"));
    assert_eq!(telemetry_row.2, "2026-02-02T02:40:00Z");

    let consent_history_events: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM history_events WHERE kind = ?1",
            params!["telemetry-consent-changed"],
            |row| row.get(0),
        )
        .expect("history query succeeds");
    assert_eq!(consent_history_events, 0);
}

#[test]
fn install_suppresses_remote_telemetry_for_private_https_git_sources() {
    let workspace = TestWorkspace::new();
    workspace.write_skill_source("git-source", "release-notes");
    workspace.init_git_repo("git-source");

    let private_https_url = "https://github.com/example/private-skill.git";
    let git_config_path = workspace.path().join("gitconfig");
    fs::write(
        &git_config_path,
        format!(
            "[url \"{}\"]\n\tinsteadOf = {}\n",
            workspace.git_repo_url("git-source"),
            private_https_url
        ),
    )
    .expect("git config written");

    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .env("GIT_CONFIG_GLOBAL", &git_config_path)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .args([
            "--json",
            "--no-input",
            "--name",
            "release-notes",
            "install",
            private_https_url,
        ])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    let body: Value = serde_json::from_slice(&assert.get_output().stdout).expect("stdout is json");

    assert_eq!(body["command"], "install");
    assert_eq!(body["ok"], true);
    assert_eq!(body["data"]["source"]["type"], "git");
    assert_eq!(body["data"]["source"]["url"], private_https_url);
    assert_eq!(body["data"]["telemetry"]["events"][0]["kind"], "install");
    assert_eq!(body["data"]["telemetry"]["events"][0]["emitted"], false);
    assert_eq!(
        body["data"]["telemetry"]["events"][0]["suppression_reason"],
        "private-source"
    );
    assert_eq!(
        body["data"]["telemetry"]["events"][0]["source_visibility"],
        "suppressed-private"
    );
    assert!(body["data"]["telemetry"]["events"][0]["public_source"].is_null());
}

#[test]
fn install_emits_remote_telemetry_for_verified_public_https_git_sources() {
    let workspace = TestWorkspace::new();
    workspace.write_skill_source("git-source", "release-notes");
    workspace.init_git_repo("git-source");

    let public_https_url = "https://github.com/vercel/ai.git";
    let git_config_path = workspace.path().join("gitconfig");
    fs::write(
        &git_config_path,
        format!(
            "[url \"{}\"]\n\tinsteadOf = {}\n",
            workspace.git_repo_url("git-source"),
            public_https_url
        ),
    )
    .expect("git config written");

    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .env("GIT_CONFIG_GLOBAL", &git_config_path)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .args([
            "--json",
            "--no-input",
            "--name",
            "release-notes",
            "install",
            public_https_url,
        ])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    let body: Value = serde_json::from_slice(&assert.get_output().stdout).expect("stdout is json");

    assert_eq!(body["command"], "install");
    assert_eq!(body["ok"], true);
    assert_eq!(body["data"]["source"]["type"], "git");
    assert_eq!(body["data"]["source"]["url"], public_https_url);
    assert_eq!(body["data"]["telemetry"]["events"][0]["kind"], "install");
    assert_eq!(body["data"]["telemetry"]["events"][0]["emitted"], true);
    assert!(body["data"]["telemetry"]["events"][0]["suppression_reason"].is_null());
    assert_eq!(
        body["data"]["telemetry"]["events"][0]["source_visibility"],
        "public"
    );
    assert_eq!(
        body["data"]["telemetry"]["events"][0]["public_source"],
        public_https_url
    );
}

#[test]
fn install_warns_and_reports_trust_for_unreviewed_script_imports() {
    let workspace = TestWorkspace::new();
    workspace.write_skill_source("shared-skills", "release-notes");
    workspace.write_file(
        "shared-skills/.agents/skills/release-notes/scripts/release.sh",
        "#!/bin/sh\necho release\n",
    );

    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args([
            "--json",
            "--no-input",
            "--name",
            "release-notes",
            "install",
            "shared-skills",
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::is_empty());

    let body: Value =
        serde_json::from_slice(&assert.get_output().stdout).expect("stdout is valid json");
    let trust = &body["data"]["installed"][0]["trust"];

    assert_eq!(body["command"], "install");
    assert_eq!(body["ok"], true);
    assert!(
        body["warnings"]
            .as_array()
            .expect("warnings array exists")
            .iter()
            .any(|warning| warning
                .as_str()
                .expect("warning exists")
                .contains("contains scripts and remains unreviewed")),
        "unexpected warnings: {body:#?}",
    );
    assert_eq!(trust["source_state"], "imported-unreviewed");
    assert_eq!(trust["effective_state"], "imported-unreviewed");
    assert_eq!(trust["risk_level"], "elevated");
    assert_eq!(trust["contains_scripts"], true);
    assert_eq!(trust["review_required"], true);
}

#[test]
fn telemetry_status_enable_and_disable_use_the_local_state_store() {
    let workspace = TestWorkspace::new();

    let status_assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--json", "telemetry", "status"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    let status_body: Value =
        serde_json::from_slice(&status_assert.get_output().stdout).expect("stdout is valid json");
    assert_eq!(status_body["command"], "telemetry-status");
    assert_eq!(status_body["ok"], true);
    assert_eq!(status_body["data"]["consent"], "unknown");
    assert_eq!(status_body["data"]["notice_seen"], false);
    assert_eq!(status_body["data"]["workspace_enabled"], true);
    assert_eq!(status_body["data"]["workspace_mode"], "public-only");
    assert_eq!(status_body["data"]["effective_enabled"], false);
    assert_eq!(status_body["data"]["effective_mode"], "public-only");

    let enable_assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .env("SOURCE_DATE_EPOCH", "1770000000")
        .args(["--json", "telemetry", "enable"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    let enable_body: Value =
        serde_json::from_slice(&enable_assert.get_output().stdout).expect("stdout is valid json");
    assert_eq!(enable_body["command"], "telemetry-enable");
    assert_eq!(enable_body["ok"], true);
    assert_eq!(enable_body["data"]["consent"], "enabled");
    assert_eq!(enable_body["data"]["notice_seen"], true);
    assert_eq!(
        enable_body["data"]["notice_seen_at"],
        "2026-02-02T02:40:00Z"
    );
    assert_eq!(enable_body["data"]["updated_at"], "2026-02-02T02:40:00Z");
    assert_eq!(enable_body["data"]["effective_enabled"], true);
    assert_eq!(enable_body["data"]["changed"], true);

    let disable_assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .env("SOURCE_DATE_EPOCH", "1770001234")
        .args(["--json", "telemetry", "disable"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    let disable_body: Value =
        serde_json::from_slice(&disable_assert.get_output().stdout).expect("stdout is valid json");
    assert_eq!(disable_body["command"], "telemetry-disable");
    assert_eq!(disable_body["ok"], true);
    assert_eq!(disable_body["data"]["consent"], "disabled");
    assert_eq!(disable_body["data"]["notice_seen"], true);
    assert_eq!(
        disable_body["data"]["notice_seen_at"],
        "2026-02-02T02:40:00Z"
    );
    assert_eq!(disable_body["data"]["updated_at"], "2026-02-02T03:00:34Z");
    assert_eq!(disable_body["data"]["effective_enabled"], false);
    assert_eq!(disable_body["data"]["changed"], true);

    let connection = Connection::open(workspace.home_path().join(".skillctl/state.db"))
        .expect("state database opens");
    let telemetry_row: (String, String, String) = connection
        .query_row(
            "SELECT consent, notice_seen_at, updated_at FROM telemetry_settings",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("telemetry settings row exists");
    assert_eq!(telemetry_row.0, "disabled");
    assert_eq!(telemetry_row.1, "2026-02-02T02:40:00Z");
    assert_eq!(telemetry_row.2, "2026-02-02T03:00:34Z");

    let history_kinds: Vec<String> = connection
        .prepare("SELECT kind FROM history_events WHERE kind = ?1 ORDER BY occurred_at ASC, id ASC")
        .expect("statement prepares")
        .query_map(params!["telemetry-consent-changed"], |row| row.get(0))
        .expect("history query succeeds")
        .collect::<Result<_, _>>()
        .expect("history rows decode");
    assert_eq!(
        history_kinds,
        vec![
            "telemetry-consent-changed".to_string(),
            "telemetry-consent-changed".to_string(),
        ]
    );
}

#[test]
fn validate_reports_invalid_local_skills() {
    let workspace = TestWorkspace::new();
    workspace.write_file(
        ".agents/skills/broken/SKILL.md",
        concat!("---\n", "name: broken\n", "---\n", "\n", "# broken\n"),
    );

    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .args(["--json", "validate"])
        .assert()
        .failure()
        .code(3)
        .stderr(predicate::str::is_empty());

    let output = assert.get_output();
    let body: Value = serde_json::from_slice(&output.stdout).expect("stdout is valid json");

    assert_eq!(body["command"], "validate");
    assert_eq!(body["ok"], false);
    assert_eq!(body["data"]["summary"]["error_count"], 1);
    assert_eq!(body["data"]["issues"][0]["code"], "invalid-skill");
    assert_eq!(body["data"]["issues"][0]["severity"], "error");
    assert!(
        body["data"]["issues"][0]["path"]
            .as_str()
            .expect("path exists")
            .contains(".agents/skills/broken/SKILL.md"),
    );
}

#[test]
fn validate_reports_invalid_overlay_shadow_mappings() {
    let workspace = TestWorkspace::new();
    workspace.write_skill_source("shared-skills", "release-notes");

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
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
        .args(["override", "release-notes"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    workspace.write_file(".agents/overlays/release-notes/extra.md", "# unmanaged\n");

    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--json", "validate"])
        .assert()
        .failure()
        .code(3)
        .stderr(predicate::str::is_empty());

    let output = assert.get_output();
    let body: Value = serde_json::from_slice(&output.stdout).expect("stdout is valid json");

    assert_eq!(body["command"], "validate");
    assert_eq!(body["ok"], false);
    assert!(
        body["data"]["issues"]
            .as_array()
            .expect("issues array exists")
            .iter()
            .any(|issue| {
                issue["code"] == "invalid-overlay-mapping"
                    && issue["path"] == ".agents/overlays/release-notes/extra.md"
            }),
        "unexpected issues: {body:#?}",
    );
}

#[test]
fn init_bootstraps_the_default_workspace_layout() {
    let workspace = TestWorkspace::new();

    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .arg("init")
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    assert
        .stdout(predicate::str::contains("Initialized skillctl workspace"))
        .stdout(predicate::str::contains(".agents/skills"))
        .stdout(predicate::str::contains(".agents/overlays"))
        .stdout(predicate::str::contains(".agents/skillctl.yaml"))
        .stdout(predicate::str::contains(".agents/skillctl.lock"))
        .stdout(predicate::str::contains("Skipped local git excludes"));

    assert!(workspace.path().join(".agents/skills").is_dir());
    assert!(workspace.path().join(".agents/overlays").is_dir());
    assert_eq!(
        fs::read_to_string(workspace.path().join(".agents/skillctl.yaml"))
            .expect("manifest exists"),
        concat!(
            "version: 1\n",
            "\n",
            "targets:\n",
            "  - codex\n",
            "  - gemini-cli\n",
            "  - opencode\n",
        )
    );
    assert_eq!(
        fs::read_to_string(workspace.path().join(".agents/skillctl.lock"))
            .expect("lockfile exists"),
        MINIMAL_LOCKFILE
    );
}

#[test]
fn init_updates_git_info_exclude_without_touching_gitignore_and_is_idempotent() {
    let workspace = TestWorkspace::new();
    fs::create_dir_all(workspace.path().join(".git/info")).expect("git info directory exists");
    fs::write(
        workspace.path().join(".git/info/exclude"),
        "# existing rule\n/.cache/\n",
    )
    .expect("exclude file exists");
    fs::write(
        workspace.path().join(".gitignore"),
        "node_modules/\ncoverage/\n",
    )
    .expect("gitignore exists");

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .arg("init")
        .assert()
        .success()
        .stderr(predicate::str::is_empty())
        .stdout(predicate::str::contains("Updated local git excludes"));

    let exclude_after_first =
        fs::read_to_string(workspace.path().join(".git/info/exclude")).expect("exclude readable");
    assert!(
        exclude_after_first.contains("# existing rule\n/.cache/\n"),
        "preserves existing exclude content",
    );
    assert!(
        exclude_after_first.contains("/.claude/skills/\n"),
        "adds claude projection root",
    );
    assert!(
        exclude_after_first.contains("/.github/skills/\n"),
        "adds github projection root",
    );
    assert!(
        exclude_after_first.contains("/.gemini/skills/\n"),
        "adds gemini projection root",
    );
    assert!(
        exclude_after_first.contains("/.opencode/skills/\n"),
        "adds opencode projection root",
    );
    assert_eq!(
        fs::read_to_string(workspace.path().join(".gitignore")).expect("gitignore readable"),
        "node_modules/\ncoverage/\n"
    );

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .arg("init")
        .assert()
        .success()
        .stderr(predicate::str::is_empty())
        .stdout(predicate::str::contains("No changes were required"));

    let exclude_after_second =
        fs::read_to_string(workspace.path().join(".git/info/exclude")).expect("exclude readable");
    assert_eq!(exclude_after_first, exclude_after_second);
}

#[test]
fn init_json_output_describes_created_and_skipped_items() {
    let workspace = TestWorkspace::new();
    fs::create_dir_all(workspace.path().join(".git/info")).expect("git info directory exists");

    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .args(["--json", "init"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    let output = assert.get_output();
    let body: Value = serde_json::from_slice(&output.stdout).expect("stdout is valid json");

    assert_eq!(
        body,
        json!({
            "ok": true,
            "command": "init",
            "warnings": [],
            "errors": [],
            "data": {
                "created": [
                    ".agents/skills",
                    ".agents/overlays",
                    ".agents/skillctl.yaml",
                    ".agents/skillctl.lock"
                ],
                "skipped": [],
                "git_exclude": {
                    "path": ".git/info/exclude",
                    "created": [
                        "/.claude/skills/",
                        "/.github/skills/",
                        "/.gemini/skills/",
                        "/.opencode/skills/"
                    ],
                    "skipped": []
                }
            }
        })
    );
}

#[test]
fn sync_materializes_generated_copies_without_touching_canonical_skills() {
    let workspace = TestWorkspace::new();
    workspace.write_manifest(concat!(
        "version: 1\n",
        "\n",
        "targets:\n",
        "  - codex\n",
        "  - claude-code\n",
    ));
    workspace.write_lockfile(MINIMAL_LOCKFILE);
    workspace.write_workspace_skill(
        "release-notes",
        "Summarize release notes.",
        &[("notes.md", "# Notes\n")],
    );

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .arg("sync")
        .assert()
        .success()
        .stderr(predicate::str::is_empty())
        .stdout(predicate::str::contains(
            "Materialized 1 generated projection",
        ));

    let generated_skill = workspace
        .path()
        .join(".claude/skills/release-notes/SKILL.md");
    assert_eq!(
        fs::read_to_string(&generated_skill).expect("generated skill exists"),
        fs::read_to_string(
            workspace
                .path()
                .join(".agents/skills/release-notes/SKILL.md")
        )
        .expect("canonical skill exists")
    );
    assert_eq!(
        fs::read_to_string(
            workspace
                .path()
                .join(".claude/skills/release-notes/notes.md")
        )
        .expect("generated note exists"),
        "# Notes\n"
    );
    assert!(
        !workspace
            .path()
            .join(".agents/skills/release-notes/.skillctl-projection.json")
            .exists(),
        "canonical authoring directory must not receive generated metadata",
    );

    let metadata: Value = serde_json::from_str(
        &fs::read_to_string(
            workspace
                .path()
                .join(".claude/skills/release-notes/.skillctl-projection.json"),
        )
        .expect("projection metadata exists"),
    )
    .expect("projection metadata is valid json");
    assert_eq!(metadata["tool"], "skillctl");
    assert_eq!(metadata["generation_mode"], "copy");
    assert_eq!(metadata["physical_root"], ".claude/skills");
    assert_eq!(metadata["skill_name"], "release-notes");
    assert_eq!(metadata["source"]["kind"], "canonical-local");
    assert_eq!(metadata["source"]["scope"], "workspace");
    assert_eq!(
        metadata["source"]["relative_path"],
        ".agents/skills/release-notes"
    );
    assert!(metadata["generated_at"].is_string());
}

#[test]
fn sync_symlink_mode_requires_explicit_override_for_unstable_targets() {
    let workspace = TestWorkspace::new();
    workspace.write_manifest(concat!(
        "version: 1\n",
        "\n",
        "projection:\n",
        "  mode: symlink\n",
        "\n",
        "targets:\n",
        "  - claude-code\n",
    ));
    workspace.write_lockfile(MINIMAL_LOCKFILE);
    workspace.write_workspace_skill("release-notes", "Summarize release notes.", &[]);

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .arg("sync")
        .assert()
        .failure()
        .code(3)
        .stderr(predicate::str::contains("projection.allow_unsafe_targets"))
        .stderr(predicate::str::contains("claude-code"));
}

#[test]
fn sync_refreshes_projection_state_and_history_when_targets_move_generated_roots() {
    let workspace = TestWorkspace::new();
    workspace.write_manifest(concat!(
        "version: 1\n",
        "\n",
        "projection:\n",
        "  policy: prefer-native\n",
        "\n",
        "targets:\n",
        "  - opencode\n",
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

    let manifest_path = workspace.path().join(".agents/skillctl.yaml");
    let manifest_after_install =
        fs::read_to_string(&manifest_path).expect("manifest exists after install");
    fs::write(
        &manifest_path,
        manifest_after_install.replace("  - opencode\n", "  - claude-code\n"),
    )
    .expect("manifest target updates");
    workspace.write_file(
        ".claude/skills/stale-skill/SKILL.md",
        concat!(
            "---\n",
            "name: stale-skill\n",
            "description: Old generated projection.\n",
            "---\n",
            "\n",
            "# Stale\n"
        ),
    );
    workspace.write_file(
        ".claude/skills/stale-skill/.skillctl-projection.json",
        concat!(
            "{\n",
            "  \"tool\": \"skillctl\",\n",
            "  \"generation_mode\": \"copy\",\n",
            "  \"physical_root\": \".claude/skills\"\n",
            "}\n"
        ),
    );

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .env("SOURCE_DATE_EPOCH", "1770001234")
        .arg("sync")
        .assert()
        .success()
        .stderr(predicate::str::is_empty())
        .stdout(predicate::str::contains(
            "Materialized 1 generated projection",
        ))
        .stdout(predicate::str::contains("Pruned 1 stale projection"));

    assert!(
        workspace
            .path()
            .join(".claude/skills/release-notes/SKILL.md")
            .is_file(),
        "sync should project the managed skill into the new runtime root",
    );
    assert!(
        !workspace.path().join(".claude/skills/stale-skill").exists(),
        "sync should prune stale generated projections in the active root",
    );

    let path_assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--json", "path", "release-notes"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    let path_body: Value =
        serde_json::from_slice(&path_assert.get_output().stdout).expect("stdout is valid json");
    assert_eq!(path_body["data"]["projections"][0]["target"], "claude-code");
    assert_eq!(
        path_body["data"]["projections"][0]["root"],
        ".claude/skills"
    );

    let connection = Connection::open(workspace.home_path().join(".skillctl/state.db"))
        .expect("state database opens");
    let projection_rows: Vec<(String, String, String)> = connection
        .prepare(
            "SELECT target, physical_root, projected_path \
             FROM projection_records WHERE skill_id = ?1 ORDER BY target ASC",
        )
        .expect("statement prepares")
        .query_map(params!["release-notes"], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .expect("projection query succeeds")
        .collect::<Result<_, _>>()
        .expect("projection rows decode");
    assert_eq!(
        projection_rows,
        vec![(
            "claude-code".to_string(),
            ".claude/skills".to_string(),
            "release-notes".to_string(),
        )]
    );

    let projection_targets: Vec<String> = connection
        .prepare(
            "SELECT target FROM history_events \
             WHERE skill_id = ?1 AND kind = ?2 ORDER BY occurred_at ASC, id ASC",
        )
        .expect("statement prepares")
        .query_map(params!["release-notes", "projection"], |row| row.get(0))
        .expect("history query succeeds")
        .collect::<Result<_, _>>()
        .expect("history rows decode");
    assert_eq!(
        projection_targets,
        vec!["opencode".to_string(), "claude-code".to_string()]
    );

    let prune_events: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM history_events WHERE skill_id = ?1 AND kind = ?2",
            params!["stale-skill", "prune"],
            |row| row.get(0),
        )
        .expect("prune history query succeeds");
    assert_eq!(prune_events, 1);
}

#[test]
fn sync_symlink_mode_materializes_symlinked_files_and_emits_warnings() {
    let workspace = TestWorkspace::new();
    workspace.write_manifest(concat!(
        "version: 1\n",
        "\n",
        "projection:\n",
        "  mode: symlink\n",
        "  allow_unsafe_targets:\n",
        "    - claude-code\n",
        "\n",
        "targets:\n",
        "  - claude-code\n",
    ));
    workspace.write_lockfile(MINIMAL_LOCKFILE);
    workspace.write_workspace_skill(
        "release-notes",
        "Summarize release notes.",
        &[("notes.md", "# Notes\n")],
    );

    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .args(["--json", "sync"])
        .assert()
        .code(2)
        .stderr(predicate::str::is_empty());
    let body: Value = serde_json::from_slice(&assert.get_output().stdout).expect("stdout is json");

    assert_eq!(body["command"], "sync");
    assert_eq!(body["ok"], true);
    assert_eq!(body["data"]["mode"], "symlink");
    assert!(
        body["warnings"]
            .as_array()
            .expect("warnings array exists")
            .iter()
            .any(|warning| warning
                .as_str()
                .expect("warning is a string")
                .contains("claude-code")),
        "unexpected warnings: {body:#?}",
    );

    let projected_manifest = workspace
        .path()
        .join(".claude/skills/release-notes/SKILL.md");
    let projected_notes = workspace
        .path()
        .join(".claude/skills/release-notes/notes.md");
    assert!(
        fs::symlink_metadata(&projected_manifest)
            .expect("projected manifest exists")
            .file_type()
            .is_symlink(),
        "symlink mode should project SKILL.md as a symlink",
    );
    assert!(
        fs::symlink_metadata(&projected_notes)
            .expect("projected notes exist")
            .file_type()
            .is_symlink(),
        "symlink mode should project auxiliary files as symlinks",
    );

    let metadata: Value = serde_json::from_str(
        &fs::read_to_string(
            workspace
                .path()
                .join(".claude/skills/release-notes/.skillctl-projection.json"),
        )
        .expect("projection metadata exists"),
    )
    .expect("projection metadata is valid json");
    assert_eq!(metadata["generation_mode"], "symlink");
}

#[test]
fn sync_symlink_mode_human_output_includes_warning_lines() {
    let workspace = TestWorkspace::new();
    workspace.write_manifest(concat!(
        "version: 1\n",
        "\n",
        "projection:\n",
        "  mode: symlink\n",
        "  allow_unsafe_targets:\n",
        "  - claude-code\n",
        "\n",
        "targets:\n",
        "  - claude-code\n",
    ));
    workspace.write_lockfile(MINIMAL_LOCKFILE);
    workspace.write_workspace_skill("release-notes", "Summarize release notes.", &[]);

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .arg("sync")
        .assert()
        .code(2)
        .stderr(predicate::str::is_empty())
        .stdout(predicate::str::contains(
            "warning: target 'claude-code' documents unstable symlink behavior",
        ));
}

#[test]
fn install_symlink_mode_records_symlink_projection_state_and_warnings() {
    let workspace = TestWorkspace::new();
    workspace.write_manifest(concat!(
        "version: 1\n",
        "\n",
        "projection:\n",
        "  mode: symlink\n",
        "  allow_unsafe_targets:\n",
        "  - claude-code\n",
        "\n",
        "targets:\n",
        "  - claude-code\n",
    ));
    workspace.write_skill_source("shared-skills", "release-notes");

    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args([
            "--json",
            "--no-input",
            "--name",
            "release-notes",
            "install",
            "shared-skills",
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::is_empty());
    let body: Value = serde_json::from_slice(&assert.get_output().stdout).expect("stdout is json");

    assert_eq!(body["command"], "install");
    assert!(
        body["warnings"]
            .as_array()
            .expect("warnings array exists")
            .iter()
            .any(|warning| warning
                .as_str()
                .expect("warning is a string")
                .contains("claude-code")),
        "unexpected warnings: {body:#?}",
    );
    assert_eq!(body["data"]["projection"]["mode"], "symlink");

    let projected_manifest = workspace
        .path()
        .join(".claude/skills/release-notes/SKILL.md");
    assert!(
        fs::symlink_metadata(&projected_manifest)
            .expect("projected manifest exists")
            .file_type()
            .is_symlink(),
        "install should preserve symlink projection mode",
    );

    let connection = Connection::open(workspace.home_path().join(".skillctl/state.db"))
        .expect("state database opens");
    let generation_mode: String = connection
        .query_row(
            "SELECT generation_mode FROM projection_records WHERE skill_id = ?1",
            params!["release-notes"],
            |row| row.get(0),
        )
        .expect("projection record exists");
    assert_eq!(generation_mode, "symlink");
}

#[test]
fn doctor_reports_symlink_risk_with_override_guidance() {
    let workspace = TestWorkspace::new();
    workspace.write_manifest(concat!(
        "version: 1\n",
        "\n",
        "projection:\n",
        "  mode: symlink\n",
        "\n",
        "targets:\n",
        "  - claude-code\n",
    ));

    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .args(["--json", "doctor"])
        .assert()
        .code(2)
        .stderr(predicate::str::is_empty());
    let body: Value = serde_json::from_slice(&assert.get_output().stdout).expect("stdout is json");
    let issue = body["data"]["issues"]
        .as_array()
        .expect("issues array exists")
        .iter()
        .find(|issue| issue["code"] == "symlink-risk" && issue["target"] == "claude-code")
        .expect("symlink risk issue exists");

    assert!(
        issue["message"]
            .as_str()
            .expect("message exists")
            .contains("projection.allow_unsafe_targets"),
        "unexpected issue: {issue:#?}",
    );
    assert!(
        issue["fix"]
            .as_str()
            .expect("fix exists")
            .contains("projection.allow_unsafe_targets"),
        "unexpected issue: {issue:#?}",
    );
}

#[test]
fn doctor_reports_shadowing_and_projection_drift() {
    let workspace = TestWorkspace::new();
    workspace.write_manifest(concat!(
        "version: 1\n",
        "\n",
        "targets:\n",
        "  - claude-code\n",
    ));
    workspace.write_skill_source("shared-skills", "release-notes");

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
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

    workspace.write_workspace_skill(
        "release-notes",
        "Canonical release notes helper.",
        &[("notes.md", "# Canonical\n")],
    );

    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--json", "doctor"])
        .assert()
        .code(2)
        .stderr(predicate::str::is_empty());

    let output = assert.get_output();
    let body: Value = serde_json::from_slice(&output.stdout).expect("stdout is valid json");
    let issues = body["data"]["issues"]
        .as_array()
        .expect("issues array exists");

    assert_eq!(body["command"], "doctor");
    assert_eq!(body["ok"], true);
    assert!(
        issues
            .iter()
            .any(|issue| issue["code"] == "shadowed-skill" && issue["skill"] == "release-notes"),
        "unexpected issues: {body:#?}",
    );
    assert!(
        issues.iter().any(|issue| {
            issue["code"] == "projection-drift" && issue["skill"] == "release-notes"
        }),
        "unexpected issues: {body:#?}",
    );
}

#[test]
fn doctor_reports_stale_lockfile_entries() {
    let workspace = TestWorkspace::new();
    workspace.write_manifest(concat!("version: 1\n", "\n", "targets:\n", "  - codex\n",));
    workspace.write_lockfile(concat!(
        "version: 1\n",
        "\n",
        "state:\n",
        "  manifest_version: 1\n",
        "  local_state_version: 1\n",
        "\n",
        "imports:\n",
        "  stale-skill:\n",
        "    source:\n",
        "      type: local-path\n",
        "      url: file:///tmp/stale-skill\n",
        "      subpath: .agents/skills/stale-skill\n",
        "    revision:\n",
        "      resolved: deadbeef\n",
        "    timestamps:\n",
        "      fetched_at: 2026-01-01T00:00:00Z\n",
        "      first_installed_at: 2026-01-01T00:00:00Z\n",
        "      last_updated_at: 2026-01-01T00:00:00Z\n",
        "    hashes:\n",
        "      content: sha256:content\n",
        "      overlay: sha256:none\n",
        "      effective_version: sha256:effective\n",
    ));

    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--json", "doctor"])
        .assert()
        .code(2)
        .stderr(predicate::str::is_empty());

    let output = assert.get_output();
    let body: Value = serde_json::from_slice(&output.stdout).expect("stdout is valid json");

    assert!(
        body["data"]["issues"]
            .as_array()
            .expect("issues array exists")
            .iter()
            .any(|issue| issue["code"] == "stale-lockfile-entry" && issue["skill"] == "stale-skill"),
        "unexpected issues: {body:#?}",
    );
}

#[test]
fn doctor_reports_trust_details_for_unreviewed_script_imports() {
    let workspace = TestWorkspace::new();
    workspace.write_skill_source("shared-skills", "release-notes");
    workspace.write_file(
        "shared-skills/.agents/skills/release-notes/scripts/release.sh",
        "#!/bin/sh\necho release\n",
    );

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args([
            "--no-input",
            "--name",
            "release-notes",
            "install",
            "shared-skills",
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::is_empty());

    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--json", "doctor"])
        .assert()
        .code(2)
        .stderr(predicate::str::is_empty());

    let body: Value =
        serde_json::from_slice(&assert.get_output().stdout).expect("stdout is valid json");
    let issue = body["data"]["issues"]
        .as_array()
        .expect("issues array exists")
        .iter()
        .find(|issue| issue["code"] == "script-risk" && issue["skill"] == "release-notes")
        .expect("script-risk issue exists");

    assert_eq!(issue["trust"]["source_state"], "imported-unreviewed");
    assert_eq!(issue["trust"]["effective_state"], "imported-unreviewed");
    assert_eq!(issue["trust"]["risk_level"], "elevated");
    assert_eq!(issue["trust"]["contains_scripts"], true);
}

#[test]
fn explain_reports_winner_shadowed_candidates_visibility_and_drift() {
    let workspace = TestWorkspace::new();
    workspace.write_manifest(concat!(
        "version: 1\n",
        "\n",
        "targets:\n",
        "  - claude-code\n",
    ));
    workspace.write_skill_source("shared-skills", "release-notes");

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
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

    workspace.write_workspace_skill(
        "release-notes",
        "Canonical release notes helper.",
        &[("notes.md", "# Canonical\n")],
    );

    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--json", "explain", "release-notes"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    let output = assert.get_output();
    let body: Value = serde_json::from_slice(&output.stdout).expect("stdout is valid json");

    assert_eq!(body["command"], "explain");
    assert_eq!(body["ok"], true);
    assert_eq!(body["data"]["skill"], "release-notes");
    assert_eq!(body["data"]["status"], "selected");
    assert_eq!(body["data"]["winner"]["source_class"], "canonical-local");
    assert_eq!(
        body["data"]["winner"]["root"],
        ".agents/skills/release-notes"
    );
    assert_eq!(body["data"]["shadowed"][0]["source_class"], "imported");
    assert_eq!(body["data"]["targets"][0]["target"], "claude-code");
    assert_eq!(body["data"]["targets"][0]["root"], ".claude/skills");
    assert_eq!(body["data"]["targets"][0]["visible"], true);
    assert_eq!(
        body["data"]["drift"]["active_projection_matches_winner"],
        false
    );
    assert_eq!(
        body["data"]["drift"]["active_differs_from_pinned_source"],
        true
    );
}

#[test]
fn sync_prunes_only_prior_skillctl_generated_projections() {
    let workspace = TestWorkspace::new();
    workspace.write_manifest(concat!(
        "version: 1\n",
        "\n",
        "targets:\n",
        "  - claude-code\n",
    ));
    workspace.write_lockfile(MINIMAL_LOCKFILE);
    workspace.write_workspace_skill("release-notes", "Summarize release notes.", &[]);
    workspace.write_file(
        ".claude/skills/stale-skill/SKILL.md",
        concat!(
            "---\n",
            "name: stale-skill\n",
            "description: Old generated projection.\n",
            "---\n",
            "\n",
            "# Stale\n"
        ),
    );
    workspace.write_file(
        ".claude/skills/stale-skill/.skillctl-projection.json",
        concat!(
            "{\n",
            "  \"tool\": \"skillctl\",\n",
            "  \"generation_mode\": \"copy\",\n",
            "  \"physical_root\": \".claude/skills\"\n",
            "}\n"
        ),
    );
    workspace.write_file(
        ".claude/skills/manual-skill/SKILL.md",
        concat!(
            "---\n",
            "name: manual-skill\n",
            "description: Hand-authored runtime skill.\n",
            "---\n",
            "\n",
            "# Manual\n"
        ),
    );

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .arg("sync")
        .assert()
        .success()
        .stderr(predicate::str::is_empty())
        .stdout(predicate::str::contains("Pruned 1 stale projection"));

    assert!(
        !workspace.path().join(".claude/skills/stale-skill").exists(),
        "stale generated projection should be pruned",
    );
    assert!(
        workspace
            .path()
            .join(".claude/skills/manual-skill")
            .is_dir(),
        "hand-authored runtime directories must be preserved",
    );
    assert!(
        workspace
            .path()
            .join(".claude/skills/release-notes")
            .is_dir(),
        "current projection should be materialized",
    );
}

#[test]
fn sync_refuses_to_overwrite_hand_authored_runtime_skill_directories() {
    let workspace = TestWorkspace::new();
    workspace.write_manifest(concat!(
        "version: 1\n",
        "\n",
        "targets:\n",
        "  - claude-code\n",
    ));
    workspace.write_lockfile(MINIMAL_LOCKFILE);
    workspace.write_workspace_skill("release-notes", "Summarize release notes.", &[]);
    workspace.write_file(
        ".claude/skills/release-notes/SKILL.md",
        concat!(
            "---\n",
            "name: release-notes\n",
            "description: Manual runtime copy.\n",
            "---\n",
            "\n",
            "# Manual\n"
        ),
    );

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .arg("sync")
        .assert()
        .failure()
        .code(3)
        .stderr(predicate::str::contains(
            "refusing to overwrite hand-authored runtime skill directory",
        ));
}

#[test]
fn override_creates_a_minimal_overlay_and_updates_manifest_lockfile_and_state() {
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

    let installed_lockfile = workspace.read_lockfile_yaml();
    let original_overlay_hash = installed_lockfile["imports"]["release-notes"]["hashes"]["overlay"]
        .as_str()
        .expect("overlay hash exists")
        .to_string();
    let original_effective_version =
        installed_lockfile["imports"]["release-notes"]["hashes"]["effective_version"]
            .as_str()
            .expect("effective version exists")
            .to_string();
    assert_eq!(original_overlay_hash, "sha256:none");

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .env("SOURCE_DATE_EPOCH", "1770001234")
        .args(["override", "release-notes"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty())
        .stdout(predicate::str::contains(
            "Created overlay for release-notes",
        ))
        .stdout(predicate::str::contains(".agents/overlays/release-notes"));

    let overlay_manifest_path = workspace
        .path()
        .join(".agents/overlays/release-notes/SKILL.md");
    assert!(
        overlay_manifest_path.is_file(),
        "overlay manifest should exist"
    );
    assert_eq!(
        fs::read_to_string(&overlay_manifest_path).expect("overlay manifest exists"),
        fs::read_to_string(
            workspace.home_path().join(
                ".skillctl/store/imports/release-notes/.agents/skills/release-notes/SKILL.md"
            )
        )
        .expect("stored import manifest exists")
    );

    let manifest = fs::read_to_string(workspace.path().join(".agents/skillctl.yaml"))
        .expect("manifest exists");
    assert!(
        manifest.contains("overrides:\n  release-notes: .agents/overlays/release-notes\n"),
        "manifest was {manifest}",
    );

    let lockfile = workspace.read_lockfile_yaml();
    let overlay_hash = lockfile["imports"]["release-notes"]["hashes"]["overlay"]
        .as_str()
        .expect("overlay hash exists")
        .to_string();
    let effective_version = lockfile["imports"]["release-notes"]["hashes"]["effective_version"]
        .as_str()
        .expect("effective version exists")
        .to_string();
    assert_ne!(overlay_hash, "sha256:none");
    assert_ne!(overlay_hash, original_overlay_hash);
    assert_ne!(effective_version, original_effective_version);
    assert_eq!(
        lockfile["imports"]["release-notes"]["timestamps"]["last_updated_at"]
            .as_str()
            .expect("timestamp exists"),
        "2026-02-02T03:00:34Z"
    );

    let connection = Connection::open(workspace.home_path().join(".skillctl/state.db"))
        .expect("state database opens");
    let state_row: (String, String, String) = connection
        .query_row(
            "SELECT overlay_hash, effective_version_hash, updated_at \
             FROM install_records WHERE skill_id = ?1",
            params!["release-notes"],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("install record exists");
    assert_eq!(state_row.0, overlay_hash);
    assert_eq!(state_row.1, effective_version);
    assert_eq!(state_row.2, "2026-02-02T03:00:34Z");

    let pin_effective_version: Option<String> = connection
        .query_row(
            "SELECT effective_version_hash FROM pins WHERE skill_id = ?1",
            params!["release-notes"],
            |row| row.get(0),
        )
        .expect("pin record exists");
    assert_eq!(
        pin_effective_version.as_deref(),
        Some(effective_version.as_str())
    );

    let history_kinds: Vec<String> = connection
        .prepare(
            "SELECT kind FROM history_events WHERE skill_id = ?1 ORDER BY occurred_at ASC, id ASC",
        )
        .expect("statement prepares")
        .query_map(params!["release-notes"], |row| row.get(0))
        .expect("history query succeeds")
        .collect::<Result<_, _>>()
        .expect("history rows decode");
    assert_eq!(history_kinds.first().map(String::as_str), Some("install"));
    assert_eq!(
        history_kinds.last().map(String::as_str),
        Some("overlay-created")
    );
    assert!(
        history_kinds.iter().any(|kind| kind == "projection"),
        "history should retain projection events: {history_kinds:?}",
    );
}

#[test]
fn override_is_idempotent_and_does_not_clobber_existing_overlay_edits() {
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
        .env("SOURCE_DATE_EPOCH", "1770001234")
        .args(["override", "release-notes"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    let overlay_path = workspace
        .path()
        .join(".agents/overlays/release-notes/SKILL.md");
    fs::write(
        &overlay_path,
        concat!(
            "---\n",
            "name: release-notes\n",
            "description: Local overlay version.\n",
            "---\n",
            "\n",
            "# release-notes\n"
        ),
    )
    .expect("overlay file updated");

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .env("SOURCE_DATE_EPOCH", "1770002468")
        .args(["override", "release-notes"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty())
        .stdout(predicate::str::contains(
            "Overlay for release-notes is ready",
        ));

    assert!(
        fs::read_to_string(&overlay_path)
            .expect("overlay exists")
            .contains("Local overlay version."),
        "existing overlay edits should be preserved",
    );

    let lockfile = workspace.read_lockfile_yaml();
    assert_ne!(
        lockfile["imports"]["release-notes"]["hashes"]["overlay"]
            .as_str()
            .expect("overlay hash exists"),
        "sha256:none"
    );
    assert_eq!(
        lockfile["imports"]["release-notes"]["timestamps"]["last_updated_at"]
            .as_str()
            .expect("timestamp exists"),
        "2026-02-02T03:21:08Z"
    );
}

#[test]
fn override_rolls_back_when_the_transaction_fails_after_state_write() {
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

    let snapshot = workspace.snapshot_paths(&[
        ".agents/skillctl.yaml",
        ".agents/skillctl.lock",
        ".agents/overlays/release-notes",
        "home/.skillctl/state.db",
    ]);

    assert_transaction_rolled_back(
        &workspace,
        "override:after-state",
        "1770001234",
        &["override", "release-notes"],
        &snapshot,
    );
}

#[test]
fn update_checks_git_upstream_and_records_a_safe_apply_plan() {
    let workspace = TestWorkspace::new();
    workspace.write_skill_source("git-source", "release-notes");
    workspace.init_git_repo("git-source");
    let repo_url = workspace.git_repo_url("git-source");

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args([
            "--no-input",
            "--name",
            "release-notes",
            "install",
            repo_url.as_str(),
        ])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    workspace.write_skill_source_at(
        "git-source",
        ".agents/skills/release-notes",
        "release-notes",
        "Updated upstream release notes helper.",
    );
    workspace.commit_all("git-source", "update release notes");

    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--json", "update", "release-notes"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    let output = assert.get_output();
    let body: Value = serde_json::from_slice(&output.stdout).expect("stdout is valid json");
    let plan = &body["data"]["plans"][0];

    assert_eq!(body["command"], "update");
    assert_eq!(body["ok"], true);
    assert_eq!(plan["skill"], "release-notes");
    assert_eq!(plan["scope"], "workspace");
    assert_eq!(plan["outcome"], "update-available");
    assert_eq!(plan["recommended_action"], "apply");
    assert_eq!(plan["overlay_detected"], false);
    assert_eq!(plan["local_modification_detected"], false);
    assert_eq!(plan["modifications"], json!([]));
    assert_ne!(
        plan["pinned_revision"]
            .as_str()
            .expect("pinned revision exists"),
        plan["latest_revision"]
            .as_str()
            .expect("latest revision exists"),
    );

    let connection = Connection::open(workspace.home_path().join(".skillctl/state.db"))
        .expect("state database opens");
    let update_row: (String, String, i64, i64) = connection
        .query_row(
            "SELECT outcome, latest_revision, overlay_detected, local_modification_detected \
             FROM update_checks WHERE skill_id = ?1 ORDER BY checked_at DESC, id DESC LIMIT 1",
            params!["release-notes"],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("update check exists");
    assert_eq!(update_row.0, "update-available");
    assert_eq!(update_row.2, 0);
    assert_eq!(update_row.3, 0);
    assert_eq!(
        update_row.1,
        plan["latest_revision"]
            .as_str()
            .expect("latest revision exists"),
    );

    let history_kinds: Vec<String> = connection
        .prepare(
            "SELECT kind FROM history_events WHERE skill_id = ?1 ORDER BY occurred_at ASC, id ASC",
        )
        .expect("statement prepares")
        .query_map(params!["release-notes"], |row| row.get(0))
        .expect("history query succeeds")
        .collect::<Result<_, _>>()
        .expect("history rows decode");
    assert_eq!(
        history_kinds.last().map(String::as_str),
        Some("update-check")
    );
    assert_eq!(body["data"]["telemetry"]["events"][0]["kind"], "update");
    assert_eq!(body["data"]["telemetry"]["events"][0]["emitted"], false);
    assert_eq!(
        body["data"]["telemetry"]["events"][0]["suppression_reason"],
        "local-source"
    );
    assert_eq!(
        body["data"]["telemetry"]["events"][0]["source_visibility"],
        "suppressed-local"
    );
}

#[test]
fn update_emits_remote_telemetry_for_verified_public_https_git_sources() {
    let workspace = TestWorkspace::new();
    workspace.write_skill_source("git-source", "release-notes");
    workspace.init_git_repo("git-source");

    let public_https_url = "https://github.com/vercel/ai.git";
    let git_config_path = workspace.path().join("gitconfig");
    fs::write(
        &git_config_path,
        format!(
            "[url \"{}\"]\n\tinsteadOf = {}\n",
            workspace.git_repo_url("git-source"),
            public_https_url
        ),
    )
    .expect("git config written");

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .env("GIT_CONFIG_GLOBAL", &git_config_path)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .args([
            "--no-input",
            "--name",
            "release-notes",
            "install",
            public_https_url,
        ])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    workspace.write_skill_source_at(
        "git-source",
        ".agents/skills/release-notes",
        "release-notes",
        "Updated upstream release notes helper.",
    );
    workspace.commit_all("git-source", "update release notes");

    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .env("GIT_CONFIG_GLOBAL", &git_config_path)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .args(["--json", "update", "release-notes"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    let body: Value = serde_json::from_slice(&assert.get_output().stdout).expect("stdout is json");
    let telemetry = &body["data"]["telemetry"]["events"][0];

    assert_eq!(body["command"], "update");
    assert_eq!(body["ok"], true);
    assert_eq!(telemetry["kind"], "update");
    assert_eq!(telemetry["emitted"], true);
    assert!(telemetry["suppression_reason"].is_null());
    assert_eq!(telemetry["source_visibility"], "public");
    assert_eq!(telemetry["public_source"], public_https_url);
}

#[test]
fn update_treats_managed_overlays_as_safe_and_does_not_flag_stale_projections() {
    let workspace = TestWorkspace::new();
    workspace.write_manifest(concat!(
        "version: 1\n",
        "\n",
        "targets:\n",
        "  - claude-code\n",
    ));
    workspace.write_skill_source("git-source", "release-notes");
    workspace.init_git_repo("git-source");
    let repo_url = workspace.git_repo_url("git-source");

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args([
            "--no-input",
            "--name",
            "release-notes",
            "install",
            repo_url.as_str(),
        ])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["override", "release-notes"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    workspace.write_skill_source_at(
        "git-source",
        ".agents/skills/release-notes",
        "release-notes",
        "Updated upstream release notes helper.",
    );
    workspace.commit_all("git-source", "update release notes");

    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--json", "update", "release-notes"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    let output = assert.get_output();
    let body: Value = serde_json::from_slice(&output.stdout).expect("stdout is valid json");
    let plan = &body["data"]["plans"][0];

    assert_eq!(plan["outcome"], "update-available");
    assert_eq!(plan["recommended_action"], "apply");
    assert_eq!(plan["overlay_detected"], true);
    assert_eq!(plan["local_modification_detected"], false);
    assert_eq!(plan["modifications"][0]["kind"], "overlay");
    assert_eq!(plan["modifications"][0]["managed"], true);

    let connection = Connection::open(workspace.home_path().join(".skillctl/state.db"))
        .expect("state database opens");
    let direct_modification_events: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM history_events WHERE skill_id = ?1 AND kind = ?2",
            params!["release-notes", "direct-modification-detected"],
            |row| row.get(0),
        )
        .expect("history query succeeds");
    assert_eq!(direct_modification_events, 0);
}

#[test]
fn update_uses_manifest_configured_overlay_roots_when_detecting_managed_overlays() {
    let workspace = TestWorkspace::new();
    workspace.write_manifest(concat!(
        "version: 1\n",
        "\n",
        "layout:\n",
        "  overlays_dir: .agents/custom-overlays\n",
        "targets:\n",
        "  - claude-code\n",
    ));
    workspace.write_skill_source("git-source", "release-notes");
    workspace.init_git_repo("git-source");
    let repo_url = workspace.git_repo_url("git-source");

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args([
            "--no-input",
            "--name",
            "release-notes",
            "install",
            repo_url.as_str(),
        ])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["override", "release-notes"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    workspace.write_skill_source_at(
        "git-source",
        ".agents/skills/release-notes",
        "release-notes",
        "Updated upstream release notes helper.",
    );
    workspace.commit_all("git-source", "update release notes");

    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--json", "update", "release-notes"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    let output = assert.get_output();
    let body: Value = serde_json::from_slice(&output.stdout).expect("stdout is valid json");
    let plan = &body["data"]["plans"][0];

    assert_eq!(plan["overlay_detected"], true);
    assert_eq!(plan["local_modification_detected"], false);
    assert_eq!(plan["modifications"][0]["kind"], "overlay");
    assert_eq!(plan["modifications"][0]["managed"], true);
    assert_eq!(
        plan["modifications"][0]["path"],
        ".agents/custom-overlays/release-notes"
    );
}

#[test]
fn update_blocks_when_a_projected_copy_was_edited_directly() {
    let workspace = TestWorkspace::new();
    workspace.write_manifest(concat!(
        "version: 1\n",
        "\n",
        "targets:\n",
        "  - claude-code\n",
    ));
    workspace.write_skill_source("git-source", "release-notes");
    workspace.init_git_repo("git-source");
    let repo_url = workspace.git_repo_url("git-source");

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args([
            "--no-input",
            "--name",
            "release-notes",
            "install",
            repo_url.as_str(),
        ])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    fs::write(
        workspace
            .path()
            .join(".claude/skills/release-notes/SKILL.md"),
        concat!(
            "---\n",
            "name: release-notes\n",
            "description: Directly edited runtime copy.\n",
            "---\n",
            "\n",
            "# release-notes\n"
        ),
    )
    .expect("projected copy edited");

    workspace.write_skill_source_at(
        "git-source",
        ".agents/skills/release-notes",
        "release-notes",
        "Updated upstream release notes helper.",
    );
    workspace.commit_all("git-source", "update release notes");

    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--json", "update", "release-notes"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    let output = assert.get_output();
    let body: Value = serde_json::from_slice(&output.stdout).expect("stdout is valid json");
    let plan = &body["data"]["plans"][0];

    assert_eq!(plan["outcome"], "blocked");
    assert_eq!(plan["recommended_action"], "create-overlay");
    assert_eq!(plan["overlay_detected"], false);
    assert_eq!(plan["local_modification_detected"], true);
    assert_eq!(plan["modifications"][0]["kind"], "projected-copy");
    assert_eq!(plan["modifications"][0]["managed"], false);
    assert!(
        plan["modifications"][0]["path"]
            .as_str()
            .expect("path exists")
            .contains(".claude/skills/release-notes/SKILL.md"),
    );

    let connection = Connection::open(workspace.home_path().join(".skillctl/state.db"))
        .expect("state database opens");
    let update_row: (String, i64) = connection
        .query_row(
            "SELECT outcome, local_modification_detected \
             FROM update_checks WHERE skill_id = ?1 ORDER BY checked_at DESC, id DESC LIMIT 1",
            params!["release-notes"],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("update check exists");
    assert_eq!(update_row.0, "blocked");
    assert_eq!(update_row.1, 1);

    let modification_kinds: Vec<String> = connection
        .prepare(
            "SELECT kind FROM local_modifications WHERE skill_id = ?1 ORDER BY detected_at ASC, id ASC",
        )
        .expect("statement prepares")
        .query_map(params!["release-notes"], |row| row.get(0))
        .expect("modification query succeeds")
        .collect::<Result<_, _>>()
        .expect("modification rows decode");
    assert_eq!(modification_kinds, vec!["projected-copy".to_string()]);
}

#[test]
fn update_downgrades_apply_recommendations_for_unreviewed_script_imports() {
    let workspace = TestWorkspace::new();
    workspace.write_skill_source("git-source", "release-notes");
    workspace.write_file(
        "git-source/.agents/skills/release-notes/scripts/release.sh",
        "#!/bin/sh\necho release\n",
    );
    workspace.init_git_repo("git-source");
    let repo_url = workspace.git_repo_url("git-source");

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args([
            "--no-input",
            "--name",
            "release-notes",
            "install",
            repo_url.as_str(),
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::is_empty());

    workspace.write_skill_source_at(
        "git-source",
        ".agents/skills/release-notes",
        "release-notes",
        "Updated upstream release notes helper.",
    );
    workspace.commit_all("git-source", "update release notes");

    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--json", "update", "release-notes"])
        .assert()
        .code(2)
        .stderr(predicate::str::is_empty());

    let body: Value =
        serde_json::from_slice(&assert.get_output().stdout).expect("stdout is valid json");
    let plan = &body["data"]["plans"][0];
    let trust = &plan["trust"];

    assert_eq!(plan["outcome"], "update-available");
    assert_eq!(plan["recommended_action"], "skip");
    assert_eq!(plan["available_actions"], json!(["skip"]));
    assert_eq!(trust["source_state"], "imported-unreviewed");
    assert_eq!(trust["risk_level"], "elevated");
    assert_eq!(trust["contains_scripts"], true);
    assert_eq!(trust["blocked_actions"], json!(["apply-update"]));
    assert!(
        plan["notes"]
            .as_array()
            .expect("notes array exists")
            .iter()
            .any(|note| note.as_str().expect("note exists").contains("trust gate")),
        "unexpected notes: {body:#?}",
    );

    let connection = Connection::open(workspace.home_path().join(".skillctl/state.db"))
        .expect("state database opens");
    let details_json: String = connection
        .query_row(
            "SELECT details_json FROM history_events \
             WHERE skill_id = ?1 AND kind = ?2 ORDER BY occurred_at DESC, id DESC LIMIT 1",
            params!["release-notes", "update-check"],
            |row| row.get(0),
        )
        .expect("update-check history exists");
    let details: Value = serde_json::from_str(&details_json).expect("details json is valid");
    assert_eq!(details["trust"]["risk_level"], "elevated");
    assert_eq!(details["trust"]["blocked_actions"], json!(["apply-update"]));
}

#[test]
fn pin_updates_manifest_lockfile_state_and_projection_metadata() {
    let workspace = TestWorkspace::new();
    workspace.write_manifest(concat!(
        "version: 1\n",
        "\n",
        "targets:\n",
        "  - claude-code\n",
    ));
    workspace.write_skill_source("git-source", "release-notes");
    workspace.init_git_repo("git-source");
    let repo_url = workspace.git_repo_url("git-source");

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
            repo_url.as_str(),
        ])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    workspace.write_skill_source_at(
        "git-source",
        ".agents/skills/release-notes",
        "release-notes",
        "Pinned release notes helper.",
    );
    workspace.commit_all("git-source", "pin release notes");
    let pinned_commit = workspace.git_head("git-source");

    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .env("SOURCE_DATE_EPOCH", "1770001234")
        .args(["--json", "pin", "release-notes", pinned_commit.as_str()])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    let output = assert.get_output();
    let body: Value = serde_json::from_slice(&output.stdout).expect("stdout is valid json");
    assert_eq!(body["command"], "pin");
    assert_eq!(body["ok"], true);
    assert_eq!(body["data"]["skill"], "release-notes");
    assert_eq!(body["data"]["resolved_revision"], pinned_commit);
    assert_eq!(body["data"]["scope"], "workspace");

    let manifest = fs::read_to_string(workspace.path().join(".agents/skillctl.yaml"))
        .expect("manifest exists");
    assert!(
        manifest.contains(&format!("ref: {pinned_commit}")),
        "manifest was {manifest}",
    );

    let lockfile = workspace.read_lockfile_yaml();
    let pinned_effective_version =
        lockfile["imports"]["release-notes"]["hashes"]["effective_version"]
            .as_str()
            .expect("effective version exists")
            .to_string();
    assert_eq!(
        lockfile["imports"]["release-notes"]["revision"]["resolved"]
            .as_str()
            .expect("resolved revision exists"),
        pinned_commit
    );
    assert_eq!(
        lockfile["imports"]["release-notes"]["timestamps"]["last_updated_at"]
            .as_str()
            .expect("timestamp exists"),
        "2026-02-02T03:00:34Z"
    );

    assert!(
        fs::read_to_string(
            workspace
                .path()
                .join(".claude/skills/release-notes/SKILL.md")
        )
        .expect("projected manifest exists")
        .contains("Pinned release notes helper."),
        "projection should reflect the pinned revision",
    );

    let projection_metadata: Value = serde_json::from_str(
        &fs::read_to_string(
            workspace
                .path()
                .join(".claude/skills/release-notes/.skillctl-projection.json"),
        )
        .expect("projection metadata exists"),
    )
    .expect("projection metadata is valid json");
    assert_eq!(
        projection_metadata["source"]["resolved_revision"],
        pinned_commit.as_str()
    );
    assert_eq!(
        projection_metadata["source"]["effective_version_hash"],
        pinned_effective_version.as_str()
    );

    let connection = Connection::open(workspace.home_path().join(".skillctl/state.db"))
        .expect("state database opens");
    let install_row: (String, String, String) = connection
        .query_row(
            "SELECT resolved_revision, effective_version_hash, updated_at \
             FROM install_records WHERE skill_id = ?1",
            params!["release-notes"],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("install record exists");
    assert_eq!(install_row.0, pinned_commit);
    assert_eq!(install_row.1, pinned_effective_version);
    assert_eq!(install_row.2, "2026-02-02T03:00:34Z");

    let pin_row: (String, String, String, String) = connection
        .query_row(
            "SELECT requested_reference, resolved_revision, effective_version_hash, pinned_at \
             FROM pins WHERE skill_id = ?1",
            params!["release-notes"],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("pin record exists");
    assert_eq!(pin_row.0, pinned_commit);
    assert_eq!(pin_row.1, pinned_commit);
    assert_eq!(pin_row.2, pinned_effective_version);
    assert_eq!(pin_row.3, "2026-02-02T03:00:34Z");

    let history_kinds: Vec<String> = connection
        .prepare(
            "SELECT kind FROM history_events WHERE skill_id = ?1 ORDER BY occurred_at ASC, id ASC",
        )
        .expect("statement prepares")
        .query_map(params!["release-notes"], |row| row.get(0))
        .expect("history query succeeds")
        .collect::<Result<_, _>>()
        .expect("history rows decode");
    assert_eq!(
        history_kinds,
        vec![
            "install".to_string(),
            "projection".to_string(),
            "pin".to_string(),
            "projection".to_string(),
        ]
    );
}

#[test]
fn pin_rolls_back_when_the_transaction_fails_after_state_write() {
    let workspace = TestWorkspace::new();
    workspace.write_manifest(concat!(
        "version: 1\n",
        "\n",
        "targets:\n",
        "  - claude-code\n",
    ));
    workspace.write_skill_source("git-source", "release-notes");
    workspace.init_git_repo("git-source");
    let repo_url = workspace.git_repo_url("git-source");

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
            repo_url.as_str(),
        ])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    workspace.write_skill_source_at(
        "git-source",
        ".agents/skills/release-notes",
        "release-notes",
        "Pinned release notes helper.",
    );
    workspace.commit_all("git-source", "pin release notes");
    let pinned_commit = workspace.git_head("git-source");

    let snapshot = workspace.snapshot_paths(&[
        ".agents/skillctl.yaml",
        ".agents/skillctl.lock",
        "home/.skillctl/state.db",
        "home/.skillctl/store/imports/release-notes",
        ".claude/skills",
    ]);

    assert_transaction_rolled_back(
        &workspace,
        "pin:after-state",
        "1770001234",
        &["pin", "release-notes", pinned_commit.as_str()],
        &snapshot,
    );
}

#[test]
fn rollback_reactivates_a_prior_effective_version_from_recorded_history() {
    let workspace = TestWorkspace::new();
    workspace.write_manifest(concat!(
        "version: 1\n",
        "\n",
        "targets:\n",
        "  - claude-code\n",
    ));
    workspace.write_skill_source("git-source", "release-notes");
    workspace.init_git_repo("git-source");
    let repo_url = workspace.git_repo_url("git-source");

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
            repo_url.as_str(),
        ])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    let initial_lockfile = workspace.read_lockfile_yaml();
    let initial_commit = initial_lockfile["imports"]["release-notes"]["revision"]["resolved"]
        .as_str()
        .expect("initial resolved revision exists")
        .to_string();
    let initial_effective_version =
        initial_lockfile["imports"]["release-notes"]["hashes"]["effective_version"]
            .as_str()
            .expect("initial effective version exists")
            .to_string();

    workspace.write_skill_source_at(
        "git-source",
        ".agents/skills/release-notes",
        "release-notes",
        "Rolled forward release notes helper.",
    );
    workspace.commit_all("git-source", "roll forward release notes");
    let newer_commit = workspace.git_head("git-source");

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .env("SOURCE_DATE_EPOCH", "1770001234")
        .args(["pin", "release-notes", newer_commit.as_str()])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .env("SOURCE_DATE_EPOCH", "1770002468")
        .args([
            "--json",
            "rollback",
            "release-notes",
            initial_effective_version.as_str(),
        ])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    let output = assert.get_output();
    let body: Value = serde_json::from_slice(&output.stdout).expect("stdout is valid json");
    assert_eq!(body["command"], "rollback");
    assert_eq!(body["ok"], true);
    assert_eq!(body["data"]["skill"], "release-notes");
    assert_eq!(body["data"]["requested_version"], initial_effective_version);
    assert_eq!(body["data"]["resolved_revision"], initial_commit);
    assert_eq!(
        body["data"]["effective_version_hash"],
        initial_effective_version
    );

    let lockfile = workspace.read_lockfile_yaml();
    assert_eq!(
        lockfile["imports"]["release-notes"]["revision"]["resolved"]
            .as_str()
            .expect("resolved revision exists"),
        initial_commit
    );
    assert_eq!(
        lockfile["imports"]["release-notes"]["hashes"]["effective_version"]
            .as_str()
            .expect("effective version exists"),
        initial_effective_version
    );
    assert_eq!(
        lockfile["imports"]["release-notes"]["timestamps"]["last_updated_at"]
            .as_str()
            .expect("timestamp exists"),
        "2026-02-02T03:21:08Z"
    );

    assert!(
        fs::read_to_string(
            workspace
                .path()
                .join(".claude/skills/release-notes/SKILL.md")
        )
        .expect("projected manifest exists")
        .contains("Summarize release notes."),
        "projection should revert to the previously recorded effective version",
    );

    let projection_metadata: Value = serde_json::from_str(
        &fs::read_to_string(
            workspace
                .path()
                .join(".claude/skills/release-notes/.skillctl-projection.json"),
        )
        .expect("projection metadata exists"),
    )
    .expect("projection metadata is valid json");
    assert_eq!(
        projection_metadata["source"]["resolved_revision"],
        initial_commit.as_str()
    );
    assert_eq!(
        projection_metadata["source"]["effective_version_hash"],
        initial_effective_version.as_str()
    );

    let connection = Connection::open(workspace.home_path().join(".skillctl/state.db"))
        .expect("state database opens");
    let install_row: (String, String, String) = connection
        .query_row(
            "SELECT resolved_revision, effective_version_hash, updated_at \
             FROM install_records WHERE skill_id = ?1",
            params!["release-notes"],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("install record exists");
    assert_eq!(install_row.0, initial_commit);
    assert_eq!(install_row.1, initial_effective_version);
    assert_eq!(install_row.2, "2026-02-02T03:21:08Z");

    let pin_row: (String, String, String) = connection
        .query_row(
            "SELECT requested_reference, resolved_revision, effective_version_hash \
             FROM pins WHERE skill_id = ?1",
            params!["release-notes"],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("pin record exists");
    assert_eq!(pin_row.0, initial_effective_version);
    assert_eq!(pin_row.1, initial_commit);
    assert_eq!(pin_row.2, initial_effective_version);

    let rollback_row: (String, String, String) = connection
        .query_row(
            "SELECT rolled_back_at, from_reference, to_reference \
             FROM rollback_records WHERE skill_id = ?1 ORDER BY rolled_back_at DESC, id DESC LIMIT 1",
            params!["release-notes"],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("rollback record exists");
    assert_eq!(rollback_row.0, "2026-02-02T03:21:08Z");
    assert_eq!(rollback_row.1, newer_commit);
    assert_eq!(rollback_row.2, initial_effective_version);

    let history_kinds: Vec<String> = connection
        .prepare(
            "SELECT kind FROM history_events WHERE skill_id = ?1 ORDER BY occurred_at ASC, id ASC",
        )
        .expect("statement prepares")
        .query_map(params!["release-notes"], |row| row.get(0))
        .expect("history query succeeds")
        .collect::<Result<_, _>>()
        .expect("history rows decode");
    assert_eq!(
        history_kinds,
        vec![
            "install".to_string(),
            "projection".to_string(),
            "pin".to_string(),
            "projection".to_string(),
            "rollback".to_string(),
            "projection".to_string(),
        ]
    );
}

#[test]
fn rollback_rolls_back_when_the_transaction_fails_after_state_write() {
    let workspace = TestWorkspace::new();
    workspace.write_manifest(concat!(
        "version: 1\n",
        "\n",
        "targets:\n",
        "  - claude-code\n",
    ));
    workspace.write_skill_source("git-source", "release-notes");
    workspace.init_git_repo("git-source");
    let repo_url = workspace.git_repo_url("git-source");
    let original_commit = workspace.git_head("git-source");

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
            repo_url.as_str(),
        ])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    workspace.write_skill_source_at(
        "git-source",
        ".agents/skills/release-notes",
        "release-notes",
        "Pinned release notes helper.",
    );
    workspace.commit_all("git-source", "pin release notes");
    let pinned_commit = workspace.git_head("git-source");

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .env("SOURCE_DATE_EPOCH", "1770001234")
        .args(["pin", "release-notes", pinned_commit.as_str()])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    let snapshot = workspace.snapshot_paths(&[
        ".agents/skillctl.yaml",
        ".agents/skillctl.lock",
        "home/.skillctl/state.db",
        "home/.skillctl/store/imports/release-notes",
        ".claude/skills",
    ]);

    assert_transaction_rolled_back(
        &workspace,
        "rollback:after-state",
        "1770002468",
        &["rollback", "release-notes", original_commit.as_str()],
        &snapshot,
    );
}

#[test]
fn fork_copies_the_effective_skill_into_local_ownership_and_detaches_updates() {
    let workspace = TestWorkspace::new();
    workspace.write_manifest(concat!(
        "version: 1\n",
        "\n",
        "targets:\n",
        "  - claude-code\n",
    ));
    workspace.write_skill_source("git-source", "release-notes");
    workspace.init_git_repo("git-source");
    let repo_url = workspace.git_repo_url("git-source");

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
            repo_url.as_str(),
        ])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .env("SOURCE_DATE_EPOCH", "1770001234")
        .args(["override", "release-notes"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    fs::write(
        workspace
            .path()
            .join(".agents/overlays/release-notes/SKILL.md"),
        concat!(
            "---\n",
            "name: release-notes\n",
            "description: Forked local release notes helper.\n",
            "---\n",
            "\n",
            "# release-notes\n"
        ),
    )
    .expect("overlay manifest updated");

    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .env("SOURCE_DATE_EPOCH", "1770002468")
        .args(["--json", "fork", "release-notes"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    let output = assert.get_output();
    let body: Value = serde_json::from_slice(&output.stdout).expect("stdout is valid json");
    assert_eq!(body["command"], "fork");
    assert_eq!(body["ok"], true);
    assert_eq!(body["data"]["skill"], "release-notes");
    assert_eq!(body["data"]["scope"], "workspace");
    assert_eq!(body["data"]["local_root"], ".agents/skills/release-notes");

    let local_manifest = workspace
        .path()
        .join(".agents/skills/release-notes/SKILL.md");
    assert!(
        local_manifest.is_file(),
        "local canonical skill should exist"
    );
    assert!(
        fs::read_to_string(&local_manifest)
            .expect("local manifest exists")
            .contains("Forked local release notes helper."),
        "fork should copy the effective overlay content into local ownership",
    );
    assert!(
        !workspace
            .path()
            .join(".agents/skills/release-notes/.skillctl-projection.json")
            .exists(),
        "canonical local skills must not retain generated projection metadata",
    );

    let manifest = fs::read_to_string(workspace.path().join(".agents/skillctl.yaml"))
        .expect("manifest exists");
    assert!(
        !manifest.contains("id: release-notes"),
        "forked skills should no longer remain active manifest imports: {manifest}",
    );
    assert!(
        !manifest.contains("release-notes: .agents/overlays/release-notes"),
        "forked skills should remove managed overlay wiring: {manifest}",
    );

    let lockfile = workspace.read_lockfile_yaml();
    assert!(
        lockfile["imports"]["release-notes"].is_null(),
        "forked skills should no longer remain in the active lockfile: {lockfile:?}",
    );

    let projection_metadata: Value = serde_json::from_str(
        &fs::read_to_string(
            workspace
                .path()
                .join(".claude/skills/release-notes/.skillctl-projection.json"),
        )
        .expect("projection metadata exists"),
    )
    .expect("projection metadata is valid json");
    assert_eq!(projection_metadata["source"]["kind"], "canonical-local");
    assert_eq!(
        projection_metadata["source"]["relative_path"],
        ".agents/skills/release-notes"
    );

    let connection = Connection::open(workspace.home_path().join(".skillctl/state.db"))
        .expect("state database opens");
    let install_row: (i64, i64, String) = connection
        .query_row(
            "SELECT detached, forked, updated_at FROM install_records WHERE skill_id = ?1",
            params!["release-notes"],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("install record exists");
    assert_eq!(install_row.0, 1);
    assert_eq!(install_row.1, 1);
    assert_eq!(install_row.2, "2026-02-02T03:21:08Z");

    let history_kinds: Vec<String> = connection
        .prepare(
            "SELECT kind FROM history_events WHERE skill_id = ?1 ORDER BY occurred_at ASC, id ASC",
        )
        .expect("statement prepares")
        .query_map(params!["release-notes"], |row| row.get(0))
        .expect("history query succeeds")
        .collect::<Result<_, _>>()
        .expect("history rows decode");
    assert_eq!(
        history_kinds,
        vec![
            "install".to_string(),
            "projection".to_string(),
            "overlay-created".to_string(),
            "fork".to_string(),
            "projection".to_string(),
        ]
    );

    let update_assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--json", "update", "release-notes"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    let update_body: Value =
        serde_json::from_slice(&update_assert.get_output().stdout).expect("stdout is valid json");
    let plan = &update_body["data"]["plans"][0];
    assert_eq!(plan["outcome"], "detached");
    assert_eq!(plan["recommended_action"], "skip");
    assert_eq!(plan["modifications"][0]["kind"], "detached-fork");
}

#[test]
fn fork_rolls_back_when_the_transaction_fails_after_state_write() {
    let workspace = TestWorkspace::new();
    workspace.write_manifest(concat!(
        "version: 1\n",
        "\n",
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
        .env("SOURCE_DATE_EPOCH", "1770001234")
        .args(["override", "release-notes"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    let snapshot = workspace.snapshot_paths(&[
        ".agents/skillctl.yaml",
        ".agents/skillctl.lock",
        ".agents/overlays/release-notes",
        ".agents/skills/release-notes",
        "home/.skillctl/state.db",
        ".claude/skills",
    ]);

    assert_transaction_rolled_back(
        &workspace,
        "fork:after-state",
        "1770002468",
        &["fork", "release-notes"],
        &snapshot,
    );
}

#[test]
fn list_and_path_report_managed_skill_inventory_and_filesystem_locations() {
    let workspace = TestWorkspace::new();
    workspace.write_manifest(concat!(
        "version: 1\n",
        "\n",
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
        .env("SOURCE_DATE_EPOCH", "1770001234")
        .args(["override", "release-notes"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    let list_assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--json", "list"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    let list_body: Value =
        serde_json::from_slice(&list_assert.get_output().stdout).expect("stdout is valid json");

    assert_eq!(list_body["command"], "list");
    assert_eq!(list_body["ok"], true);
    let release_notes = list_body["data"]["skills"]
        .as_array()
        .expect("skills array exists")
        .iter()
        .find(|skill| skill["skill"] == "release-notes" && skill["scope"] == "workspace")
        .expect("release-notes workspace skill exists");
    assert_eq!(release_notes["skill"], "release-notes");
    assert_eq!(release_notes["scope"], "workspace");
    assert_eq!(release_notes["managed_import"], true);
    assert_eq!(release_notes["managed_import_enabled"], true);
    assert_eq!(
        release_notes["overlay_root"],
        ".agents/overlays/release-notes"
    );
    assert_eq!(release_notes["source"]["type"], "local-path");
    assert_eq!(
        release_notes["projections"][0]["path"],
        ".claude/skills/release-notes"
    );

    let path_assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--json", "path", "release-notes"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    let path_body: Value =
        serde_json::from_slice(&path_assert.get_output().stdout).expect("stdout is valid json");

    assert_eq!(path_body["command"], "path");
    assert_eq!(path_body["ok"], true);
    assert_eq!(path_body["data"]["skill"], "release-notes");
    assert_eq!(path_body["data"]["scope"], "workspace");
    assert_eq!(
        path_body["data"]["overlay_root"],
        ".agents/overlays/release-notes"
    );
    assert!(
        path_body["data"]["stored_source_root"]
            .as_str()
            .expect("stored source root exists")
            .ends_with(".skillctl/store/imports/release-notes"),
    );
    assert!(
        path_body["data"]["active_source_root"]
            .as_str()
            .expect("active source root exists")
            .ends_with(".skillctl/store/imports/release-notes/.agents/skills/release-notes"),
    );
    assert_eq!(
        path_body["data"]["planned_roots"][0]["path"],
        ".claude/skills/release-notes"
    );
    assert_eq!(
        path_body["data"]["projections"][0]["path"],
        ".claude/skills/release-notes"
    );
}

#[test]
fn bundled_skill_is_installed_in_user_scope_on_first_command() {
    let workspace = TestWorkspace::new();
    let home_path = workspace.home_path();

    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", &home_path)
        .args(["--json", "telemetry", "status"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    let body: Value = serde_json::from_slice(&assert.get_output().stdout).expect("stdout is json");

    assert_eq!(body["command"], "telemetry-status");
    assert!(
        home_path.join(".agents/skills/skillctl/SKILL.md").is_file(),
        "bundled skill should project into the neutral user root",
    );
    assert!(
        home_path.join(".claude/skills/skillctl/SKILL.md").is_file(),
        "bundled skill should project into the claude-compatible user root",
    );
    assert!(
        home_path
            .join(".config/agents/skills/skillctl/SKILL.md")
            .is_file(),
        "bundled skill should project into the amp-compatible user root",
    );
    assert!(
        !home_path.join(".copilot/skills/skillctl").exists(),
        "planner should prefer the shared claude root over an extra copilot root",
    );

    let connection =
        Connection::open(home_path.join(".skillctl/state.db")).expect("state database opens");
    let install_row: (String, String, String, String) = connection
        .query_row(
            "SELECT scope, skill_id, source_url, resolved_revision FROM install_records \
             WHERE scope = ?1 AND skill_id = ?2",
            params!["user", "skillctl"],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("bundled install record exists");

    assert_eq!(install_row.0, "user");
    assert_eq!(install_row.1, "skillctl");
    assert_eq!(install_row.2, "builtin://skillctl");
    assert_eq!(install_row.3, env!("CARGO_PKG_VERSION"));
}

#[test]
fn bundled_skill_appears_in_user_scope_explain_doctor_and_tui() {
    let workspace = TestWorkspace::new();
    let home_path = workspace.home_path();

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", &home_path)
        .args(["--json", "telemetry", "status"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    let explain_assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", &home_path)
        .args(["--json", "--scope", "user", "explain", "skillctl"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    let explain_body: Value =
        serde_json::from_slice(&explain_assert.get_output().stdout).expect("stdout is json");

    assert_eq!(explain_body["command"], "explain");
    assert_eq!(explain_body["data"]["skill"], "skillctl");
    assert_eq!(explain_body["data"]["scope"], "user");
    assert_eq!(explain_body["data"]["status"], "selected");
    assert_eq!(explain_body["data"]["winner"]["scope"], "user");
    assert_eq!(explain_body["data"]["winner"]["source_class"], "bundled");
    assert_eq!(
        explain_body["data"]["drift"]["active_projection_matches_winner"],
        true
    );
    assert_eq!(
        explain_body["data"]["drift"]["active_differs_from_pinned_source"],
        false
    );
    let explain_targets = explain_body["data"]["targets"]
        .as_array()
        .expect("explain targets exist");
    assert_eq!(explain_targets.len(), 6);
    for target in [
        "codex",
        "claude-code",
        "github-copilot",
        "gemini-cli",
        "amp",
        "opencode",
    ] {
        assert!(
            explain_targets
                .iter()
                .any(|entry| entry["target"] == target && entry["visible"] == true),
            "expected bundled skill to be visible for {target}: {explain_body:#?}",
        );
    }

    let doctor_assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", &home_path)
        .args(["--json", "--scope", "user", "doctor"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    let doctor_body: Value =
        serde_json::from_slice(&doctor_assert.get_output().stdout).expect("stdout is json");

    assert_eq!(doctor_body["command"], "doctor");
    assert_eq!(
        doctor_body["data"]["summary"]["checked_skill_count"]
            .as_u64()
            .expect("checked skill count is numeric"),
        1,
    );
    assert!(
        doctor_body["data"]["issues"]
            .as_array()
            .expect("doctor issues are an array")
            .is_empty(),
        "unexpected doctor issues: {doctor_body:#?}",
    );

    let tui_assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", &home_path)
        .args(["--json", "--scope", "user", "--name", "skillctl", "tui"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    let tui_body: Value =
        serde_json::from_slice(&tui_assert.get_output().stdout).expect("stdout is json");

    assert_eq!(tui_body["command"], "tui");
    assert_eq!(tui_body["data"]["skills"][0]["skill"], "skillctl");
    assert_eq!(tui_body["data"]["skills"][0]["scope"], "user");
    assert_eq!(
        tui_body["data"]["skills"][0]["visibility"]["status"],
        "selected"
    );
    assert_eq!(
        tui_body["data"]["skills"][0]["visibility"]["winner"]["source_class"],
        "bundled"
    );
    assert_eq!(
        tui_body["data"]["skills"][0]["visibility"]["targets"]
            .as_array()
            .expect("tui targets exist")
            .len(),
        6,
    );
    assert_eq!(
        tui_body["data"]["skills"][0]["visibility"]["drift"]["active_projection_matches_winner"],
        true
    );
}

#[test]
fn bundled_skill_conflicts_are_reported_in_explain_doctor_and_tui() {
    let workspace = TestWorkspace::new();
    let home_path = workspace.home_path();
    let custom_skill_root = home_path.join(".claude/skills/skillctl");
    fs::create_dir_all(&custom_skill_root).expect("custom skill root exists");
    fs::write(
        custom_skill_root.join("SKILL.md"),
        concat!(
            "---\n",
            "name: skillctl\n",
            "description: User-managed skillctl variant.\n",
            "---\n",
            "\n",
            "# custom skillctl\n",
        ),
    )
    .expect("custom skill manifest exists");

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", &home_path)
        .args(["--json", "telemetry", "status"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    let explain_assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", &home_path)
        .args(["--json", "--scope", "user", "explain", "skillctl"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    let explain_body: Value =
        serde_json::from_slice(&explain_assert.get_output().stdout).expect("stdout is json");

    assert_eq!(explain_body["data"]["status"], "selected");
    assert_eq!(
        explain_body["data"]["drift"]["active_projection_matches_winner"],
        false
    );
    assert_eq!(
        explain_body["data"]["drift"]["active_differs_from_pinned_source"],
        true
    );

    let doctor_assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", &home_path)
        .args(["--json", "--scope", "user", "doctor"])
        .assert()
        .code(2)
        .stderr(predicate::str::is_empty());
    let doctor_body: Value =
        serde_json::from_slice(&doctor_assert.get_output().stdout).expect("stdout is json");

    assert!(
        doctor_body["data"]["issues"]
            .as_array()
            .expect("doctor issues are an array")
            .iter()
            .any(|issue| issue["code"] == "bundled-skill-conflict" && issue["skill"] == "skillctl"),
        "expected bundled conflict issue: {doctor_body:#?}",
    );
    assert!(
        doctor_body["data"]["issues"]
            .as_array()
            .expect("doctor issues are an array")
            .iter()
            .any(|issue| issue["code"] == "projection-drift" && issue["skill"] == "skillctl"),
        "expected bundled projection drift issue: {doctor_body:#?}",
    );

    let tui_assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", &home_path)
        .args(["--json", "--scope", "user", "--name", "skillctl", "tui"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    let tui_body: Value =
        serde_json::from_slice(&tui_assert.get_output().stdout).expect("stdout is json");

    assert_eq!(
        tui_body["data"]["skills"][0]["visibility"]["drift"]["active_projection_matches_winner"],
        false
    );
    assert_eq!(
        tui_body["data"]["skills"][0]["visibility"]["drift"]["active_differs_from_pinned_source"],
        true
    );
    assert!(
        tui_body["data"]["skills"][0]["visibility"]["issues"]
            .as_array()
            .expect("tui visibility issues are an array")
            .iter()
            .any(|issue| issue["code"] == "bundled-skill-conflict"),
        "expected bundled conflict issue in TUI visibility: {tui_body:#?}",
    );
}

#[test]
fn bundled_bootstrap_rolls_back_when_the_transaction_fails_after_state_write() {
    let workspace = TestWorkspace::new();

    let snapshot = workspace.snapshot_paths(&[
        "home/.skillctl/state.db",
        "home/.agents/skills",
        "home/.claude/skills",
        "home/.config/agents/skills",
    ]);

    assert_transaction_rolled_back(
        &workspace,
        "bundled-bootstrap:after-state",
        "1770000000",
        &["telemetry", "status"],
        &snapshot,
    );
}

#[test]
fn bundled_skill_removal_is_explicit_and_persists_across_later_runs() {
    let workspace = TestWorkspace::new();
    let home_path = workspace.home_path();

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", &home_path)
        .args(["--json", "telemetry", "status"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    let remove_assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", &home_path)
        .args(["--json", "--scope", "user", "remove", "skillctl"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    let remove_body: Value =
        serde_json::from_slice(&remove_assert.get_output().stdout).expect("stdout is json");

    assert_eq!(remove_body["command"], "remove");
    assert_eq!(remove_body["data"]["skill"], "skillctl");
    assert_eq!(remove_body["data"]["scope"], "user");
    assert!(
        !home_path.join(".agents/skills/skillctl").exists(),
        "explicit removal should prune bundled user projections",
    );
    assert!(
        !home_path.join(".claude/skills/skillctl").exists(),
        "explicit removal should prune bundled user projections",
    );
    assert!(
        !home_path.join(".config/agents/skills/skillctl").exists(),
        "explicit removal should prune bundled user projections",
    );

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", &home_path)
        .args(["--json", "telemetry", "status"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    assert!(
        !home_path.join(".agents/skills/skillctl").exists(),
        "explicit removal should suppress later automatic reinstall",
    );

    let history_assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", &home_path)
        .args(["--json", "--scope", "user", "history", "skillctl"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    let history_body: Value =
        serde_json::from_slice(&history_assert.get_output().stdout).expect("stdout is json");
    let latest_kind = history_body["data"]["entries"][0]["kind"]
        .as_str()
        .expect("history kind exists");
    let latest_reason = history_body["data"]["entries"][0]["details"]["reason"]
        .as_str()
        .expect("cleanup reason exists");

    assert_eq!(latest_kind, "cleanup");
    assert_eq!(latest_reason, "explicit-remove");
}

#[test]
fn bundled_remove_rolls_back_when_the_transaction_fails_after_state_write() {
    let workspace = TestWorkspace::new();
    let home_path = workspace.home_path();

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", &home_path)
        .args(["--json", "telemetry", "status"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    let snapshot = workspace.snapshot_paths(&[
        "home/.skillctl/state.db",
        "home/.agents/skills",
        "home/.claude/skills",
        "home/.config/agents/skills",
    ]);

    assert_transaction_rolled_back(
        &workspace,
        "bundled-remove:after-state",
        "1770001234",
        &["--scope", "user", "remove", "skillctl"],
        &snapshot,
    );
}

#[test]
fn bundled_skill_does_not_overwrite_hand_authored_user_skill_roots() {
    let workspace = TestWorkspace::new();
    let home_path = workspace.home_path();
    let custom_skill_root = home_path.join(".claude/skills/skillctl");
    fs::create_dir_all(&custom_skill_root).expect("custom skill root exists");
    fs::write(
        custom_skill_root.join("SKILL.md"),
        concat!(
            "---\n",
            "name: skillctl\n",
            "description: User-managed skillctl variant.\n",
            "---\n",
            "\n",
            "# custom skillctl\n",
        ),
    )
    .expect("custom skill manifest exists");

    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", &home_path)
        .args(["--json", "doctor"])
        .assert()
        .code(2)
        .stderr(predicate::str::is_empty());
    let body: Value = serde_json::from_slice(&assert.get_output().stdout).expect("stdout is json");

    assert!(
        home_path.join(".agents/skills/skillctl/SKILL.md").is_file(),
        "bundled skill should still install into other compatible roots",
    );
    assert_eq!(
        fs::read_to_string(custom_skill_root.join("SKILL.md")).expect("custom skill remains"),
        concat!(
            "---\n",
            "name: skillctl\n",
            "description: User-managed skillctl variant.\n",
            "---\n",
            "\n",
            "# custom skillctl\n",
        ),
    );
    assert!(
        body["data"]["issues"]
            .as_array()
            .expect("doctor issues is an array")
            .iter()
            .any(|issue| issue["code"] == "bundled-skill-conflict"),
        "doctor should surface the blocked bundled-skill projection",
    );
}

#[test]
fn bundled_enable_rolls_back_when_the_transaction_fails_after_state_write() {
    let workspace = TestWorkspace::new();
    let home_path = workspace.home_path();

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", &home_path)
        .args(["--json", "telemetry", "status"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", &home_path)
        .args(["--json", "--scope", "user", "remove", "skillctl"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    let snapshot = workspace.snapshot_paths(&[
        "home/.skillctl/state.db",
        "home/.agents/skills",
        "home/.claude/skills",
        "home/.config/agents/skills",
    ]);

    assert_transaction_rolled_back(
        &workspace,
        "bundled-enable:after-state",
        "1770002468",
        &["--scope", "user", "enable", "skillctl"],
        &snapshot,
    );
}

#[test]
fn disable_and_enable_toggle_manifest_state_and_projections() {
    let workspace = TestWorkspace::new();
    workspace.write_manifest(concat!(
        "version: 1\n",
        "\n",
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

    let disable_assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .env("SOURCE_DATE_EPOCH", "1770001234")
        .args(["--json", "disable", "release-notes"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    let disable_body: Value =
        serde_json::from_slice(&disable_assert.get_output().stdout).expect("stdout is valid json");

    assert_eq!(disable_body["command"], "disable");
    assert_eq!(disable_body["ok"], true);
    assert_eq!(disable_body["data"]["skill"], "release-notes");
    assert_eq!(disable_body["data"]["scope"], "workspace");
    assert_eq!(disable_body["data"]["enabled"], false);
    assert!(
        fs::read_to_string(workspace.path().join(".agents/skillctl.yaml"))
            .expect("manifest exists")
            .contains("enabled: false"),
    );
    assert!(
        !workspace
            .path()
            .join(".claude/skills/release-notes")
            .exists(),
        "disabled skill should be pruned from generated projections",
    );

    let connection = Connection::open(workspace.home_path().join(".skillctl/state.db"))
        .expect("state database opens");
    let disabled_projection_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM projection_records WHERE skill_id = ?1",
            params!["release-notes"],
            |row| row.get(0),
        )
        .expect("projection count query succeeds");
    assert_eq!(disabled_projection_count, 0);

    let enable_assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .env("SOURCE_DATE_EPOCH", "1770002468")
        .args(["--json", "enable", "release-notes"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    let enable_body: Value =
        serde_json::from_slice(&enable_assert.get_output().stdout).expect("stdout is valid json");

    assert_eq!(enable_body["command"], "enable");
    assert_eq!(enable_body["ok"], true);
    assert_eq!(enable_body["data"]["enabled"], true);
    assert!(
        fs::read_to_string(workspace.path().join(".agents/skillctl.yaml"))
            .expect("manifest exists")
            .contains("enabled: true"),
    );
    assert!(
        workspace
            .path()
            .join(".claude/skills/release-notes/SKILL.md")
            .is_file(),
        "enabled skill should be rematerialized",
    );

    let enabled_projection_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM projection_records WHERE skill_id = ?1",
            params!["release-notes"],
            |row| row.get(0),
        )
        .expect("projection count query succeeds");
    assert_eq!(enabled_projection_count, 1);
}

#[test]
fn disable_rolls_back_when_the_transaction_fails_after_state_write() {
    let workspace = TestWorkspace::new();
    workspace.write_manifest(concat!(
        "version: 1\n",
        "\n",
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

    let snapshot = workspace.snapshot_paths(&[
        ".agents/skillctl.yaml",
        "home/.skillctl/state.db",
        ".claude/skills",
    ]);

    assert_transaction_rolled_back(
        &workspace,
        "disable:after-state",
        "1770001234",
        &["disable", "release-notes"],
        &snapshot,
    );
}

#[test]
fn enable_rolls_back_when_the_transaction_fails_after_state_write() {
    let workspace = TestWorkspace::new();
    workspace.write_manifest(concat!(
        "version: 1\n",
        "\n",
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
        .env("SOURCE_DATE_EPOCH", "1770001234")
        .args(["disable", "release-notes"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    let snapshot = workspace.snapshot_paths(&[
        ".agents/skillctl.yaml",
        "home/.skillctl/state.db",
        ".claude/skills",
    ]);

    assert_transaction_rolled_back(
        &workspace,
        "enable:after-state",
        "1770002468",
        &["enable", "release-notes"],
        &snapshot,
    );
}

#[test]
fn sync_rolls_back_when_the_transaction_fails_after_state_write() {
    let workspace = TestWorkspace::new();
    workspace.write_manifest(concat!(
        "version: 1\n",
        "\n",
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

    let snapshot = workspace.snapshot_paths(&["home/.skillctl/state.db", ".claude/skills"]);

    assert_transaction_rolled_back(
        &workspace,
        "sync:after-state",
        "1770001234",
        &["sync"],
        &snapshot,
    );
}

#[test]
fn remove_drops_current_install_state_but_history_remains_queryable() {
    let workspace = TestWorkspace::new();
    workspace.write_manifest(concat!(
        "version: 1\n",
        "\n",
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
        .env("SOURCE_DATE_EPOCH", "1770001234")
        .args(["override", "release-notes"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    let remove_assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .env("SOURCE_DATE_EPOCH", "1770002468")
        .args(["--json", "remove", "release-notes"])
        .assert()
        .code(2)
        .stderr(predicate::str::is_empty());
    let remove_body: Value =
        serde_json::from_slice(&remove_assert.get_output().stdout).expect("stdout is valid json");

    assert_eq!(remove_body["command"], "remove");
    assert_eq!(remove_body["ok"], true);
    assert_eq!(remove_body["data"]["skill"], "release-notes");
    assert!(
        remove_body["warnings"][0]
            .as_str()
            .expect("warning exists")
            .contains(".agents/overlays/release-notes"),
    );

    let manifest = fs::read_to_string(workspace.path().join(".agents/skillctl.yaml"))
        .expect("manifest exists");
    assert!(
        !manifest.contains("id: release-notes"),
        "removed skill should not remain in the manifest: {manifest}",
    );
    assert!(
        !manifest.contains("release-notes: .agents/overlays/release-notes"),
        "removed skill should no longer retain active override wiring: {manifest}",
    );

    let lockfile = workspace.read_lockfile_yaml();
    assert!(lockfile["imports"]["release-notes"].is_null());
    assert!(
        !workspace
            .home_path()
            .join(".skillctl/store/imports/release-notes")
            .exists(),
        "stored immutable source should be removed",
    );
    assert!(
        workspace
            .path()
            .join(".agents/overlays/release-notes")
            .is_dir(),
        "overlay edits should be preserved for manual reuse",
    );
    assert!(
        !workspace
            .path()
            .join(".claude/skills/release-notes")
            .exists(),
        "runtime projection should be removed",
    );

    let connection = Connection::open(workspace.home_path().join(".skillctl/state.db"))
        .expect("state database opens");
    let install_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM install_records WHERE skill_id = ?1",
            params!["release-notes"],
            |row| row.get(0),
        )
        .expect("install count query succeeds");
    assert_eq!(install_count, 0);
    let pin_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM pins WHERE skill_id = ?1",
            params!["release-notes"],
            |row| row.get(0),
        )
        .expect("pin count query succeeds");
    assert_eq!(pin_count, 0);
    let projection_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM projection_records WHERE skill_id = ?1",
            params!["release-notes"],
            |row| row.get(0),
        )
        .expect("projection count query succeeds");
    assert_eq!(projection_count, 0);

    let history_assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--json", "history", "release-notes"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    let history_body: Value =
        serde_json::from_slice(&history_assert.get_output().stdout).expect("stdout is valid json");
    let history_kinds: Vec<&str> = history_body["data"]["entries"]
        .as_array()
        .expect("entries exist")
        .iter()
        .filter_map(|entry| entry["kind"].as_str())
        .collect();
    assert!(
        history_kinds.contains(&"install"),
        "install history should remain queryable: {history_kinds:?}",
    );
    assert!(
        history_kinds.contains(&"overlay-created"),
        "overlay history should remain queryable: {history_kinds:?}",
    );
    assert!(
        history_kinds.contains(&"cleanup"),
        "remove should append a cleanup event: {history_kinds:?}",
    );
}

#[test]
fn remove_rolls_back_when_the_transaction_fails_after_state_write() {
    let workspace = TestWorkspace::new();
    workspace.write_manifest(concat!(
        "version: 1\n",
        "\n",
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
        .env("SOURCE_DATE_EPOCH", "1770001234")
        .args(["override", "release-notes"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    let snapshot = workspace.snapshot_paths(&[
        ".agents/skillctl.yaml",
        ".agents/skillctl.lock",
        ".agents/overlays/release-notes",
        "home/.skillctl/state.db",
        "home/.skillctl/store/imports/release-notes",
        ".claude/skills",
    ]);

    assert_transaction_rolled_back(
        &workspace,
        "remove:after-state",
        "1770002468",
        &["remove", "release-notes"],
        &snapshot,
    );
}

#[test]
fn clean_removes_generated_projections_and_unused_import_state_without_touching_canonical_skills() {
    let workspace = TestWorkspace::new();
    workspace.write_manifest(concat!(
        "version: 1\n",
        "\n",
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

    workspace.write_workspace_skill("local-only", "Local canonical helper.", &[]);
    workspace.write_file(
        "home/.skillctl/store/imports/stale-skill/SKILL.md",
        concat!(
            "---\n",
            "name: stale-skill\n",
            "description: Stale immutable source.\n",
            "---\n",
            "\n",
            "# stale-skill\n"
        ),
    );

    let clean_assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .env("SOURCE_DATE_EPOCH", "1770001234")
        .args(["--json", "clean"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    let clean_body: Value =
        serde_json::from_slice(&clean_assert.get_output().stdout).expect("stdout is valid json");

    assert_eq!(clean_body["command"], "clean");
    assert_eq!(clean_body["ok"], true);
    assert!(
        clean_body["data"]["cleaned_projections"]
            .as_array()
            .expect("cleaned projections exist")
            .iter()
            .any(|entry| entry["path"] == ".claude/skills/release-notes"),
    );
    assert!(
        clean_body["data"]["cleaned_state"]
            .as_array()
            .expect("cleaned state exists")
            .iter()
            .any(|entry| {
                entry["path"]
                    .as_str()
                    .expect("path exists")
                    .ends_with(".skillctl/store/imports/stale-skill")
            }),
    );

    assert!(
        !workspace
            .path()
            .join(".claude/skills/release-notes")
            .exists(),
        "generated runtime projection should be removed",
    );
    assert!(
        workspace.path().join(".agents/skills/local-only").is_dir(),
        "canonical local skills must remain untouched",
    );
    assert!(
        workspace
            .home_path()
            .join(".skillctl/store/imports/release-notes")
            .is_dir(),
        "active immutable imports should not be deleted by clean",
    );
    assert!(
        !workspace
            .home_path()
            .join(".skillctl/store/imports/stale-skill")
            .exists(),
        "unused import state should be removed",
    );

    let connection = Connection::open(workspace.home_path().join(".skillctl/state.db"))
        .expect("state database opens");
    let install_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM install_records WHERE scope = ?1 AND skill_id = ?2",
            params!["workspace", "release-notes"],
            |row| row.get(0),
        )
        .expect("install count query succeeds");
    assert_eq!(install_count, 1);
    let projection_count: i64 = connection
        .query_row("SELECT COUNT(*) FROM projection_records", [], |row| {
            row.get(0)
        })
        .expect("projection count query succeeds");
    assert_eq!(projection_count, 0);
}

#[test]
fn clean_rolls_back_when_the_transaction_fails_after_state_write() {
    let workspace = TestWorkspace::new();
    workspace.write_manifest(concat!(
        "version: 1\n",
        "\n",
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

    workspace.write_file(
        "home/.skillctl/store/imports/stale-skill/SKILL.md",
        concat!(
            "---\n",
            "name: stale-skill\n",
            "description: Stale immutable source.\n",
            "---\n",
            "\n",
            "# stale-skill\n"
        ),
    );

    let snapshot = workspace.snapshot_paths(&[
        "home/.skillctl/state.db",
        "home/.skillctl/store/imports",
        ".claude/skills",
    ]);

    assert_transaction_rolled_back(
        &workspace,
        "clean:after-state",
        "1770001234",
        &["clean"],
        &snapshot,
    );
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum PathSnapshot {
    Missing,
    File(Vec<u8>),
    Directory(BTreeMap<String, PathSnapshot>),
    Symlink(PathBuf),
}

fn initialize_runtime_state(workspace: &TestWorkspace) {
    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--json", "telemetry", "status"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
}

fn assert_transaction_rolled_back(
    workspace: &TestWorkspace,
    failpoint: &str,
    source_date_epoch: &str,
    args: &[&str],
    expected: &BTreeMap<String, PathSnapshot>,
) {
    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .env("SOURCE_DATE_EPOCH", source_date_epoch)
        .env("SKILLCTL_FAILPOINT", failpoint)
        .args(args)
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("injected lifecycle failure"));

    let actual = workspace.snapshot_paths(&expected.keys().map(String::as_str).collect::<Vec<_>>());
    assert_eq!(&actual, expected);
}

fn snapshot_path(path: &Path) -> PathSnapshot {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return PathSnapshot::Missing;
        }
        Err(error) => panic!("failed to inspect '{}': {error}", path.display()),
    };

    if metadata.file_type().is_symlink() {
        return PathSnapshot::Symlink(fs::read_link(path).unwrap_or_else(|error| {
            panic!("failed to read symlink '{}': {error}", path.display())
        }));
    }

    if metadata.is_file() {
        return PathSnapshot::File(
            fs::read(path)
                .unwrap_or_else(|error| panic!("failed to read '{}': {error}", path.display())),
        );
    }

    if metadata.is_dir() {
        let mut children = BTreeMap::new();
        let mut entries = fs::read_dir(path)
            .unwrap_or_else(|error| {
                panic!("failed to read directory '{}': {error}", path.display())
            })
            .collect::<Result<Vec<_>, _>>()
            .unwrap_or_else(|error| {
                panic!(
                    "failed to read directory entry '{}': {error}",
                    path.display()
                )
            });
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let name = entry.file_name().to_string_lossy().into_owned();
            children.insert(name, snapshot_path(&entry.path()));
        }
        return PathSnapshot::Directory(children);
    }

    panic!("unsupported filesystem entry '{}'", path.display());
}

struct TestWorkspace {
    path: PathBuf,
}

impl TestWorkspace {
    fn new() -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time moved backwards")
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("skillctl-test-{}-{}", std::process::id(), unique));
        fs::create_dir_all(&path).expect("workspace exists");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn home_path(&self) -> PathBuf {
        let path = self.path.join("home");
        fs::create_dir_all(&path).expect("home directory exists");
        path
    }

    fn write_manifest(&self, contents: &str) {
        self.write_file(".agents/skillctl.yaml", contents);
    }

    fn write_lockfile(&self, contents: &str) {
        self.write_file(".agents/skillctl.lock", contents);
    }

    fn write_workspace_skill(
        &self,
        skill_name: &str,
        description: &str,
        extra_files: &[(&str, &str)],
    ) {
        let skill_root = self.path.join(".agents/skills").join(skill_name);
        fs::create_dir_all(&skill_root).expect("skill source root exists");
        fs::write(
            skill_root.join("SKILL.md"),
            format!(
                concat!(
                    "---\n",
                    "name: {skill_name}\n",
                    "description: {description}\n",
                    "---\n",
                    "\n",
                    "# {skill_name}\n"
                ),
                skill_name = skill_name,
                description = description,
            ),
        )
        .expect("skill manifest exists");

        for (relative_path, contents) in extra_files {
            let path = skill_root.join(relative_path);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("extra file parent exists");
            }
            fs::write(path, contents).expect("extra file written");
        }
    }

    fn write_file(&self, relative_path: &str, contents: &str) {
        let path = self.path.join(relative_path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("parent directory exists");
        }
        fs::write(path, contents).expect("file written");
    }

    fn write_skill_source(&self, source_dir: &str, skill_name: &str) {
        self.write_skill_source_at(
            source_dir,
            &format!(".agents/skills/{skill_name}"),
            skill_name,
            "Summarize release notes.",
        );
    }

    fn write_skill_source_at(
        &self,
        source_dir: &str,
        relative_skill_root: &str,
        skill_name: &str,
        description: &str,
    ) {
        let skill_root = self.path.join(source_dir).join(relative_skill_root);
        fs::create_dir_all(&skill_root).expect("skill source root exists");
        fs::write(
            skill_root.join("SKILL.md"),
            format!(
                concat!(
                    "---\n",
                    "name: {skill_name}\n",
                    "description: {description}\n",
                    "---\n",
                    "\n",
                    "# {skill_name}\n"
                ),
                skill_name = skill_name,
                description = description,
            ),
        )
        .expect("skill manifest exists");
    }

    fn read_lockfile_yaml(&self) -> YamlValue {
        serde_yaml::from_str(
            &fs::read_to_string(self.path().join(".agents/skillctl.lock"))
                .expect("lockfile exists"),
        )
        .expect("lockfile is valid yaml")
    }

    fn snapshot_paths(&self, relative_paths: &[&str]) -> BTreeMap<String, PathSnapshot> {
        relative_paths
            .iter()
            .map(|relative_path| {
                (
                    (*relative_path).to_string(),
                    snapshot_path(&self.path.join(relative_path)),
                )
            })
            .collect()
    }

    fn git_repo_url(&self, relative_path: &str) -> String {
        let path = fs::canonicalize(self.path.join(relative_path)).expect("repo path exists");
        format!("file://{}", path.display())
    }

    fn init_git_repo(&self, relative_path: &str) {
        let repo_path = self.path.join(relative_path);
        self.run_git(&repo_path, &["init", "--initial-branch", "main"]);
        self.run_git(&repo_path, &["config", "user.name", "Skillctl Tests"]);
        self.run_git(
            &repo_path,
            &["config", "user.email", "skillctl-tests@example.com"],
        );
        self.run_git(&repo_path, &["add", "."]);
        self.run_git(
            &repo_path,
            &[
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--quiet",
                "-m",
                "initial import",
            ],
        );
    }

    fn commit_all(&self, relative_path: &str, message: &str) {
        let repo_path = self.path.join(relative_path);
        self.run_git(&repo_path, &["add", "."]);
        self.run_git(
            &repo_path,
            &[
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--quiet",
                "-m",
                message,
            ],
        );
    }

    fn git_head(&self, relative_path: &str) -> String {
        self.git_stdout(relative_path, &["rev-parse", "HEAD"])
    }

    fn run_git(&self, repo_path: &Path, args: &[&str]) {
        let status = ProcessCommand::new("git")
            .current_dir(repo_path)
            .env("GIT_AUTHOR_NAME", "Skillctl Tests")
            .env("GIT_AUTHOR_EMAIL", "skillctl-tests@example.com")
            .env("GIT_COMMITTER_NAME", "Skillctl Tests")
            .env("GIT_COMMITTER_EMAIL", "skillctl-tests@example.com")
            .args(args)
            .status()
            .expect("git command launches");
        assert!(
            status.success(),
            "git {:?} failed in {}",
            args,
            repo_path.display()
        );
    }

    fn git_stdout(&self, relative_path: &str, args: &[&str]) -> String {
        let repo_path = self.path.join(relative_path);
        let output = ProcessCommand::new("git")
            .current_dir(&repo_path)
            .env("GIT_AUTHOR_NAME", "Skillctl Tests")
            .env("GIT_AUTHOR_EMAIL", "skillctl-tests@example.com")
            .env("GIT_COMMITTER_NAME", "Skillctl Tests")
            .env("GIT_COMMITTER_EMAIL", "skillctl-tests@example.com")
            .args(args)
            .output()
            .expect("git command launches");
        assert!(
            output.status.success(),
            "git {:?} failed in {}",
            args,
            repo_path.display()
        );
        String::from_utf8(output.stdout)
            .expect("git stdout is valid utf-8")
            .trim()
            .to_string()
    }
}

impl Drop for TestWorkspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}
