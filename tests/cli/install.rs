use super::*;

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
            .workspace_import_root("release-notes")
            .join(".agents/skills/release-notes/SKILL.md")
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
fn install_accepts_a_direct_skill_directory_source() {
    let workspace = TestWorkspace::new();
    workspace.write_manifest(concat!(
        "version: 1\n",
        "\n",
        "targets:\n",
        "  - claude-code\n",
    ));
    workspace.write_skill_source_at(
        "shared-skills",
        "release-notes",
        "release-notes",
        "Summarize release notes.",
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
            "shared-skills/release-notes",
        ])
        .assert()
        .success()
        .stderr(predicate::str::is_empty())
        .stdout(predicate::str::contains("Installed 1 skill"));

    let manifest = fs::read_to_string(workspace.path().join(".agents/skillctl.yaml"))
        .expect("manifest exists");
    assert!(
        manifest.contains("id: release-notes"),
        "manifest was {manifest}"
    );
    assert!(
        manifest.contains("path: skills/release-notes"),
        "manifest should store the packaged single-skill subpath: {manifest}",
    );

    let lockfile = fs::read_to_string(workspace.path().join(".agents/skillctl.lock"))
        .expect("lockfile exists");
    assert!(
        lockfile.contains("subpath: skills/release-notes"),
        "lockfile should store the packaged single-skill subpath: {lockfile}",
    );

    assert!(
        workspace
            .workspace_import_root("release-notes")
            .join("skills/release-notes/SKILL.md")
            .is_file(),
        "stored import checkout should contain the repackaged direct skill",
    );
    assert!(
        workspace
            .path()
            .join(".claude/skills/release-notes/.skillctl-projection.json")
            .is_file(),
        "generated projection metadata should exist",
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
        "home/.skillctl/store/imports",
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

    let connection = Connection::open(workspace.home_path().join(".skillctl/state.db"))
        .expect("state database opens");
    let install_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM install_records WHERE scope = ?1 AND skill_id = ?2",
            params!["workspace", "release-notes"],
            |row| row.get(0),
        )
        .expect("install count query succeeds");
    let projection_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM projection_records WHERE scope = ?1 AND skill_id = ?2",
            params!["workspace", "release-notes"],
            |row| row.get(0),
        )
        .expect("projection count query succeeds");
    let history_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM history_events WHERE scope = ?1 AND skill_id = ?2",
            params!["workspace", "release-notes"],
            |row| row.get(0),
        )
        .expect("history count query succeeds");
    assert_eq!(install_count, 0);
    assert_eq!(projection_count, 0);
    assert_eq!(history_count, 0);
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
