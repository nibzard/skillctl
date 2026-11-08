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
fn update_reports_a_deleted_projection_as_missing_and_recommends_sync() {
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

    fs::remove_dir_all(workspace.path().join(".agents/skills/release-notes"))
        .expect("generated projection deleted");

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

    assert_eq!(body["ok"], true, "absence is not an error: {body:#?}");
    assert_eq!(plan["outcome"], "update-available");
    assert_eq!(plan["recommended_action"], "apply");
    assert_eq!(plan["local_modification_detected"], false);
    assert_eq!(plan["modifications"], json!([]));
    assert_eq!(
        plan["missing_projections"],
        json!([{
            "targets": ["codex", "gemini-cli", "opencode"],
            "root": ".agents/skills",
            "path": ".agents/skills/release-notes",
        }]),
        "unexpected plan: {body:#?}",
    );
    let notes = plan["notes"]
        .as_array()
        .expect("notes array exists")
        .iter()
        .filter_map(|note| note.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        notes.contains("skillctl sync"),
        "the plan must recommend sync: {body:#?}",
    );
    assert!(
        body["warnings"]
            .as_array()
            .expect("warnings array exists")
            .iter()
            .any(|warning| warning
                .as_str()
                .expect("warning is a string")
                .contains("skillctl sync")),
        "the run must warn about the missing projection: {body:#?}",
    );

    let explain_assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--json", "explain", "release-notes"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    let explain_body: Value = serde_json::from_slice(&explain_assert.get_output().stdout)
        .expect("explain stdout is valid json");
    let explain_targets: Vec<&Value> = explain_body["data"]["targets"]
        .as_array()
        .expect("explain targets exist")
        .iter()
        .filter(|target| target["path"] == json!(".agents/skills/release-notes"))
        .collect();
    assert_eq!(
        explain_targets.len(),
        3,
        "unexpected explain targets: {explain_body:#?}",
    );
    for target in &explain_targets {
        assert_eq!(
            target["observed_presence"], "missing",
            "explain must observe the same absence: {target:#?}",
        );
    }

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
    assert_eq!(update_row, ("update-available".to_string(), 0));
    let recorded_modifications: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM local_modifications WHERE skill_id = ?1",
            params!["release-notes"],
            |row| row.get(0),
        )
        .expect("modification query succeeds");
    assert_eq!(
        recorded_modifications, 0,
        "a deleted projection is not a local modification",
    );

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["sync"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    let synced = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--json", "update", "release-notes"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    let synced_body: Value =
        serde_json::from_slice(&synced.get_output().stdout).expect("stdout is valid json");
    let synced_plan = &synced_body["data"]["plans"][0];
    assert_eq!(synced_plan["outcome"], "update-available");
    assert_eq!(
        synced_plan.get("missing_projections"),
        None,
        "a materialized projection is no longer missing: {synced_body:#?}",
    );
    assert_eq!(synced_body["warnings"], json!([]));
}

/// Rewrite the manifest target set in place, leaving every import alone.
fn retarget_manifest(workspace: &TestWorkspace, targets: &str) {
    let manifest_path = workspace.path().join(".agents/skillctl.yaml");
    let manifest = fs::read_to_string(&manifest_path).expect("manifest exists");
    let previous = concat!(
        "targets:\n",
        "  - codex\n",
        "  - gemini-cli\n",
        "  - opencode\n",
    );
    assert!(
        manifest.contains(previous),
        "unexpected manifest before retargeting: {manifest}",
    );
    fs::write(
        &manifest_path,
        manifest.replace(previous, &format!("targets:\n  - {targets}\n")),
    )
    .expect("manifest target set updates");
}

#[test]
fn update_names_a_planned_root_no_projection_record_covers() {
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

    // The target set moves to Claude Code without a sync, so the manifest plans
    // a root no projection record covers yet.
    retarget_manifest(&workspace, "claude-code");

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

    assert_eq!(body["ok"], true, "absence is not an error: {body:#?}");
    assert_eq!(plan["outcome"], "up-to-date");
    assert_eq!(plan["local_modification_detected"], false);
    assert_eq!(plan["modifications"], json!([]));
    assert_eq!(
        plan["missing_projections"],
        json!([{
            "targets": ["claude-code"],
            "root": ".claude/skills",
            "path": ".claude/skills/release-notes",
        }]),
        "unexpected plan: {body:#?}",
    );
    let notes = plan["notes"]
        .as_array()
        .expect("notes array exists")
        .iter()
        .filter_map(|note| note.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        notes.contains("skillctl sync"),
        "the plan must recommend sync: {body:#?}",
    );
    assert!(
        body["warnings"]
            .as_array()
            .expect("warnings array exists")
            .iter()
            .any(|warning| {
                let warning = warning.as_str().expect("warning is a string");
                warning.contains(".claude/skills/release-notes")
                    && warning.contains("skillctl sync")
            }),
        "the run must warn about the planned root: {body:#?}",
    );

    let explain_assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--json", "explain", "release-notes"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    let explain_body: Value = serde_json::from_slice(&explain_assert.get_output().stdout)
        .expect("explain stdout is valid json");
    let explained_missing: Vec<&Value> = explain_body["data"]["targets"]
        .as_array()
        .expect("explain targets exist")
        .iter()
        .filter(|target| target["observed_presence"] == "missing")
        .collect();
    assert_eq!(
        explained_missing
            .iter()
            .map(|target| target["path"].as_str())
            .collect::<Vec<_>>(),
        vec![Some(".claude/skills/release-notes")],
        "explain must observe the same absence: {explain_body:#?}",
    );

    let connection = Connection::open(workspace.home_path().join(".skillctl/state.db"))
        .expect("state database opens");
    let recorded_modifications: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM local_modifications WHERE skill_id = ?1",
            params!["release-notes"],
            |row| row.get(0),
        )
        .expect("modification query succeeds");
    assert_eq!(
        recorded_modifications, 0,
        "a planned root without a copy is not a local modification",
    );

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["sync"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    let synced = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--json", "update", "release-notes"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    let synced_body: Value =
        serde_json::from_slice(&synced.get_output().stdout).expect("stdout is valid json");
    assert_eq!(
        synced_body["data"]["plans"][0].get("missing_projections"),
        None,
        "a materialized projection is no longer missing: {synced_body:#?}",
    );
    assert_eq!(synced_body["warnings"], json!([]));
}

#[test]
fn update_ignores_a_recorded_root_the_plan_no_longer_includes() {
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

    retarget_manifest(&workspace, "claude-code");
    // The abandoned root loses its copy too, which a record-driven check would
    // keep reporting although the plan no longer expects anything there.
    fs::remove_dir_all(workspace.path().join(".agents/skills/release-notes"))
        .expect("abandoned projection deleted");

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
    assert_eq!(
        body["data"]["plans"][0]["missing_projections"],
        json!([{
            "targets": ["claude-code"],
            "root": ".claude/skills",
            "path": ".claude/skills/release-notes",
        }]),
        "only the planned root is named: {body:#?}",
    );
}

#[test]
fn update_names_a_missing_projection_for_a_forked_import() {
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

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--json", "fork", "release-notes"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    // A fork keeps the canonical copy in '.agents/skills', so a target that
    // reads another root names a destination only a sync can rebuild.
    retarget_manifest(&workspace, "claude-code");
    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["sync"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    // An intact fork reports projection health before anything is removed.
    let intact = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--json", "update", "release-notes"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    let intact_body: Value =
        serde_json::from_slice(&intact.get_output().stdout).expect("stdout is valid json");
    assert_eq!(
        intact_body["data"]["plans"][0].get("missing_projections"),
        None,
        "a materialized fork projection is not missing: {intact_body:#?}",
    );
    assert_eq!(intact_body["warnings"], json!([]));

    fs::remove_dir_all(workspace.path().join(".claude/skills/release-notes"))
        .expect("forked projection deleted");

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

    assert_eq!(body["ok"], true, "absence is not an error: {body:#?}");
    assert_eq!(plan["outcome"], "detached");
    assert_eq!(plan["recommended_action"], "skip");
    assert_eq!(plan["trust"]["source_state"], "local-trusted");
    assert_eq!(
        plan["missing_projections"],
        json!([{
            "targets": ["claude-code"],
            "root": ".claude/skills",
            "path": ".claude/skills/release-notes",
        }]),
        "unexpected plan: {body:#?}",
    );
    let notes = plan["notes"]
        .as_array()
        .expect("notes array exists")
        .iter()
        .filter_map(|note| note.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        notes.contains("skillctl sync"),
        "the plan must recommend sync: {body:#?}",
    );
    assert!(
        body["warnings"]
            .as_array()
            .expect("warnings array exists")
            .iter()
            .any(|warning| {
                let warning = warning.as_str().expect("warning is a string");
                warning.contains(".claude/skills/release-notes")
                    && warning.contains("skillctl sync")
            }),
        "the run must warn about the missing destination: {body:#?}",
    );

    // Explain and doctor already named this destination, so the check must
    // observe the same absence they do.
    let explain_assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--json", "explain", "release-notes"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    let explain_body: Value = serde_json::from_slice(&explain_assert.get_output().stdout)
        .expect("explain stdout is valid json");
    assert_eq!(explain_body["data"]["status"], "selected");
    assert_eq!(
        explain_body["data"]["targets"]
            .as_array()
            .expect("explain targets exist")
            .iter()
            .filter(|target| target["observed_presence"] == "missing")
            .map(|target| target["path"].as_str())
            .collect::<Vec<_>>(),
        vec![Some(".claude/skills/release-notes")],
        "explain must observe the same absence: {explain_body:#?}",
    );

    let doctor_assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--json", "doctor"])
        .assert()
        .code(2)
        .stderr(predicate::str::is_empty());
    let doctor_body: Value = serde_json::from_slice(&doctor_assert.get_output().stdout)
        .expect("doctor stdout is valid json");
    assert!(
        doctor_body["data"]["issues"]
            .as_array()
            .expect("doctor issues exist")
            .iter()
            .any(|issue| issue["code"] == "missing-projection"
                && issue["path"] == json!(".claude/skills/release-notes")),
        "doctor must report the same missing destination: {doctor_body:#?}",
    );

    let connection = Connection::open(workspace.home_path().join(".skillctl/state.db"))
        .expect("state database opens");
    let modification_kinds: Vec<String> = connection
        .prepare(
            "SELECT kind FROM local_modifications WHERE skill_id = ?1 \
             ORDER BY detected_at ASC, id ASC",
        )
        .expect("statement prepares")
        .query_map(params!["release-notes"], |row| row.get(0))
        .expect("modification query succeeds")
        .collect::<Result<_, _>>()
        .expect("modification rows decode");
    assert!(
        modification_kinds
            .iter()
            .all(|kind| kind == "detached-fork"),
        "a deleted fork projection is not a local modification: {modification_kinds:?}",
    );

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["sync"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    let synced = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--json", "update", "release-notes"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    let synced_body: Value =
        serde_json::from_slice(&synced.get_output().stdout).expect("stdout is valid json");
    assert_eq!(
        synced_body["data"]["plans"][0].get("missing_projections"),
        None,
        "a rebuilt fork projection is no longer missing: {synced_body:#?}",
    );
    assert_eq!(synced_body["warnings"], json!([]));
}

/// Every warning a plain check reports must survive a `--plan` run, because
/// the document mutates nothing and rebuilds no projection.
fn assert_missing_projection_warning(body: &Value) {
    let warnings = body["warnings"]
        .as_array()
        .expect("warnings array exists")
        .iter()
        .filter_map(|warning| warning.as_str())
        .collect::<Vec<_>>();
    assert!(
        warnings
            .iter()
            .any(|warning| warning.contains("release-notes") && warning.contains("skillctl sync")),
        "the run must warn about the missing projection: {body:#?}",
    );
}

#[test]
fn update_plan_document_warns_about_a_missing_projection_of_an_up_to_date_import() {
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

    // Upstream stays on the installed revision, so the check contributes no
    // apply target, while the projection is gone for every planned target.
    fs::remove_dir_all(workspace.path().join(".agents/skills/release-notes"))
        .expect("generated projection deleted");

    let plain = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--json", "update", "release-notes"])
        .assert()
        .code(2)
        .stderr(predicate::str::is_empty());
    let plain_body: Value =
        serde_json::from_slice(&plain.get_output().stdout).expect("plain stdout is valid json");
    assert_eq!(plain_body["data"]["plans"][0]["outcome"], "up-to-date");
    assert_missing_projection_warning(&plain_body);

    let plan_path = workspace.path().join("update-plan.json");
    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args([
            "--json",
            "update",
            "release-notes",
            "--plan",
            "update-plan.json",
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::is_empty());

    let body: Value =
        serde_json::from_slice(&assert.get_output().stdout).expect("stdout is valid json");
    assert_eq!(
        body["ok"], true,
        "a missing projection is not an error: {body:#?}",
    );
    assert_eq!(
        body["data"]["plans"].as_array().map(Vec::len),
        Some(0),
        "an up-to-date import contributes no plan: {body:#?}",
    );
    assert_eq!(
        body["data"]["plan_skipped"],
        json!([{
            "skill": "release-notes",
            "scope": "workspace",
            "reason": "check outcome was 'up-to-date', so nothing was planned",
        }]),
        "the document names the skill it left out: {body:#?}",
    );
    assert_missing_projection_warning(&body);

    let document: Value =
        serde_json::from_str(&fs::read_to_string(&plan_path).expect("plan document exists"))
            .expect("plan document is valid json");
    assert_eq!(document["plans"].as_array().map(Vec::len), Some(0));
}

#[test]
fn update_plan_document_warns_about_a_missing_projection_of_a_forked_skill() {
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

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--json", "fork", "release-notes"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    // A fork keeps the canonical copy in '.agents/skills', so a target that
    // reads another root names a destination only a sync can rebuild.
    retarget_manifest(&workspace, "claude-code");
    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["sync"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    fs::remove_dir_all(workspace.path().join(".claude/skills/release-notes"))
        .expect("forked projection deleted");

    let plan_path = workspace.path().join("update-plan.json");
    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args([
            "--json",
            "update",
            "release-notes",
            "--plan",
            "update-plan.json",
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::is_empty());

    let body: Value =
        serde_json::from_slice(&assert.get_output().stdout).expect("stdout is valid json");
    assert_eq!(body["ok"], true, "absence is not an error: {body:#?}");
    assert_eq!(
        body["data"]["plans"].as_array().map(Vec::len),
        Some(0),
        "a forked skill contributes no plan: {body:#?}",
    );
    let warnings = body["warnings"]
        .as_array()
        .expect("warnings array exists")
        .iter()
        .filter_map(|warning| warning.as_str())
        .collect::<Vec<_>>();
    assert!(
        warnings
            .iter()
            .any(|warning| warning.contains(".claude/skills/release-notes")
                && warning.contains("skillctl sync")),
        "the document run must warn about the missing destination: {body:#?}",
    );

    let document: Value =
        serde_json::from_str(&fs::read_to_string(&plan_path).expect("plan document exists"))
            .expect("plan document is valid json");
    assert_eq!(document["plans"].as_array().map(Vec::len), Some(0));
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
    assert!(
        !lockfile["imports"]["release-notes"]
            .as_mapping()
            .expect("import entry exists")
            .contains_key("timestamps"),
        "local timestamps stay out of the portable lockfile"
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

/// Read one update plan for a single installed skill.
fn update_plan(workspace: &TestWorkspace, home: &Path, skill: &str) -> Value {
    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", home)
        .args(["--json", "update", skill])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    let body: Value =
        serde_json::from_slice(&assert.get_output().stdout).expect("stdout is valid json");
    body["data"]["plans"][0].clone()
}

/// Read the freshness report of one import from `skillctl --json verify`.
fn verify_freshness(workspace: &TestWorkspace, home: &Path) -> Value {
    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", home)
        .args(["--json", "verify"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    let body: Value =
        serde_json::from_slice(&assert.get_output().stdout).expect("stdout is valid json");
    body["data"]["imports"][0]["freshness"].clone()
}

#[test]
fn update_reports_tracking_as_unavailable_for_a_pinned_commit() {
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
    let pinned_commit = workspace.git_head("git-source");

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--json", "pin", "release-notes", pinned_commit.as_str()])
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
    let latest_commit = workspace.git_head("git-source");

    let plan = update_plan(&workspace, &workspace.home_path(), "release-notes");
    assert_eq!(plan["outcome"], "commit-pinned");
    assert_eq!(plan["pinned_revision"], pinned_commit);
    assert!(
        plan["latest_revision"].is_null(),
        "no upstream revision was observed: {plan:#?}"
    );
    assert_eq!(plan["recommended_action"], "skip");

    let notes = plan["notes"]
        .as_array()
        .expect("notes array exists")
        .iter()
        .filter_map(|note| note.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        notes.contains("upstream tracking is unavailable"),
        "the note must name the unavailable state: {notes}"
    );
    assert!(
        notes.contains("skillctl pin release-notes <branch-or-tag>"),
        "the note must name how tracking resumes: {notes}"
    );
    assert!(
        !notes.contains("pinned revision already matches upstream"),
        "a commit pin must not be reported as an observation: {notes}"
    );

    // The lockfile records no upstream observation for the commit pin, and the
    // check records the named outcome with no fabricated revision.
    assert!(
        workspace.read_lockfile_yaml()["imports"]["release-notes"]["revision"]["upstream"]
            .is_null(),
        "a commit pin observes no upstream revision"
    );
    let connection = Connection::open(workspace.home_path().join(".skillctl/state.db"))
        .expect("state database opens");
    let check_row: (String, Option<String>) = connection
        .query_row(
            "SELECT outcome, latest_revision \
             FROM update_checks WHERE skill_id = ?1 ORDER BY checked_at DESC, id DESC LIMIT 1",
            params!["release-notes"],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("update check exists");
    assert_eq!(check_row, ("commit-pinned".to_string(), None));

    let freshness = verify_freshness(&workspace, &workspace.home_path());
    assert_eq!(freshness["upstream_check"], "unavailable-commit-pinned");
    assert!(
        freshness["upstream_revision"].is_null(),
        "verify must not report an unobserved upstream revision: {freshness:#?}"
    );

    // Pinning a movable reference again resumes tracking: the check observes
    // the branch tip the source really serves.
    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--json", "pin", "release-notes", "main"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    let plan = update_plan(&workspace, &workspace.home_path(), "release-notes");
    assert_eq!(plan["outcome"], "up-to-date");
    assert_eq!(plan["pinned_revision"], latest_commit);
    assert_eq!(plan["latest_revision"], latest_commit);

    let freshness = verify_freshness(&workspace, &workspace.home_path());
    assert_eq!(freshness["upstream_check"], "available");
}

#[test]
fn update_reports_tracking_as_unavailable_after_a_rollback_to_a_commit() {
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
    let rolled_back_commit = workspace.git_head("git-source");

    workspace.write_skill_source_at(
        "git-source",
        ".agents/skills/release-notes",
        "release-notes",
        "Updated upstream release notes helper.",
    );
    workspace.commit_all("git-source", "update release notes");

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args([
            "--json",
            "rollback",
            "release-notes",
            rolled_back_commit.as_str(),
        ])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    workspace.write_skill_source_at(
        "git-source",
        ".agents/skills/release-notes",
        "release-notes",
        "Rewritten upstream release notes helper.",
    );
    workspace.commit_all("git-source", "rewrite release notes");
    let latest_commit = workspace.git_head("git-source");

    let plan = update_plan(&workspace, &workspace.home_path(), "release-notes");
    assert_eq!(plan["outcome"], "commit-pinned");
    assert_eq!(plan["pinned_revision"], rolled_back_commit);
    assert!(
        plan["latest_revision"].is_null(),
        "no upstream revision was observed: {plan:#?}"
    );
    assert_ne!(rolled_back_commit, latest_commit);

    let notes = plan["notes"]
        .as_array()
        .expect("notes array exists")
        .iter()
        .filter_map(|note| note.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        !notes.contains("pinned revision already matches upstream"),
        "a rolled back commit must not be reported as an observation: {notes}"
    );

    let freshness = verify_freshness(&workspace, &workspace.home_path());
    assert_eq!(freshness["upstream_check"], "unavailable-commit-pinned");
    assert!(freshness["upstream_revision"].is_null());
}

#[test]
fn update_withholds_a_legacy_upstream_record_from_a_forked_commit_pinned_import() {
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
    let pinned_commit = workspace.git_head("git-source");

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--json", "pin", "release-notes", pinned_commit.as_str()])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    // A build before the fix recorded the moved-to commit in the install
    // record too, so a checkout made by that build still carries it.
    let connection = Connection::open(workspace.home_path().join(".skillctl/state.db"))
        .expect("state database opens");
    connection
        .execute(
            "UPDATE install_records SET upstream_revision = ?1 WHERE skill_id = ?2",
            params![pinned_commit, "release-notes"],
        )
        .expect("legacy upstream revision is written");
    drop(connection);

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--json", "fork", "release-notes"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    let plan = update_plan(&workspace, &workspace.home_path(), "release-notes");
    assert_eq!(plan["outcome"], "detached");
    assert!(
        plan["latest_revision"].is_null(),
        "a forked commit pin must not report the recorded value as an observation: {plan:#?}",
    );

    let connection = Connection::open(workspace.home_path().join(".skillctl/state.db"))
        .expect("state database opens");
    let check_row: (String, Option<String>) = connection
        .query_row(
            "SELECT outcome, latest_revision \
             FROM update_checks WHERE skill_id = ?1 ORDER BY checked_at DESC, id DESC LIMIT 1",
            params!["release-notes"],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("update check exists");
    assert_eq!(check_row, ("detached".to_string(), None));
}

#[test]
fn update_apply_repairs_edited_copies_of_a_commit_pinned_import() {
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
    let pinned_commit = workspace.git_head("git-source");

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--json", "pin", "release-notes", pinned_commit.as_str()])
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

    // An unmanaged edit in a generated copy is still repairable, because the
    // repair keeps the pinned revision instead of moving upstream.
    let generated_copy = workspace
        .path()
        .join(".agents/skills/release-notes/SKILL.md");
    let mut edited = fs::read_to_string(&generated_copy).expect("generated copy exists");
    edited.push_str("keep my edit\n");
    fs::write(&generated_copy, edited).expect("generated copy is edited");

    let plan = update_plan(&workspace, &workspace.home_path(), "release-notes");
    assert_eq!(plan["outcome"], "commit-pinned");
    assert_eq!(plan["local_modification_detected"], true);
    assert_eq!(plan["recommended_action"], "create-overlay");

    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args([
            "--json",
            "update",
            "release-notes",
            "--apply",
            "--strategy",
            "overlay",
        ])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    let body: Value =
        serde_json::from_slice(&assert.get_output().stdout).expect("stdout is valid json");
    assert_eq!(body["data"]["applied"][0]["skill"], "release-notes");

    assert_eq!(
        workspace.read_lockfile_yaml()["imports"]["release-notes"]["revision"]["resolved"],
        pinned_commit,
        "the repair keeps the commit-pinned revision"
    );

    // A clean apply on a commit pin has nothing to move and says so.
    let output = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--json", "update", "release-notes", "--apply"])
        .output()
        .expect("command runs");
    let body: Value = serde_json::from_slice(&output.stdout).expect("stdout is valid json");
    assert_eq!(
        output.status.code(),
        Some(2),
        "a skipped apply warns: {body}"
    );
    let reasons = body["data"]["apply_skipped"]
        .as_array()
        .expect("skips are listed")
        .iter()
        .filter_map(|skip| skip["reason"].as_str())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        reasons.contains("upstream tracking is unavailable"),
        "the skip names the commit-pinned state: {reasons}"
    );
}

#[test]
fn update_names_init_when_the_workspace_holds_no_manifest() {
    let workspace = TestWorkspace::new();
    workspace.write_workspace_skill("bare", "Bare skill.", &[]);
    let home = workspace.home_path();

    // The named form sees the same refusal, because the check reads the
    // declared imports before it can say anything about `bare`.
    for args in [vec!["update"], vec!["update", "bare"]] {
        let output = Command::cargo_bin("skillctl")
            .expect("binary exists")
            .current_dir(workspace.path())
            .env("HOME", &home)
            .args(&args)
            .output()
            .expect("command runs");
        assert_eq!(
            output.status.code(),
            Some(3),
            "{args:?} must refuse as a validation failure"
        );
        assert!(
            output.stdout.is_empty(),
            "{args:?} reports the refusal on stderr, not as a check report"
        );
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        assert!(
            stderr.contains(".agents/skillctl.yaml"),
            "{args:?} must name the missing manifest: {stderr}"
        );
        assert!(
            stderr.contains("skillctl init"),
            "{args:?} must name the repair: {stderr}"
        );
        assert!(
            !stderr.contains("os error"),
            "{args:?} must not surface the raw filesystem error: {stderr}"
        );
    }

    // The named repair is real: once `init` declares the workspace, the check
    // runs and the plan surface works again.
    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", &home)
        .arg("init")
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    let output = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", &home)
        .args(["--json", "update"])
        .output()
        .expect("command runs");
    let body: Value = serde_json::from_slice(&output.stdout).expect("stdout is valid json");
    assert_eq!(body["ok"], true, "update must answer after init: {body:#?}");

    // A plan document written while the manifest existed refuses the same way
    // once the manifest is gone, because applying it also reads declared state.
    let plan_document = workspace.path().join("update-plan.json");
    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", &home)
        .arg("update")
        .arg("--plan")
        .arg(&plan_document)
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    fs::remove_file(workspace.path().join(".agents/skillctl.yaml"))
        .expect("the manifest is removed again");

    let output = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", &home)
        .args(["update", "--apply", "--plan-file"])
        .arg(&plan_document)
        .output()
        .expect("command runs");
    assert_eq!(
        output.status.code(),
        Some(3),
        "a plan apply must refuse instead of reading a defaulted manifest"
    );
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        stderr.contains("skillctl init"),
        "the plan apply must name the repair: {stderr}"
    );
}

/// `update` in a workspace that holds a manifest and an install but lost its
/// lockfile.
///
/// The check compares installed state with the pins the lockfile holds, so an
/// absent lockfile refuses instead of reporting every import as new. The
/// refusal names both repairs and keeps the raw read error out of the output.
#[test]
fn update_names_both_repairs_when_the_manifest_survives_a_lost_lockfile() {
    let workspace = TestWorkspace::new();
    workspace.write_skill_source("shared-skills", "release-notes");
    let home = workspace.home_path();

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", &home)
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

    // A plan document written while the lockfile existed, for the apply path.
    let plan_document = workspace.path().join("update-plan.json");
    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", &home)
        .arg("update")
        .arg("--plan")
        .arg(&plan_document)
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    fs::remove_file(workspace.path().join(".agents/skillctl.lock"))
        .expect("the lockfile is removed");

    for args in [
        vec!["update"],
        vec!["update", "release-notes"],
        vec!["update", "release-notes", "--apply"],
        vec!["update", "--apply", "--plan-file"],
    ] {
        let mut command = Command::cargo_bin("skillctl").expect("binary exists");
        command
            .current_dir(workspace.path())
            .env("HOME", &home)
            .args(&args);
        if args.last() == Some(&"--plan-file") {
            command.arg(&plan_document);
        }
        let output = command.output().expect("command runs");
        assert_eq!(
            output.status.code(),
            Some(3),
            "{args:?} must refuse as a validation failure"
        );
        assert!(
            output.stdout.is_empty(),
            "{args:?} reports the refusal on stderr, not as a check report"
        );
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        assert!(
            stderr.contains(".agents/skillctl.lock"),
            "{args:?} must name the missing lockfile: {stderr}"
        );
        assert!(
            stderr.contains("'update'"),
            "{args:?} must name the command that needs pinned state: {stderr}"
        );
        assert!(
            stderr.contains("retrieve the committed .agents/skillctl.lock"),
            "{args:?} must name the team-checkout repair: {stderr}"
        );
        assert!(
            stderr.contains("skillctl init"),
            "{args:?} must name the new-workspace repair: {stderr}"
        );
        assert!(
            !stderr.contains("os error"),
            "{args:?} must not surface the raw filesystem error: {stderr}"
        );
    }
}

const WELL_KNOWN_INDEX_PATH: &str = "/.well-known/agent-skills/index.json";
const DISCOVERY_SCHEMA: &str = "https://schemas.agentskills.io/discovery/0.2.0/schema.json";

/// One `skill-md` artifact body with the digest of its bytes.
fn well_known_skill_md(name: &str, description: &str) -> (String, String) {
    let body = format!("---\nname: {name}\ndescription: {description}\n---\n\n# {name}\n");
    let digest = {
        let mut hasher = Sha256::new();
        hasher.update(body.as_bytes());
        format!("sha256:{:x}", hasher.finalize())
    };
    (body, digest)
}

/// One discovery index entry as raw JSON.
fn well_known_entry(name: &str, url: &str, digest: &str) -> String {
    format!(
        "{{\"name\":\"{name}\",\"type\":\"skill-md\",\"description\":\"Published through a \
         well-known index.\",\"url\":\"{url}\",\"digest\":\"{digest}\"}}"
    )
}

/// What one editable test origin currently serves.
///
/// The update check and the apply re-read the discovery index, so a test has to
/// change what the index declares between requests.
#[derive(Clone)]
struct EditableOrigin {
    index: Arc<std::sync::Mutex<String>>,
    artifacts: Arc<std::sync::Mutex<BTreeMap<String, String>>>,
}

impl EditableOrigin {
    /// Declare the index document and the artifact bodies it references.
    fn declare(&self, entries: &[String], artifacts: &[(&str, String)]) {
        *self.index.lock().expect("index state locks") = format!(
            "{{\"$schema\":\"{DISCOVERY_SCHEMA}\",\"skills\":[{}]}}",
            entries.join(",")
        );
        let mut served = self.artifacts.lock().expect("artifact state locks");
        served.clear();
        for (path, body) in artifacts {
            served.insert((*path).to_string(), body.clone());
        }
    }
}

/// Serve an editable discovery origin on a free loopback port.
fn serve_editable_origin() -> (String, EditableOrigin) {
    let origin = EditableOrigin {
        index: Arc::new(std::sync::Mutex::new(String::new())),
        artifacts: Arc::new(std::sync::Mutex::new(BTreeMap::new())),
    };
    let listener = TcpListener::bind("127.0.0.1:0").expect("test origin binds a free port");
    let address = listener
        .local_addr()
        .expect("test origin reports its address");
    let served = origin.clone();

    thread::spawn(move || {
        while let Ok((mut stream, _)) = listener.accept() {
            let mut buffer = [0_u8; 8192];
            let read = stream.read(&mut buffer).unwrap_or(0);
            let request = String::from_utf8_lossy(&buffer[..read]);
            let path = request
                .lines()
                .next()
                .and_then(|line| line.split_whitespace().nth(1))
                .unwrap_or_default()
                .split('?')
                .next()
                .unwrap_or_default()
                .to_string();

            let (status, body) = if path == WELL_KNOWN_INDEX_PATH {
                (
                    200_u16,
                    served.index.lock().expect("index state locks").clone(),
                )
            } else if let Some(body) = served
                .artifacts
                .lock()
                .expect("artifact state locks")
                .get(&path)
                .cloned()
            {
                (200, body)
            } else {
                (404, "{\"error\":\"not found\"}".to_string())
            };
            let reason = if status == 200 { "OK" } else { "Not Found" };
            let response = format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n",
                body.len(),
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.write_all(body.as_bytes());
            let _ = stream.flush();
        }
    });

    (format!("http://{address}"), origin)
}

/// Declare the one `code-review` entry an editable origin serves.
fn declare_code_review(origin: &EditableOrigin, body: &str, digest: &str) {
    origin.declare(
        &[well_known_entry(
            "code-review",
            "/download/code-review.md",
            digest,
        )],
        &[("/download/code-review.md", body.to_string())],
    );
}

/// Install the `code-review` entry an editable origin currently declares.
fn install_well_known(workspace: &TestWorkspace, origin: &str) {
    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--no-input", "--name", "code-review", "install", origin])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
}

/// Run one `skillctl` command in the workspace and return its JSON body.
fn run_update_json(workspace: &TestWorkspace, args: &[&str]) -> Value {
    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(args)
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    serde_json::from_slice(&assert.get_output().stdout).expect("stdout is valid json")
}

/// The plan of one skill inside an update response.
fn plan_for<'a>(body: &'a Value, skill: &str) -> &'a Value {
    body["data"]["plans"]
        .as_array()
        .expect("plans array exists")
        .iter()
        .find(|plan| plan["skill"] == skill)
        .unwrap_or_else(|| panic!("no plan for '{skill}': {body:#?}"))
}

#[test]
fn update_re_reads_the_discovery_index_and_reports_a_digest_change() {
    let (origin, state) = serve_editable_origin();
    let (first_body, first_digest) = well_known_skill_md("code-review", "Review code.");
    let (second_body, second_digest) = well_known_skill_md("code-review", "Review code again.");
    declare_code_review(&state, &first_body, &first_digest);
    let workspace = TestWorkspace::new();
    workspace.write_manifest(concat!(
        "version: 1\n",
        "\n",
        "targets:\n",
        "  - claude-code\n",
    ));
    install_well_known(&workspace, &origin);

    // The publisher replaces the artifact and declares the new digest.
    declare_code_review(&state, &second_body, &second_digest);
    let body = run_update_json(&workspace, &["--json", "update", "code-review"]);
    let plan = plan_for(&body, "code-review");
    assert_eq!(plan["outcome"], "update-available");
    assert_eq!(plan["recommended_action"], "apply");
    assert_eq!(plan["pinned_revision"], first_digest);
    assert_eq!(plan["latest_revision"], second_digest);
    assert!(
        plan["notes"]
            .as_array()
            .expect("notes array exists")
            .iter()
            .any(|note| note
                .as_str()
                .unwrap_or_default()
                .contains("the discovery index now declares the digest")),
        "the note should name the digest change: {plan:#?}",
    );

    // The loopback origin is local-only, so its update event stays suppressed.
    assert_eq!(
        body["data"]["telemetry"]["events"][0]["source_visibility"], "suppressed-local",
        "unexpected telemetry classification: {body:#?}",
    );

    let connection = Connection::open(workspace.home_path().join(".skillctl/state.db"))
        .expect("state database opens");
    let outcome: (String, String) = connection
        .query_row(
            "SELECT outcome, latest_revision FROM update_checks WHERE skill_id = 'code-review' \
             ORDER BY checked_at DESC, id DESC LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("update check exists");
    assert_eq!(outcome, ("update-available".to_string(), second_digest));
}

#[test]
fn update_apply_moves_a_well_known_import_to_the_declared_digest() {
    let (origin, state) = serve_editable_origin();
    let (first_body, first_digest) = well_known_skill_md("code-review", "Review code.");
    let (second_body, second_digest) = well_known_skill_md("code-review", "Review code again.");
    declare_code_review(&state, &first_body, &first_digest);
    let workspace = TestWorkspace::new();
    workspace.write_manifest(concat!(
        "version: 1\n",
        "\n",
        "targets:\n",
        "  - claude-code\n",
    ));
    install_well_known(&workspace, &origin);

    declare_code_review(&state, &second_body, &second_digest);
    let body = run_update_json(&workspace, &["--json", "update", "code-review", "--apply"]);
    assert_eq!(
        body["data"]["applied"][0]["resolved_revision"], second_digest,
        "the apply should move the import to the declared digest: {body:#?}",
    );
    assert_eq!(
        body["data"]["applied"][0]["previous_revision"],
        first_digest
    );

    let lockfile = fs::read_to_string(workspace.path().join(".agents/skillctl.lock"))
        .expect("lockfile exists");
    assert!(
        lockfile.contains(&format!("digest: {second_digest}")),
        "the discovery block should pin the new digest: {lockfile}",
    );
    assert!(
        lockfile.contains(&format!("resolved: {second_digest}")),
        "the revision should pin the new digest: {lockfile}",
    );
    let manifest = fs::read_to_string(workspace.path().join(".agents/skillctl.yaml"))
        .expect("manifest exists");
    assert!(
        manifest.contains(&format!("ref: {second_digest}")),
        "the manifest ref should pin the new digest: {manifest}",
    );
    assert_eq!(
        fs::read_to_string(workspace.path().join(".claude/skills/code-review/SKILL.md"))
            .expect("projection exists"),
        second_body,
        "the apply should rebuild the projection from the new artifact",
    );

    // The moved import still verifies, because the lockfile and the store now
    // describe the same content.
    let verified = run_update_json(&workspace, &["--json", "verify"]);
    assert_eq!(verified["ok"], true, "verify should pass: {verified:#?}");

    // The moved pin reads as up to date, because the check compares it with
    // the digest the index declares now.
    let body = run_update_json(&workspace, &["--json", "update", "code-review"]);
    let plan = plan_for(&body, "code-review");
    assert_eq!(plan["outcome"], "up-to-date");
    assert_eq!(plan["pinned_revision"], second_digest);
    assert_eq!(plan["latest_revision"], second_digest);
}

#[test]
fn a_plan_document_refuses_when_the_index_moved_past_its_digest() {
    let (origin, state) = serve_editable_origin();
    let (first_body, first_digest) = well_known_skill_md("code-review", "Review code.");
    let (second_body, second_digest) = well_known_skill_md("code-review", "Review code again.");
    let (third_body, third_digest) = well_known_skill_md("code-review", "Review code once more.");
    declare_code_review(&state, &first_body, &first_digest);
    let workspace = TestWorkspace::new();
    workspace.write_manifest(concat!(
        "version: 1\n",
        "\n",
        "targets:\n",
        "  - claude-code\n",
    ));
    install_well_known(&workspace, &origin);

    declare_code_review(&state, &second_body, &second_digest);
    run_update_json(
        &workspace,
        &["--json", "update", "code-review", "--plan", "plan.json"],
    );

    // The publisher moves on again before the document is applied, so the
    // digest it proposes is no longer what the index declares.
    declare_code_review(&state, &third_body, &third_digest);
    let output = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["update", "--apply", "--plan-file", "plan.json"])
        .output()
        .expect("command runs");
    assert_eq!(
        output.status.code(),
        Some(3),
        "the apply must refuse as a validation failure"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("now declares the digest") && stderr.contains(&third_digest),
        "the refusal should name the digest the index declares now: {stderr}",
    );
    assert!(
        stderr.contains("skillctl update code-review"),
        "the refusal should name the re-run that observes the newer entry: {stderr}",
    );

    // The import keeps the digest it pinned.
    let lockfile = fs::read_to_string(workspace.path().join(".agents/skillctl.lock"))
        .expect("lockfile exists");
    assert!(
        lockfile.contains(&format!("resolved: {first_digest}")),
        "a refused apply must not move the pin: {lockfile}",
    );
}

#[test]
fn update_reports_a_failed_check_when_the_index_drops_the_entry() {
    let (origin, state) = serve_editable_origin();
    let (body, digest) = well_known_skill_md("code-review", "Review code.");
    declare_code_review(&state, &body, &digest);
    let workspace = TestWorkspace::new();
    workspace.write_manifest(concat!(
        "version: 1\n",
        "\n",
        "targets:\n",
        "  - claude-code\n",
    ));
    install_well_known(&workspace, &origin);

    // The publisher drops the entry entirely.
    state.declare(&[], &[("/download/code-review.md", body)]);
    let check = run_update_json(&workspace, &["--json", "update", "code-review"]);
    let plan = plan_for(&check, "code-review");
    assert_eq!(plan["outcome"], "failed");
    assert_eq!(plan["recommended_action"], "skip");
    assert!(
        plan["notes"]
            .as_array()
            .expect("notes array exists")
            .iter()
            .any(|note| note
                .as_str()
                .unwrap_or_default()
                .contains("no longer declares 'code-review'")),
        "the note should name the dropped entry: {plan:#?}",
    );

    // The pinned content is untouched, so the workspace still verifies.
    run_update_json(&workspace, &["--json", "verify"]);
}

#[test]
fn update_and_discover_refuse_a_discovery_origin_the_trust_policy_blocks() {
    let workspace = TestWorkspace::new();
    workspace.write_manifest(concat!(
        "version: 1\n",
        "\n",
        "targets:\n",
        "  - claude-code\n",
    ));

    // 192.0.2.1 is a non-loopback address, so the origin-trust policy refuses
    // it before anything is fetched; no listener has to exist.
    let refused = "http://192.0.2.1";
    let install = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--no-input", "--name", "code-review", "install", refused])
        .output()
        .expect("command runs");
    assert_eq!(install.status.code(), Some(3));
    let stderr = String::from_utf8_lossy(&install.stderr);
    assert!(
        stderr.contains("origin trust policy refuses this discovery origin"),
        "the install refusal should name the policy: {stderr}",
    );

    let discover = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["discover", refused])
        .output()
        .expect("command runs");
    assert_eq!(discover.status.code(), Some(3));
    let stderr = String::from_utf8_lossy(&discover.stderr);
    assert!(
        stderr.contains("origin trust policy refuses this discovery origin"),
        "the discover refusal should name the policy: {stderr}",
    );
}

#[test]
fn a_well_known_check_whose_origin_the_policy_refuses_fails_without_contacting_it() {
    let (origin, state) = serve_editable_origin();
    let (body, digest) = well_known_skill_md("code-review", "Review code.");
    declare_code_review(&state, &body, &digest);
    let workspace = TestWorkspace::new();
    workspace.write_manifest(concat!(
        "version: 1\n",
        "\n",
        "targets:\n",
        "  - claude-code\n",
    ));
    install_well_known(&workspace, &origin);

    // A lockfile committed by another machine can pin a plain-HTTP origin this
    // build refuses to contact. The recorded source URL is what the check
    // re-reads, so rewriting it simulates that checkout without a listener.
    let connection = Connection::open(workspace.home_path().join(".skillctl/state.db"))
        .expect("state database opens");
    connection
        .execute(
            "UPDATE install_records SET source_url = 'http://192.0.2.1/.well-known/agent-skills/\
             index.json' WHERE skill_id = 'code-review'",
            [],
        )
        .expect("source URL is rewritten");

    let check = run_update_json(&workspace, &["--json", "update", "code-review"]);
    let plan = plan_for(&check, "code-review");
    assert_eq!(plan["outcome"], "failed");
    assert!(
        plan["notes"]
            .as_array()
            .expect("notes array exists")
            .iter()
            .any(|note| note
                .as_str()
                .unwrap_or_default()
                .contains("origin trust policy refuses this discovery origin")),
        "the failed check should name the policy: {plan:#?}",
    );
}
