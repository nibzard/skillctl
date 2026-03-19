use super::*;

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
fn tui_does_not_bootstrap_bundled_skill_on_a_fresh_home() {
    let workspace = TestWorkspace::new();
    let home_path = workspace.home_path();

    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", &home_path)
        .args(["--json", "tui"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    let body: Value = serde_json::from_slice(&assert.get_output().stdout).expect("stdout is json");

    assert_eq!(body["command"], "tui");
    assert_eq!(body["ok"], true);
    assert!(
        body["data"]["skills"]
            .as_array()
            .expect("skills array exists")
            .is_empty(),
        "unexpected TUI payload: {body:#?}",
    );
    assert!(
        !home_path.join(".agents/skills/skillctl/SKILL.md").exists(),
        "tui should not bootstrap the bundled skill into the neutral user root",
    );
    assert!(
        !home_path.join(".claude/skills/skillctl/SKILL.md").exists(),
        "tui should not bootstrap the bundled skill into the claude-compatible user root",
    );
    assert!(
        !home_path
            .join(".config/agents/skills/skillctl/SKILL.md")
            .exists(),
        "tui should not bootstrap the bundled skill into the amp-compatible user root",
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
