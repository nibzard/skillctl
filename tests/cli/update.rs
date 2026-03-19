use super::*;

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
fn update_uses_the_active_pinned_branch_instead_of_remote_head() {
    let workspace = TestWorkspace::new();
    workspace.write_skill_source("git-source", "release-notes");
    workspace.init_git_repo("git-source");
    let repo_url = workspace.git_repo_url("git-source");
    let repo_path = workspace.path().join("git-source");

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

    workspace.run_git(&repo_path, &["checkout", "--quiet", "-b", "feature"]);
    workspace.write_skill_source_at(
        "git-source",
        ".agents/skills/release-notes",
        "release-notes",
        "Feature branch release notes helper.",
    );
    workspace.commit_all("git-source", "create feature branch version");
    let pinned_feature_commit = workspace.git_head("git-source");

    let pin_assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--json", "pin", "release-notes", "feature"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    let pin_body: Value =
        serde_json::from_slice(&pin_assert.get_output().stdout).expect("stdout is valid json");
    assert_eq!(pin_body["data"]["requested_reference"], "feature");
    assert_eq!(pin_body["data"]["resolved_revision"], pinned_feature_commit);

    workspace.write_skill_source_at(
        "git-source",
        ".agents/skills/release-notes",
        "release-notes",
        "Updated feature branch release notes helper.",
    );
    workspace.commit_all("git-source", "update feature release notes");
    let latest_feature_commit = workspace.git_head("git-source");

    workspace.run_git(&repo_path, &["checkout", "--quiet", "main"]);
    workspace.write_skill_source_at(
        "git-source",
        ".agents/skills/release-notes",
        "release-notes",
        "Updated main branch release notes helper.",
    );
    workspace.commit_all("git-source", "update main release notes");
    let latest_main_commit = workspace.git_head("git-source");
    assert_ne!(latest_feature_commit, latest_main_commit);

    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--json", "update", "release-notes"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    let body: Value = serde_json::from_slice(&assert.get_output().stdout).expect("stdout is json");
    let plan = &body["data"]["plans"][0];
    assert_eq!(body["command"], "update");
    assert_eq!(body["ok"], true);
    assert_eq!(plan["outcome"], "update-available");
    assert_eq!(plan["pinned_revision"], pinned_feature_commit);
    assert_eq!(plan["latest_revision"], latest_feature_commit);
    assert_ne!(plan["latest_revision"], latest_main_commit);

    let connection = Connection::open(workspace.home_path().join(".skillctl/state.db"))
        .expect("state database opens");
    let pin_row: (String, String) = connection
        .query_row(
            "SELECT requested_reference, resolved_revision FROM pins WHERE skill_id = ?1",
            params!["release-notes"],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("pin record exists");
    assert_eq!(pin_row.0, "feature");
    assert_eq!(pin_row.1, pinned_feature_commit);

    let update_row: (String, String) = connection
        .query_row(
            "SELECT outcome, latest_revision \
             FROM update_checks WHERE skill_id = ?1 ORDER BY checked_at DESC, id DESC LIMIT 1",
            params!["release-notes"],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("update check exists");
    assert_eq!(update_row.0, "update-available");
    assert_eq!(update_row.1, latest_feature_commit);
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
fn pin_refreshes_projection_state_for_untouched_skills_when_targets_move() {
    let workspace = TestWorkspace::new();
    workspace.write_manifest(concat!(
        "version: 1\n",
        "\n",
        "targets:\n",
        "  - claude-code\n",
    ));
    workspace.write_skill_source("git-release", "release-notes");
    workspace.write_skill_source("git-bugs", "bug-triage");
    workspace.init_git_repo("git-release");
    workspace.init_git_repo("git-bugs");
    let release_repo_url = workspace.git_repo_url("git-release");
    let bug_repo_url = workspace.git_repo_url("git-bugs");

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
            release_repo_url.as_str(),
        ])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .env("SOURCE_DATE_EPOCH", "1770000001")
        .args([
            "--no-input",
            "--name",
            "bug-triage",
            "install",
            bug_repo_url.as_str(),
        ])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    let manifest_path = workspace.path().join(".agents/skillctl.yaml");
    let manifest = fs::read_to_string(&manifest_path).expect("manifest exists");
    fs::write(
        &manifest_path,
        manifest.replace("  - claude-code\n", "  - codex\n"),
    )
    .expect("manifest target updates");

    workspace.write_skill_source_at(
        "git-release",
        ".agents/skills/release-notes",
        "release-notes",
        "Pinned release notes helper.",
    );
    workspace.commit_all("git-release", "pin release notes");
    let pinned_commit = workspace.git_head("git-release");

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .env("SOURCE_DATE_EPOCH", "1770001234")
        .args(["pin", "release-notes", pinned_commit.as_str()])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    assert!(
        workspace
            .path()
            .join(".agents/skills/bug-triage/SKILL.md")
            .is_file(),
        "pin should re-project untouched skills into the new target root",
    );

    let path_assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--json", "path", "bug-triage"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    let path_body: Value =
        serde_json::from_slice(&path_assert.get_output().stdout).expect("stdout is valid json");
    assert_eq!(path_body["data"]["projections"][0]["target"], "codex");
    assert_eq!(
        path_body["data"]["projections"][0]["root"],
        ".agents/skills"
    );

    let connection = Connection::open(workspace.home_path().join(".skillctl/state.db"))
        .expect("state database opens");
    let projection_row: (String, String, String) = connection
        .query_row(
            "SELECT target, physical_root, projected_path \
             FROM projection_records WHERE skill_id = ?1",
            params!["bug-triage"],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("projection record exists");
    assert_eq!(
        projection_row,
        (
            "codex".to_string(),
            ".agents/skills".to_string(),
            "bug-triage".to_string(),
        )
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
        "home/.skillctl/store/imports",
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
