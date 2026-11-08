use super::*;

#[test]
fn explain_conflict_reports_failure_across_cli_json_and_exit_code() {
    let workspace = TestWorkspace::new();
    seed_same_name_import_conflict(&workspace);

    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--json", "explain", "release-notes"])
        .assert()
        .code(3)
        .stderr(predicate::str::is_empty());
    let body: Value =
        serde_json::from_slice(&assert.get_output().stdout).expect("stdout is valid json");

    assert_eq!(body["command"], "explain");
    assert_eq!(body["ok"], false, "conflict explain must not claim success");
    assert!(
        body["errors"]
            .as_array()
            .expect("errors array exists")
            .iter()
            .any(|error| error
                .as_str()
                .expect("error is a string")
                .contains("same-name conflict")),
        "unexpected errors: {body:#?}",
    );
    assert_eq!(body["data"]["status"], "conflict");
    for target in body["data"]["targets"]
        .as_array()
        .expect("explain targets exist")
    {
        assert_eq!(
            target["visible"], false,
            "conflicted skill must not report target visibility: {target:#?}",
        );
        assert!(
            target.get("observed_matches_winner").is_none(),
            "no winner exists to match against: {target:#?}",
        );
    }
}

#[test]
fn inspect_conflict_reports_failure_across_cli_json_and_exit_code() {
    let workspace = TestWorkspace::new();
    seed_same_name_import_conflict(&workspace);

    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--json", "inspect", "release-notes"])
        .assert()
        .code(3)
        .stderr(predicate::str::is_empty());
    let body: Value =
        serde_json::from_slice(&assert.get_output().stdout).expect("stdout is valid json");

    assert_eq!(body["command"], "inspect");
    assert_eq!(body["ok"], false);
    assert_eq!(body["data"]["status"], "conflict");
}

#[test]
fn doctor_conflict_reports_failure_across_cli_json_and_exit_code() {
    let workspace = TestWorkspace::new();
    seed_same_name_import_conflict(&workspace);

    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--json", "doctor"])
        .assert()
        .code(3)
        .stderr(predicate::str::is_empty());
    let body: Value =
        serde_json::from_slice(&assert.get_output().stdout).expect("stdout is valid json");

    assert_eq!(body["command"], "doctor");
    assert_eq!(body["ok"], false);
    assert!(
        body["data"]["issues"]
            .as_array()
            .expect("issues array exists")
            .iter()
            .any(|issue| issue["code"] == "projected-name-conflict"),
        "unexpected issues: {body:#?}",
    );
}

#[test]
fn explain_conflict_human_output_names_the_repair_step() {
    let workspace = TestWorkspace::new();
    seed_same_name_import_conflict(&workspace);

    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .arg("explain")
        .arg("release-notes")
        .assert()
        .code(3)
        .stdout(predicate::str::is_empty());

    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    assert!(
        stderr.contains("same-name conflict"),
        "unexpected stderr: {stderr}",
    );
    assert!(
        stderr.contains("fix:"),
        "human output must surface the repair step: {stderr}",
    );
}

#[test]
fn explain_reports_observed_projection_state_per_target() {
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

    let healthy = explain_targets(&workspace);
    assert_eq!(healthy.len(), 1);
    assert_eq!(healthy[0]["target"], "claude-code");
    assert_eq!(healthy[0]["visible"], true);
    assert_eq!(healthy[0]["observed_presence"], "present");
    assert_eq!(healthy[0]["observed_matches_winner"], true);
    assert_eq!(
        healthy[0]["observed_digest"], healthy[0]["expected_digest"],
        "healthy projection digests must agree: {healthy:#?}",
    );
    assert!(
        healthy[0]["observed_digest"]
            .as_str()
            .expect("observed digest exists")
            .starts_with("sha256:"),
        "unexpected digest: {healthy:#?}",
    );
    assert!(
        healthy[0].get("fix").is_none(),
        "healthy targets must not suggest repairs: {healthy:#?}",
    );

    workspace.write_workspace_skill("release-notes", "Canonical release notes helper.", &[]);

    let unsynced = explain_targets(&workspace);
    assert_eq!(unsynced[0]["observed_presence"], "present");
    assert_eq!(unsynced[0]["observed_matches_winner"], false);
    assert_eq!(unsynced[0]["visible"], false);
    assert_ne!(
        unsynced[0]["observed_digest"], unsynced[0]["expected_digest"],
        "stale projection content must not match the new winner: {unsynced:#?}",
    );

    fs::remove_dir_all(workspace.path().join(".claude/skills/release-notes"))
        .expect("projection removed");

    let missing = explain_targets(&workspace);
    assert_eq!(missing[0]["visible"], false, "missing: {missing:#?}");
    assert_eq!(missing[0]["observed_presence"], "missing");
    assert!(
        missing[0].get("observed_digest").is_none(),
        "missing projections have no digest: {missing:#?}",
    );
    assert!(
        missing[0]["expected_digest"].is_string(),
        "the winner still pins an expected digest: {missing:#?}",
    );
    assert!(
        missing[0]["fix"]
            .as_str()
            .expect("fix exists")
            .contains("skillctl sync"),
        "missing projections must point at repair: {missing:#?}",
    );
}

#[test]
fn explain_observes_projections_per_scope() {
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
            "--scope",
            "user",
            "--target",
            "claude-code",
            "install",
            "shared-skills",
        ])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    let user_assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--json", "--scope", "user", "explain", "release-notes"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    let user_body: Value =
        serde_json::from_slice(&user_assert.get_output().stdout).expect("stdout is valid json");
    assert_eq!(user_body["data"]["status"], "selected");
    assert_eq!(
        user_body["data"]["targets"][0]["root"],
        "home/.claude/skills"
    );
    assert_eq!(
        user_body["data"]["targets"][0]["observed_presence"],
        "present"
    );
    assert_eq!(user_body["data"]["targets"][0]["visible"], true);

    let workspace_assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--json", "explain", "release-notes"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    let workspace_body: Value = serde_json::from_slice(&workspace_assert.get_output().stdout)
        .expect("stdout is valid json");
    assert_eq!(
        workspace_body["data"]["targets"][0]["root"],
        ".claude/skills"
    );
    assert_eq!(
        workspace_body["data"]["targets"][0]["observed_presence"], "missing",
        "workspace scope must observe its own root: {workspace_body:#?}",
    );
    assert_eq!(workspace_body["data"]["targets"][0]["visible"], false);
}

/// Install one skill at user scope with the default targets, the state an
/// unscoped `explain` and `inspect` must read through workspace scope.
fn seed_user_scope_import(workspace: &TestWorkspace) {
    workspace.write_manifest(concat!(
        "version: 1\n",
        "\n",
        "targets:\n",
        "  - codex\n",
        "  - gemini-cli\n",
        "  - opencode\n",
    ));
    workspace.write_skill_source("shared-skills", "delta");

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args([
            "--no-input",
            "--name",
            "delta",
            "--scope",
            "user",
            "install",
            "shared-skills",
        ])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
}

#[test]
fn explain_reports_workspace_absence_for_a_user_scope_winner() {
    let workspace = TestWorkspace::new();
    seed_user_scope_import(&workspace);

    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--json", "explain", "delta"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    let body: Value =
        serde_json::from_slice(&assert.get_output().stdout).expect("stdout is valid json");

    assert_eq!(body["ok"], true, "explain body: {body:#?}");
    assert_eq!(body["data"]["status"], "selected");
    assert_eq!(body["data"]["scope"], "workspace");
    assert_eq!(body["data"]["winner"]["scope"], "user");
    let issues = body["data"]["issues"]
        .as_array()
        .expect("issues array exists");
    assert!(
        issues.is_empty(),
        "a healthy user-scope import reports no workspace issue: {body:#?}",
    );

    let targets = body["data"]["targets"]
        .as_array()
        .expect("explain targets exist");
    assert_eq!(targets.len(), 3);
    for target in targets {
        assert_eq!(target["root"], ".agents/skills", "target: {target:#?}");
        assert_eq!(target["observed_presence"], "missing");
        assert_eq!(target["visible"], false);
        let reason = target["reason"].as_str().expect("reason is a string");
        assert!(
            reason.contains("managed at user scope")
                && reason.contains("does not project into workspace scope"),
            "the observation must be absence from workspace scope: {reason}",
        );
        let fix = target["fix"].as_str().expect("the fix names a repair");
        assert!(
            fix.contains("skillctl --scope user explain delta")
                && fix.contains("skillctl --scope user sync"),
            "the fix must name commands that change the report: {fix}",
        );
    }

    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .arg("explain")
        .arg("delta")
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    assert!(
        stdout.contains("workspace scope plans no projection for it"),
        "the summary must state the scope absence: {stdout}",
    );
    assert!(
        stdout.contains("skillctl --scope user explain delta"),
        "the summary must name a command that resolves the observation: {stdout}",
    );
    assert!(
        !stdout.contains("'skillctl sync'"),
        "a plain sync cannot change this report, so it must not be advised: {stdout}",
    );

    // A plain sync in workspace scope materializes nothing for this skill, so
    // the report must read the same after one runs.
    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .arg("sync")
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .arg("explain")
        .arg("delta")
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "workspace scope plans no projection for it",
        ))
        .stdout(predicate::str::contains(
            "skillctl --scope user explain delta",
        ));

    // `doctor` reads the same scope and must agree on the skill's health.
    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .arg("doctor")
        .assert()
        .success()
        .stdout(predicate::str::contains("0 errors, 0 warnings"));

    // The scope the winner lives in still verifies its own projections.
    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--scope", "user", "explain", "delta"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "3 of 3 target projections verified",
        ));
}

#[test]
fn inspect_labels_a_user_scope_import_with_its_own_scope() {
    let workspace = TestWorkspace::new();
    seed_user_scope_import(&workspace);

    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .arg("inspect")
        .arg("delta")
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    assert!(
        stdout.contains("Inspected preflight for delta in user scope"),
        "the import must be labeled with its own scope: {stdout}",
    );
    assert!(
        !stdout.contains("in workspace scope"),
        "a user-scope import must not be labeled as workspace scope: {stdout}",
    );
    assert!(
        stdout.contains("skillctl --scope user explain delta"),
        "the note must name the scope that holds the projections: {stdout}",
    );

    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--json", "inspect", "delta"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    let body: Value =
        serde_json::from_slice(&assert.get_output().stdout).expect("stdout is valid json");
    assert_eq!(body["data"]["scope"], "workspace");
    assert_eq!(body["data"]["candidate"]["scope"], "user");
}

#[test]
fn user_scope_explain_reports_user_scope_absence_for_a_workspace_import() {
    let workspace = TestWorkspace::new();
    workspace.write_manifest(concat!(
        "version: 1\n",
        "\n",
        "targets:\n",
        "  - codex\n",
        "  - gemini-cli\n",
        "  - opencode\n",
    ));
    workspace.write_skill_source("shared-skills", "delta");

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--no-input", "--name", "delta", "install", "shared-skills"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    // The observation is symmetric: user scope plans no projection for a
    // workspace import either.
    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--json", "--scope", "user", "explain", "delta"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    let body: Value =
        serde_json::from_slice(&assert.get_output().stdout).expect("stdout is valid json");
    assert_eq!(body["data"]["scope"], "user");
    assert_eq!(body["data"]["winner"]["scope"], "workspace");
    for target in body["data"]["targets"]
        .as_array()
        .expect("explain targets exist")
    {
        assert_eq!(target["visible"], false, "target: {target:#?}");
        let fix = target["fix"].as_str().expect("the fix names a repair");
        assert!(
            fix.contains("skillctl --scope workspace explain delta"),
            "the fix must name the scope that owns the projections: {fix}",
        );
    }
}

/// Seed one canonical workspace skill with the default targets, the state a
/// `--scope user` `explain` must read through user scope.
fn seed_canonical_workspace_skill(workspace: &TestWorkspace) {
    workspace.write_manifest(concat!(
        "version: 1\n",
        "\n",
        "targets:\n",
        "  - codex\n",
        "  - gemini-cli\n",
        "  - opencode\n",
    ));
    workspace.write_workspace_skill("bare", "A canonical workspace skill.", &[]);
    // A manifest without a lockfile is a lost file, and `doctor` reports it.
    // This fixture seeds a healthy workspace, so it carries the lockfile
    // `skillctl init` writes beside the manifest.
    workspace.write_lockfile(MINIMAL_LOCKFILE.as_str());
}

#[test]
fn user_scope_explain_names_the_scope_mismatch_for_a_workspace_canonical_winner() {
    let workspace = TestWorkspace::new();
    seed_canonical_workspace_skill(&workspace);

    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--json", "--scope", "user", "explain", "bare"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    let body: Value =
        serde_json::from_slice(&assert.get_output().stdout).expect("stdout is valid json");

    assert_eq!(body["ok"], true, "explain body: {body:#?}");
    assert_eq!(body["data"]["status"], "selected");
    // The report stays labeled with the requested scope while it names a
    // winner the workspace owns.
    assert_eq!(body["data"]["scope"], "user");
    assert_eq!(body["data"]["winner"]["scope"], "workspace");
    assert_eq!(body["data"]["winner"]["root"], ".agents/skills/bare");
    assert_eq!(body["data"]["winner"]["source_class"], "canonical-local");
    assert!(
        body["data"]["issues"]
            .as_array()
            .expect("issues array exists")
            .is_empty(),
        "a healthy canonical skill reports no user-scope issue: {body:#?}",
    );

    let targets = body["data"]["targets"]
        .as_array()
        .expect("explain targets exist");
    assert_eq!(targets.len(), 3);
    for target in targets {
        assert_eq!(target["root"], "home/.agents/skills", "target: {target:#?}");
        assert_eq!(target["observed_presence"], "missing");
        assert_eq!(target["visible"], false);
        let reason = target["reason"].as_str().expect("reason is a string");
        assert!(
            reason.contains("managed at workspace scope")
                && reason.contains("does not project into user scope"),
            "the observation must be absence from user scope: {reason}",
        );
        let fix = target["fix"].as_str().expect("the fix names a repair");
        assert!(
            fix.contains("skillctl --scope workspace explain bare")
                && fix.contains("skillctl --scope workspace sync"),
            "the fix must name commands that change the report: {fix}",
        );
    }

    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--scope", "user", "explain", "bare"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    assert!(
        stdout.contains("user scope plans no projection for it"),
        "the summary must state the scope absence: {stdout}",
    );
    assert!(
        stdout.contains("skillctl --scope workspace explain bare"),
        "the summary must name a command that resolves the observation: {stdout}",
    );
    assert!(
        !stdout.contains("'skillctl sync'"),
        "a plain sync cannot change this report, so it must not be advised: {stdout}",
    );

    // A user-scope sync materializes nothing for this skill, so the report
    // must read the same after one runs.
    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--scope", "user", "sync"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    assert!(
        !workspace.path().join("home/.agents/skills/bare").exists(),
        "a user-scope sync cannot project a workspace canonical skill",
    );
    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--scope", "user", "explain", "bare"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "user scope plans no projection for it",
        ))
        .stdout(predicate::str::contains(
            "skillctl --scope workspace explain bare",
        ));

    // `doctor` reads the same scope and must agree on the skill's health.
    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--scope", "user", "doctor"])
        .assert()
        .success()
        .stdout(predicate::str::contains("0 errors, 0 warnings"));

    // The scope the winner lives in still verifies its own projections.
    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--scope", "workspace", "explain", "bare"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "3 of 3 target projections verified",
        ));
}

#[test]
fn user_scope_explain_names_the_scope_qualified_prune_for_a_leftover_user_copy() {
    let workspace = TestWorkspace::new();
    workspace.write_manifest(concat!(
        "version: 1\n",
        "\n",
        "targets:\n",
        "  - codex\n",
        "  - gemini-cli\n",
        "  - opencode\n",
    ));
    workspace.write_skill_source("shared-skills", "bare");

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args([
            "--no-input",
            "--name",
            "bare",
            "--scope",
            "user",
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
        .args(["--scope", "user", "sync"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    // A canonical skill added later wins the name, so the user-root copy the
    // import generated becomes a leftover this scope no longer plans.
    workspace.write_workspace_skill("bare", "A canonical workspace skill.", &[]);

    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--json", "--scope", "user", "explain", "bare"])
        // Exit code 2 is success with warnings: the leftover copy is one.
        .assert()
        .code(2)
        .stderr(predicate::str::is_empty());
    let body: Value =
        serde_json::from_slice(&assert.get_output().stdout).expect("stdout is valid json");

    assert_eq!(body["ok"], true, "explain body: {body:#?}");
    assert_eq!(body["data"]["scope"], "user");
    assert_eq!(body["data"]["winner"]["scope"], "workspace");
    let issue = body["data"]["issues"]
        .as_array()
        .expect("issues array exists")
        .iter()
        .find(|issue| issue["code"] == "stale-generated-projection")
        .unwrap_or_else(|| panic!("the leftover user-root copy must be reported: {body:#?}"));
    let fix = issue["fix"].as_str().expect("the fix names a repair");
    assert!(
        fix.contains("'skillctl --scope user sync'"),
        "a plain sync reads workspace scope and cannot prune a user root: {fix}",
    );

    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--scope", "user", "explain", "bare"])
        .assert()
        .code(2)
        .stderr(predicate::str::is_empty());
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    assert!(
        stdout.contains("user scope plans no projection for it"),
        "the summary must still state the scope absence: {stdout}",
    );
    assert!(
        stdout.contains("Run 'skillctl --scope user sync' to prune"),
        "the summary must name the sync that prunes a user root: {stdout}",
    );
    assert!(
        !stdout.contains("'skillctl sync'"),
        "a plain sync cannot prune a user root, so it must not be advised: {stdout}",
    );

    // The named command is the one that actually prunes the copy.
    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--scope", "user", "sync"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    assert!(
        !workspace.path().join("home/.agents/skills/bare").exists(),
        "the user-scope sync must prune the leftover copy",
    );
    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--scope", "user", "explain", "bare"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "user scope plans no projection for it",
        ));
}

/// A report that observes user scope must name `skillctl --scope user sync` in
/// every repair text, because a plain `skillctl sync` reads workspace scope
/// and cannot materialize a user root.
#[test]
fn user_scope_reports_name_the_scope_qualified_sync_for_a_missing_projection() {
    let workspace = TestWorkspace::new();
    seed_user_scope_import(&workspace);

    let user_copy = workspace.path().join("home/.agents/skills/delta");
    assert!(user_copy.is_dir(), "the install projected the user copy");
    fs::remove_dir_all(&user_copy).expect("the user copy is removed");

    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--json", "--scope", "user", "explain", "delta"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    let body: Value =
        serde_json::from_slice(&assert.get_output().stdout).expect("stdout is valid json");
    for target in body["data"]["targets"]
        .as_array()
        .expect("explain targets exist")
    {
        let fix = target["fix"].as_str().expect("the fix names a repair");
        assert!(
            fix.contains("run skillctl --scope user sync to materialize the missing projection"),
            "a plain sync cannot materialize a user root: {fix}",
        );
    }

    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--scope", "user", "explain", "delta"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    assert!(
        stdout.contains(
            "Run 'skillctl --scope user sync' to materialize or refresh the remaining projections.",
        ),
        "the summary must name the sync that reads user scope: {stdout}",
    );
    assert!(
        !stdout.contains("'skillctl sync'"),
        "a plain sync cannot change this report, so it must not be advised: {stdout}",
    );

    // `doctor` reads the same scope, so its fixes must agree.
    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--scope", "user", "doctor"])
        // Exit code 2 is success with warnings: the missing projection is one.
        .assert()
        .code(2)
        .stderr(predicate::str::is_empty());
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    assert!(
        stdout
            .contains("fix: run skillctl --scope user sync to materialize the missing projection"),
        "the doctor fix must name the sync that reads user scope: {stdout}",
    );
    assert!(
        !stdout.contains("run skillctl sync to materialize"),
        "a plain sync cannot change this report, so it must not be advised: {stdout}",
    );

    // An update check reads the same plan, so its warning must agree too.
    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--scope", "user", "update", "delta"])
        // Exit code 2 is success with warnings: the missing projection is one.
        .assert()
        .code(2)
        .stderr(predicate::str::is_empty());
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    assert!(
        stdout.contains("run 'skillctl --scope user sync' to materialize the missing projection"),
        "the update warning must name the sync that reads user scope: {stdout}",
    );

    // The command the reports refuse to name really cannot deliver the repair.
    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .arg("sync")
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    assert!(
        !user_copy.exists(),
        "a plain sync reads workspace scope and leaves the user root missing",
    );
    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--scope", "user", "sync"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    assert!(
        user_copy.is_dir(),
        "the qualified sync materializes the user root",
    );

    // A blocked destination reads the same scope, so its fix must agree.
    fs::remove_dir_all(&user_copy).expect("the user copy is removed again");
    fs::write(&user_copy, "not a directory").expect("the planned path is blocked");
    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--scope", "user", "doctor"])
        .assert()
        .code(2)
        .stderr(predicate::str::is_empty());
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    assert!(
        stdout.contains("then run skillctl --scope user sync"),
        "the blocked-projection fix must name the sync that reads user scope: {stdout}",
    );
    assert!(
        !stdout.contains("then run skillctl sync"),
        "a plain sync cannot change this report, so it must not be advised: {stdout}",
    );

    // A precedence change reads the same scope, so its fix must agree too.
    let manifest = fs::read_to_string(workspace.path().join(".agents/skillctl.yaml"))
        .expect("the manifest exists");
    workspace.write_manifest(&format!(
        "{manifest}\nadapters:\n  codex:\n    user_root: .custom-user-root\n"
    ));
    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--scope", "user", "doctor"])
        .assert()
        .code(2)
        .stderr(predicate::str::is_empty());
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    assert!(
        stdout.contains(
            "fix: run skillctl --scope user sync to regenerate projections in the planned roots"
        ),
        "the wrong-precedence-root fix must name the sync that reads user scope: {stdout}",
    );
    assert!(
        !stdout.contains("run skillctl sync to regenerate"),
        "a plain sync cannot change this report, so it must not be advised: {stdout}",
    );
}

/// A user-scope copy that holds unrecorded edits is refreshed only by the
/// qualified discard, so the summary must name that command.
#[test]
fn user_scope_explain_names_the_scope_qualified_discard_for_an_edited_copy() {
    let workspace = TestWorkspace::new();
    seed_user_scope_import(&workspace);

    let skill_file = workspace.path().join("home/.agents/skills/delta/SKILL.md");
    let mut contents = fs::read_to_string(&skill_file).expect("the generated copy exists");
    contents.push_str("\n# local note\n");
    fs::write(&skill_file, contents).expect("the generated copy is edited");

    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--scope", "user", "explain", "delta"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    assert!(
        stdout.contains("a plain 'skillctl --scope user sync' preserves the copy"),
        "the summary must explain the sync of the scope it observes: {stdout}",
    );
    assert!(
        stdout.contains("'skillctl --scope user sync --discard' also refreshes the copy"),
        "the summary must name the discard that reads user scope: {stdout}",
    );
    assert!(
        !stdout.contains("'skillctl sync'"),
        "a plain sync cannot change this report, so it must not be advised: {stdout}",
    );

    // The discard the summary refuses to name really cannot refresh the copy.
    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["sync", "--discard"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    assert!(
        fs::read_to_string(&skill_file)
            .expect("the copy survives the plain discard")
            .contains("# local note"),
        "a plain discard reads workspace scope and keeps the edited user copy",
    );
    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--scope", "user", "sync", "--discard"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    assert!(
        !fs::read_to_string(&skill_file)
            .expect("the copy was refreshed")
            .contains("# local note"),
        "the qualified discard refreshes the edited user copy",
    );

    // A removal leaves the edited copy behind with discard-only advice, which
    // must name the same qualified command.
    let mut contents = fs::read_to_string(&skill_file).expect("the refreshed copy exists");
    contents.push_str("\n# another note\n");
    fs::write(&skill_file, contents).expect("the generated copy is edited again");
    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--no-input", "--scope", "user", "remove", "delta"])
        // Exit code 2 is success with warnings: the preserved copy is one.
        .assert()
        .code(2)
        .stderr(predicate::str::is_empty());
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    assert!(
        stdout.contains("'skillctl --scope user sync --discard'"),
        "the removal's advice must name the discard that reads user scope: {stdout}",
    );
    assert!(
        !stdout.contains("'skillctl sync --discard'"),
        "a plain discard cannot repair a user-scope copy: {stdout}",
    );
}

#[test]
fn explain_and_doctor_agree_on_a_leftover_copy_under_a_user_scope_winner() {
    let workspace = TestWorkspace::new();
    seed_user_scope_import(&workspace);

    // Leave the generated user-scope copy in the workspace root the default
    // targets plan, the state every assigned runtime keeps loading.
    workspace.copy_path(
        &workspace.path().join("home/.agents/skills/delta"),
        ".agents/skills/delta",
    );

    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--json", "explain", "delta"])
        // Exit code 2 is success with warnings: the leftover copy is one.
        .assert()
        .code(2)
        .stderr(predicate::str::is_empty());
    let body: Value =
        serde_json::from_slice(&assert.get_output().stdout).expect("stdout is valid json");
    assert_eq!(body["ok"], true, "explain body: {body:#?}");
    let codes = body["data"]["issues"]
        .as_array()
        .expect("issues array exists")
        .iter()
        .map(|issue| issue["code"].as_str().unwrap_or_default().to_string())
        .collect::<Vec<_>>();
    assert!(
        codes.contains(&"stale-generated-projection".to_string()),
        "explain must report the copy the way doctor does: {body:#?}",
    );
    for target in body["data"]["targets"]
        .as_array()
        .expect("explain targets exist")
    {
        assert_eq!(target["observed_presence"], "present");
        assert_eq!(
            target["visible"], false,
            "a copy this scope does not plan is not verified visibility: {target:#?}",
        );
        assert!(
            target.get("fix").is_none(),
            "the stale-copy issue carries the prune repair: {target:#?}",
        );
    }

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--json", "doctor"])
        .assert()
        .code(2)
        .stdout(predicate::str::contains("stale-generated-projection"));

    // The repair both surfaces is one a plain sync delivers.
    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .arg("sync")
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    assert!(
        !workspace.path().join(".agents/skills/delta").exists(),
        "sync prunes the copy this scope does not plan",
    );
    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .arg("explain")
        .arg("delta")
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "workspace scope plans no projection for it",
        ));
}

#[test]
fn explain_does_not_call_a_disabled_user_scope_imports_pin_stale() {
    let workspace = TestWorkspace::new();
    seed_user_scope_import(&workspace);

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--scope", "user", "disable", "delta"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--json", "explain", "delta"])
        // Exit code 2 is success with warnings: the disable is one.
        .assert()
        .code(2)
        .stderr(predicate::str::is_empty());
    let body: Value =
        serde_json::from_slice(&assert.get_output().stdout).expect("stdout is valid json");

    let issues = body["data"]["issues"]
        .as_array()
        .expect("issues array exists");
    assert!(
        !issues
            .iter()
            .any(|issue| issue["code"] == "stale-lockfile-entry"),
        "the user-scope import owns the pin, so it is not stale: {body:#?}",
    );
    let disabled = issues
        .iter()
        .find(|issue| issue["code"] == "disabled-import")
        .unwrap_or_else(|| panic!("the disable must still be reported: {body:#?}"));
    assert_eq!(disabled["scope"], "user", "issue: {disabled:#?}");
    let fix = disabled["fix"].as_str().expect("fix is a string");
    assert!(
        fix.contains("skillctl --scope user enable delta")
            && fix.contains("skillctl --scope user remove delta"),
        "the repair must name commands that work in the import's scope: {fix}",
    );

    // The named repair is the one that changes the report.
    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--scope", "user", "enable", "delta"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--scope", "user", "explain", "delta"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "3 of 3 target projections verified",
        ));
}

#[cfg(unix)]
#[test]
fn explain_reports_unreadable_projection_roots() {
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

    let root = workspace.path().join(".claude/skills");
    set_mode(&root, 0o000);

    let targets = explain_targets(&workspace);
    set_mode(&root, 0o755);

    assert_eq!(
        targets[0]["observed_presence"], "unreadable",
        "{targets:#?}"
    );
    assert_eq!(targets[0]["visible"], false);
    assert!(
        targets[0]["fix"]
            .as_str()
            .expect("fix exists")
            .contains("permissions"),
        "unreadable roots must point at permissions: {targets:#?}",
    );
}

#[test]
fn doctor_distinguishes_missing_projections_from_drift() {
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

    fs::remove_dir_all(workspace.path().join(".claude/skills/release-notes"))
        .expect("projection removed");

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
    let issues = body["data"]["issues"]
        .as_array()
        .expect("issues array exists");

    assert_eq!(body["ok"], true);
    assert!(
        issues
            .iter()
            .any(|issue| issue["code"] == "missing-projection"
                && issue["skill"] == "release-notes"
                && issue["target"] == "claude-code"),
        "missing projection must be reported explicitly: {body:#?}",
    );
    let missing_fix = issues
        .iter()
        .find(|issue| issue["code"] == "missing-projection")
        .expect("missing-projection issue exists");
    assert!(
        missing_fix["fix"]
            .as_str()
            .expect("fix exists")
            .contains("skillctl sync"),
        "unexpected fix: {missing_fix:#?}",
    );

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .arg("sync")
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    workspace.write_file(
        ".claude/skills/release-notes/SKILL.md",
        concat!(
            "---\n",
            "name: release-notes\n",
            "description: Hand-edited projected copy.\n",
            "---\n",
            "\n",
            "# Release Notes\n"
        ),
    );

    let drift_assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--json", "doctor"])
        .assert()
        .code(2)
        .stderr(predicate::str::is_empty());
    let drift_body: Value =
        serde_json::from_slice(&drift_assert.get_output().stdout).expect("stdout is valid json");

    assert!(
        drift_body["data"]["issues"]
            .as_array()
            .expect("issues array exists")
            .iter()
            .any(|issue| issue["code"] == "projection-drift"
                && issue["skill"] == "release-notes"
                && issue["target"] == "claude-code"),
        "edited projection must report drift: {drift_body:#?}",
    );
}

#[test]
fn doctor_human_output_includes_repair_steps() {
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
        .env("HOME", workspace.home_path())
        .arg("doctor")
        .assert()
        .code(2)
        .stderr(predicate::str::is_empty());

    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    assert!(
        stdout.contains(
            "warning: [claude-code]: target 'claude-code' documents unstable symlink behavior",
        ),
        "unexpected stdout: {stdout}",
    );
    assert!(
        stdout.contains("fix:"),
        "human doctor output must surface repair steps: {stdout}",
    );
    assert!(
        stdout.contains("projection.allow_unsafe_targets"),
        "repair steps must stay actionable: {stdout}",
    );
}

#[test]
fn explain_missing_projection_human_output_points_at_sync() {
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

    fs::remove_dir_all(workspace.path().join(".claude/skills/release-notes"))
        .expect("projection removed");

    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .arg("explain")
        .arg("release-notes")
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    assert!(
        stdout.contains("skillctl sync"),
        "human explain output must point at the missing projection repair: {stdout}",
    );
}

#[test]
fn doctor_reports_stale_generated_projections_left_in_planned_roots() {
    let workspace = TestWorkspace::new();
    seed_stale_generated_projection(&workspace, ".agents/skills");

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
        .find(|issue| issue["code"] == "stale-generated-projection")
        .expect("stale generated projection issue exists");

    assert_eq!(issue["skill"], "delta");
    assert_eq!(issue["path"], ".agents/skills/delta");
    assert!(
        issue["message"]
            .as_str()
            .expect("message exists")
            .contains("codex"),
        "the issue must name a runtime that still reads the root: {issue:#?}",
    );
    assert!(
        issue["fix"]
            .as_str()
            .expect("fix exists")
            .contains("skillctl sync"),
        "the issue must point at the repair: {issue:#?}",
    );
}

#[test]
fn doctor_stays_quiet_when_a_generated_projection_belongs_to_an_active_import() {
    let workspace = TestWorkspace::new();
    workspace.write_manifest(concat!(
        "version: 1\n",
        "\n",
        "targets:\n",
        "  - codex\n",
        "  - gemini-cli\n",
        "  - opencode\n",
    ));
    workspace.write_skill_source("shared-skills", "delta");

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--no-input", "--name", "delta", "install", "shared-skills"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--json", "doctor"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    let body: Value =
        serde_json::from_slice(&assert.get_output().stdout).expect("stdout is valid json");

    assert!(
        !body["data"]["issues"]
            .as_array()
            .expect("issues array exists")
            .iter()
            .any(|issue| issue["code"] == "stale-generated-projection"),
        "the projection of an enabled import is not stale: {body:#?}",
    );
}

#[test]
fn doctor_reports_stale_generated_projections_left_in_roots_that_stopped_being_planned() {
    let workspace = TestWorkspace::new();
    workspace.write_manifest(concat!(
        "version: 1\n",
        "\n",
        "targets:\n",
        "  - claude-code\n",
    ));
    workspace.write_lockfile(MINIMAL_LOCKFILE.as_str());
    workspace.write_file(
        ".agents/skills/delta/SKILL.md",
        concat!(
            "---\n",
            "name: delta\n",
            "description: Leftover generated projection.\n",
            "---\n",
            "\n",
            "# Delta\n"
        ),
    );
    workspace.write_file(
        ".agents/skills/delta/.skillctl-projection.json",
        concat!(
            "{\n",
            "  \"tool\": \"skillctl\",\n",
            "  \"generation_mode\": \"copy\",\n",
            "  \"physical_root\": \".agents/skills\"\n",
            "}\n"
        ),
    );

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
        .find(|issue| issue["code"] == "stale-generated-projection")
        .expect("the unplanned root must be reported");

    assert_eq!(issue["skill"], "delta");
    assert_eq!(issue["path"], ".agents/skills/delta");
    assert!(
        issue["message"]
            .as_str()
            .expect("message exists")
            .contains("no enabled target plans"),
        "the issue must say the root stopped being planned: {issue:#?}",
    );
}

#[test]
fn explain_names_the_generated_copy_left_in_a_root_the_runtimes_read() {
    let workspace = TestWorkspace::new();
    seed_stale_generated_projection(&workspace, ".agents/skills");

    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .arg("explain")
        .arg("delta")
        .assert()
        // Exit code 2 is success with warnings: the leftover copy is one.
        .code(2)
        .stderr(predicate::str::is_empty());
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();

    assert!(
        stdout.contains("not currently visible"),
        "the summary must still state that no candidate resolves: {stdout}",
    );
    assert!(
        stdout.contains(".agents/skills/delta"),
        "the summary must name the leftover generated copy: {stdout}",
    );
    assert!(
        stdout.contains("skillctl sync"),
        "the summary must point at the prune repair: {stdout}",
    );
    assert!(
        stdout.contains("warning:"),
        "explain must report the leftover copy as a warning: {stdout}",
    );

    let json_assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--json", "explain", "delta"])
        .assert()
        .code(2)
        .stderr(predicate::str::is_empty());
    let body: Value =
        serde_json::from_slice(&json_assert.get_output().stdout).expect("stdout is valid json");

    assert_eq!(body["data"]["status"], "missing");
    assert!(
        body["data"]["issues"]
            .as_array()
            .expect("issues array exists")
            .iter()
            .any(|issue| issue["code"] == "stale-generated-projection"),
        "unexpected issues: {body:#?}",
    );

    let inspect_assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--json", "inspect", "delta"])
        .assert()
        .code(2)
        .stderr(predicate::str::is_empty());
    let inspect_body: Value =
        serde_json::from_slice(&inspect_assert.get_output().stdout).expect("stdout is valid json");
    assert!(
        inspect_body["data"]["issues"]
            .as_array()
            .expect("issues array exists")
            .iter()
            .any(|issue| issue["code"] == "stale-generated-projection"),
        "preflight must report the leftover copy too: {inspect_body:#?}",
    );
}

/// Install one skill from a Git source and disable it, so the manifest, the
/// lockfile, and the import store all agree and only the disable decision stops
/// the skill from resolving.
fn seed_disabled_import(workspace: &TestWorkspace) {
    workspace.write_skill_source("git-source", "release-notes");
    workspace.init_git_repo("git-source");

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args([
            "--no-input",
            "--name",
            "release-notes",
            "install",
            workspace.git_repo_url("git-source").as_str(),
        ])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["disable", "release-notes"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
}

#[test]
fn explain_does_not_call_a_disabled_imports_lockfile_pin_stale() {
    let workspace = TestWorkspace::new();
    seed_disabled_import(&workspace);

    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--json", "explain", "release-notes"])
        .assert()
        // Exit code 2 is success with warnings: the disable is one.
        .code(2)
        .stderr(predicate::str::is_empty());
    let body: Value =
        serde_json::from_slice(&assert.get_output().stdout).expect("stdout is valid json");

    assert_eq!(body["ok"], true, "explain body: {body:#?}");
    assert_eq!(body["data"]["status"], "missing");
    let issues = body["data"]["issues"]
        .as_array()
        .expect("issues array exists");
    assert!(
        !issues
            .iter()
            .any(|issue| issue["code"] == "stale-lockfile-entry"),
        "a disabled import keeps its pin, so the entry is not stale: {body:#?}",
    );

    let disabled = issues
        .iter()
        .find(|issue| issue["code"] == "disabled-import")
        .expect("the disable must still be reported");
    let fix = disabled["fix"].as_str().expect("fix is a string");
    assert!(
        fix.contains("skillctl enable release-notes"),
        "the fix must name the enable repair: {fix}",
    );
    assert!(
        fix.contains("skillctl remove release-notes"),
        "the fix must name the removal repair: {fix}",
    );

    // The pin the issue would call stale is exactly what `restore --frozen`
    // and a later `enable` still need, so it must stay in place.
    assert!(
        !workspace.read_lockfile_yaml()["imports"]["release-notes"].is_null(),
        "the lockfile must keep pinning the disabled import",
    );
}

#[test]
fn explain_keeps_reporting_a_leftover_copy_without_calling_the_pin_stale() {
    let workspace = TestWorkspace::new();
    seed_disabled_import(&workspace);

    // Leave one generated copy behind in a root the default targets plan, the
    // state the runtimes keep loading even though nothing resolves any more.
    workspace.write_file(
        ".agents/skills/release-notes/SKILL.md",
        concat!(
            "---\n",
            "name: release-notes\n",
            "description: Summarize release notes.\n",
            "---\n",
            "\n",
            "# Release Notes\n"
        ),
    );
    workspace.write_file(
        ".agents/skills/release-notes/.skillctl-projection.json",
        concat!(
            "{\n",
            "  \"tool\": \"skillctl\",\n",
            "  \"generation_mode\": \"copy\",\n",
            "  \"physical_root\": \".agents/skills\"\n",
            "}\n"
        ),
    );

    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--json", "explain", "release-notes"])
        .assert()
        .code(2)
        .stderr(predicate::str::is_empty());
    let body: Value =
        serde_json::from_slice(&assert.get_output().stdout).expect("stdout is valid json");

    let codes = body["data"]["issues"]
        .as_array()
        .expect("issues array exists")
        .iter()
        .map(|issue| issue["code"].as_str().unwrap_or_default().to_string())
        .collect::<Vec<_>>();
    assert!(
        codes.contains(&"disabled-import".to_string())
            && codes.contains(&"stale-generated-projection".to_string()),
        "both the disable and the leftover copy must be reported: {body:#?}",
    );
    assert!(
        !codes.contains(&"stale-lockfile-entry".to_string()),
        "the kept pin is not stale even with a leftover copy: {body:#?}",
    );
}

#[test]
fn inspect_does_not_call_a_disabled_imports_lockfile_pin_stale() {
    let workspace = TestWorkspace::new();
    seed_disabled_import(&workspace);

    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--json", "inspect", "release-notes"])
        .assert()
        .code(2)
        .stderr(predicate::str::is_empty());
    let body: Value =
        serde_json::from_slice(&assert.get_output().stdout).expect("stdout is valid json");

    let issues = body["data"]["issues"]
        .as_array()
        .expect("issues array exists");
    assert!(
        !issues
            .iter()
            .any(|issue| issue["code"] == "stale-lockfile-entry"),
        "preflight must not report the kept pin as stale: {body:#?}",
    );
    assert!(
        issues
            .iter()
            .any(|issue| issue["code"] == "disabled-import"),
        "preflight must still report the disable: {body:#?}",
    );
}

#[test]
fn explain_human_output_for_a_disabled_import_names_supported_repairs() {
    let workspace = TestWorkspace::new();
    seed_disabled_import(&workspace);

    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .arg("explain")
        .arg("release-notes")
        .assert()
        .code(2)
        .stderr(predicate::str::is_empty());
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();

    assert!(
        stdout.contains("is disabled"),
        "the warning must name the disable: {stdout}",
    );
    assert!(
        stdout.contains("skillctl enable release-notes")
            && stdout.contains("skillctl remove release-notes"),
        "the repair must name supported commands: {stdout}",
    );
    assert!(
        !stdout.contains("stale"),
        "no part of the report may call the kept pin stale: {stdout}",
    );
}

#[test]
fn doctor_reports_a_disabled_import_the_way_explain_does() {
    let workspace = TestWorkspace::new();
    seed_disabled_import(&workspace);

    // `explain` warns about the disable for the skill the reader names, so an
    // unscoped `doctor` must not read as health in the same workspace.
    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .arg("doctor")
        .assert()
        // Exit code 2 is success with warnings: the disable is one.
        .code(2)
        .stderr(predicate::str::is_empty());
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    assert!(
        stdout.contains("manifest import 'release-notes' exists but is disabled"),
        "doctor must name the disable: {stdout}",
    );
    assert!(
        stdout.contains("skillctl enable release-notes")
            && stdout.contains("skillctl remove release-notes"),
        "the doctor repair must name supported commands: {stdout}",
    );

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

    assert_eq!(body["ok"], true, "doctor body: {body:#?}");
    assert_eq!(
        body["data"]["summary"]["warning_count"], 1,
        "the disable is the only warning: {body:#?}",
    );
    let issues = body["data"]["issues"]
        .as_array()
        .expect("issues array exists");
    assert!(
        !issues
            .iter()
            .any(|issue| issue["code"] == "stale-lockfile-entry"),
        "a disabled import keeps its pin, so the entry is not stale: {body:#?}",
    );
    let disabled = issues
        .iter()
        .find(|issue| issue["code"] == "disabled-import")
        .expect("doctor must report the disable");
    assert_eq!(disabled["severity"], "warning", "issue: {disabled:#?}");

    // The repair the issue names is the one that clears the report.
    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["enable", "release-notes"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .arg("doctor")
        .assert()
        .success()
        .stdout(predicate::str::contains("0 errors, 0 warnings"));
}

#[test]
fn doctor_names_the_owning_scope_for_a_disabled_user_scope_import() {
    let workspace = TestWorkspace::new();
    seed_user_scope_import(&workspace);

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--scope", "user", "disable", "delta"])
        .assert()
        .success()
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

    let disabled = body["data"]["issues"]
        .as_array()
        .expect("issues array exists")
        .iter()
        .find(|issue| issue["code"] == "disabled-import")
        .unwrap_or_else(|| panic!("doctor must report the user-scope disable: {body:#?}"));
    assert_eq!(disabled["scope"], "user", "issue: {disabled:#?}");
    let fix = disabled["fix"].as_str().expect("fix is a string");
    assert!(
        fix.contains("skillctl --scope user enable delta")
            && fix.contains("skillctl --scope user remove delta"),
        "the repair must name commands that work in the import's scope: {fix}",
    );
}

#[test]
fn explain_still_reports_a_lockfile_entry_nothing_declares() {
    let workspace = TestWorkspace::new();
    workspace.write_manifest(concat!("version: 1\n", "\n", "targets:\n", "  - codex\n",));
    workspace.write_lockfile(concat!(
        "version: 1\n",
        "\n",
        "state:\n",
        "  manifest_version: 1\n",
        "  local_state_version: 2\n",
        "\n",
        "imports:\n",
        "  orphan-skill:\n",
        "    source:\n",
        "      type: local-path\n",
        "      url: file:///tmp/does-not-exist\n",
        "      subpath: .agents/skills/orphan-skill\n",
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
        .args(["--json", "explain", "orphan-skill"])
        .assert()
        .code(2)
        .stderr(predicate::str::is_empty());
    let body: Value =
        serde_json::from_slice(&assert.get_output().stdout).expect("stdout is valid json");

    let issue = body["data"]["issues"]
        .as_array()
        .expect("issues array exists")
        .iter()
        .find(|issue| issue["code"] == "stale-lockfile-entry")
        .unwrap_or_else(|| panic!("an undeclared entry must stay stale: {body:#?}"));
    let fix = issue["fix"].as_str().expect("fix is a string");
    assert!(
        fix.contains("skillctl remove orphan-skill"),
        "the fix must name the command that clears the entry: {fix}",
    );
    assert!(
        fix.contains("skillctl install"),
        "the fix must still name the reinstall that declares the import again: {fix}",
    );

    // The named repair is the one `remove` performs on this exact state.
    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--json", "remove", "orphan-skill"])
        .assert()
        .code(2)
        .stderr(predicate::str::is_empty());
    let removed: Value =
        serde_json::from_slice(&assert.get_output().stdout).expect("stdout is valid json");
    assert_eq!(removed["ok"], true, "remove failed: {removed:#?}");
    assert!(
        workspace.read_lockfile_yaml()["imports"]["orphan-skill"].is_null(),
        "the entry the fix named must be gone",
    );
}

#[test]
fn doctor_reports_adapter_verification_metadata() {
    let workspace = TestWorkspace::new();
    workspace.write_manifest(concat!("version: 1\n", "\n", "targets:\n", "  - codex\n",));
    // A manifest without a lockfile is a lost file, not a healthy workspace,
    // and doctor reports it. This test reads the adapter metadata, so its
    // fixture carries the lockfile `init` would have written.
    workspace.write_lockfile(MINIMAL_LOCKFILE.as_str());

    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--json", "doctor"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    let body: Value =
        serde_json::from_slice(&assert.get_output().stdout).expect("stdout is valid json");
    let adapters = body["data"]["adapters"]
        .as_array()
        .expect("doctor reports adapter verification metadata");

    assert_eq!(adapters.len(), 6);
    for entry in adapters {
        assert!(
            entry["verified_on"]
                .as_str()
                .expect("verification date exists")
                .starts_with("20"),
            "unexpected entry: {entry:#?}",
        );
        assert!(
            entry["documentation_sources"]
                .as_array()
                .expect("documentation sources exist")
                .iter()
                .all(|source| source.as_str().unwrap_or_default().starts_with("https://")),
            "unexpected entry: {entry:#?}",
        );
    }
    let copilot = adapters
        .iter()
        .find(|entry| entry["target"] == "github-copilot")
        .expect("copilot verification entry exists");
    assert!(
        copilot["documentation_sources"]
            .as_array()
            .expect("copilot sources exist")
            .iter()
            .any(|source| {
                source
                    .as_str()
                    .expect("source is a string")
                    .contains("docs.github.com")
            }),
        "unexpected copilot entry: {copilot:#?}",
    );

    let validate_assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--json", "validate"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    let validate_body: Value =
        serde_json::from_slice(&validate_assert.get_output().stdout).expect("stdout is valid json");
    assert!(
        validate_body["data"].get("adapters").is_none(),
        "validate must stay unchanged: {validate_body:#?}",
    );
}

#[test]
fn doctor_reports_a_manifest_free_workspace_as_an_issue() {
    let workspace = TestWorkspace::new();
    workspace.write_workspace_skill("bare", "Bare skill.", &[]);

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
    assert_eq!(body["ok"], true, "doctor must keep answering: {body:#?}");
    let issues = body["data"]["issues"]
        .as_array()
        .expect("issues array exists");
    let manifest_issue = issues
        .iter()
        .find(|issue| issue["code"] == "manifest-missing")
        .expect("the missing manifest must be reported as an issue");
    assert_eq!(manifest_issue["severity"], "warning");
    assert_eq!(manifest_issue["path"], ".agents/skillctl.yaml");
    assert!(
        manifest_issue["fix"]
            .as_str()
            .expect("fix exists")
            .contains("skillctl init"),
        "the issue must name the repair: {manifest_issue:#?}",
    );
    assert!(
        issues
            .iter()
            .all(|issue| issue["code"] != "lockfile-missing"),
        "a workspace without a manifest never adopted declared state, so the absent \
         lockfile belongs to the manifest issue alone: {issues:#?}",
    );

    // The human report says the same thing without JSON.
    let human_assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .arg("doctor")
        .assert()
        .code(2)
        .stderr(predicate::str::is_empty());
    let stdout = String::from_utf8_lossy(&human_assert.get_output().stdout).into_owned();
    assert!(
        stdout.contains("1 warning"),
        "the summary must count the issue: {stdout}",
    );
    assert!(
        stdout.contains("skillctl init"),
        "the human output must name the repair: {stdout}",
    );

    // `validate` keeps accepting the documented manifest-free local flow, so
    // the issue stays a doctor finding instead of a validation error.
    let validate_assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .arg("validate")
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    let validate_stdout =
        String::from_utf8_lossy(&validate_assert.get_output().stdout).into_owned();
    assert!(
        validate_stdout.contains("validate checked 1 skill: 0 errors, 0 warnings."),
        "validate must stay clean for the simplest local-only flow: {validate_stdout}",
    );

    // A directory that never had a manifest, skills, or a lockfile is not a
    // workspace that lost one. The bundled user-scope flow runs there, so
    // doctor stays quiet instead of asking for `skillctl init`.
    let bare = TestWorkspace::new();
    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(bare.path())
        .env("HOME", bare.home_path())
        .arg("doctor")
        .assert()
        .success()
        .stderr(predicate::str::is_empty())
        .stdout(predicate::str::contains(
            "doctor checked 0 skills: 0 errors, 0 warnings.",
        ));
}

#[test]
fn doctor_reports_a_lost_lockfile_beside_a_surviving_manifest() {
    let workspace = TestWorkspace::new();
    workspace.write_manifest(concat!(
        "version: 1\n",
        "\n",
        "targets:\n",
        "  - codex\n",
        "  - gemini-cli\n",
        "  - opencode\n",
    ));
    workspace.write_workspace_skill("bare", "Bare skill.", &[]);

    // The manifest declares no imports, so no per-import
    // `missing-lockfile-entry` error can carry the loss. The warning is then
    // the only doctor finding, and it keeps both repairs the refusals name.
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
    assert_eq!(body["ok"], true, "doctor must keep answering: {body:#?}");
    let issues = body["data"]["issues"]
        .as_array()
        .expect("issues array exists");
    let lockfile_issue = issues
        .iter()
        .find(|issue| issue["code"] == "lockfile-missing")
        .expect("the absent lockfile must be reported beside the manifest");
    assert_eq!(lockfile_issue["severity"], "warning");
    assert_eq!(lockfile_issue["path"], ".agents/skillctl.lock");
    let fix = lockfile_issue["fix"].as_str().expect("fix exists");
    assert!(
        fix.contains("retrieve the committed .agents/skillctl.lock"),
        "the issue must name the team-checkout repair: {lockfile_issue:#?}",
    );
    assert!(
        fix.contains("skillctl init") && fix.contains("pinned nothing yet"),
        "the issue must keep the init repair conditional on nothing being pinned: \
         {lockfile_issue:#?}",
    );

    // The human report says the same thing without JSON.
    let human_assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .arg("doctor")
        .assert()
        .code(2)
        .stderr(predicate::str::is_empty());
    let stdout = String::from_utf8_lossy(&human_assert.get_output().stdout).into_owned();
    assert!(
        stdout.contains("1 warning"),
        "the summary must count the issue: {stdout}",
    );
    assert!(
        stdout.contains("retrieve the committed .agents/skillctl.lock")
            && stdout.contains("pinned nothing yet"),
        "the human output must keep both repairs: {stdout}",
    );

    // doctor agrees with the commands that refuse on the same state instead of
    // reporting a healthy workspace that pins nothing.
    for command in ["verify", "update"] {
        let output = Command::cargo_bin("skillctl")
            .expect("binary exists")
            .current_dir(workspace.path())
            .env("HOME", workspace.home_path())
            .args(["--json", command])
            .output()
            .expect("command runs");
        assert_eq!(
            output.status.code(),
            Some(3),
            "{command} must refuse as a validation failure"
        );
        let refusal: Value = serde_json::from_slice(&output.stdout).expect("stdout is valid json");
        assert_eq!(refusal["ok"], false, "{command} refusal body: {refusal:#?}");
        assert!(
            refusal["errors"]
                .as_array()
                .expect("errors array exists")
                .iter()
                .any(|error| error
                    .as_str()
                    .expect("error is a string")
                    .contains("no lockfile at '.agents/skillctl.lock'")),
            "{command} must name the missing lockfile: {refusal:#?}",
        );
    }

    // `validate` stays a validation report: the loss is a doctor finding, the
    // way the manifest-free flow keeps `validate` clean.
    let validate_assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .arg("validate")
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    let validate_stdout =
        String::from_utf8_lossy(&validate_assert.get_output().stdout).into_owned();
    assert!(
        validate_stdout.contains("validate checked 1 skill: 0 errors, 0 warnings."),
        "validate must stay clean: {validate_stdout}",
    );

    // A manifest that declares imports keeps the per-import error and carries
    // the same warning beside it, so the workspace-level repairs stay named.
    workspace.write_manifest(concat!(
        "version: 1\n",
        "\n",
        "imports:\n",
        "  - id: rel\n",
        "    type: git\n",
        "    url: https://example.invalid/skills.git\n",
        "    ref: HEAD\n",
        "    path: .agents/skills/rel\n",
        "    scope: workspace\n",
        "    enabled: true\n",
        "\n",
        "targets:\n",
        "  - codex\n",
    ));
    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--json", "doctor"])
        .assert()
        .code(3)
        .stderr(predicate::str::is_empty());
    let body: Value =
        serde_json::from_slice(&assert.get_output().stdout).expect("stdout is valid json");
    let issues = body["data"]["issues"]
        .as_array()
        .expect("issues array exists");
    assert!(
        issues
            .iter()
            .any(|issue| issue["code"] == "missing-lockfile-entry" && issue["severity"] == "error"),
        "the declared import must stay a per-import error: {issues:#?}",
    );
    assert!(
        issues
            .iter()
            .any(|issue| issue["code"] == "lockfile-missing" && issue["severity"] == "warning"),
        "the absent lockfile must stay a named warning beside the errors: {issues:#?}",
    );

    // The warning tracks the file, not the entry: a present lockfile that
    // lacks one import keeps only the per-import error.
    workspace.write_lockfile(MINIMAL_LOCKFILE.as_str());
    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--json", "doctor"])
        .assert()
        .code(3)
        .stderr(predicate::str::is_empty());
    let body: Value =
        serde_json::from_slice(&assert.get_output().stdout).expect("stdout is valid json");
    let issues = body["data"]["issues"]
        .as_array()
        .expect("issues array exists");
    assert!(
        issues
            .iter()
            .all(|issue| issue["code"] != "lockfile-missing"),
        "a present lockfile must not report the file as absent: {issues:#?}",
    );
}

fn explain_targets(workspace: &TestWorkspace) -> Vec<Value> {
    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--json", "explain", "release-notes"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    let body: Value =
        serde_json::from_slice(&assert.get_output().stdout).expect("stdout is valid json");
    assert_eq!(body["ok"], true, "explain body: {body:#?}");
    body["data"]["targets"]
        .as_array()
        .expect("explain targets exist")
        .clone()
}

/// Leave one skillctl-generated copy of `delta` in a planned runtime root that
/// no candidate resolves to any more.
fn seed_stale_generated_projection(workspace: &TestWorkspace, root: &str) {
    workspace.write_manifest(concat!(
        "version: 1\n",
        "\n",
        "targets:\n",
        "  - codex\n",
        "  - gemini-cli\n",
        "  - opencode\n",
    ));
    workspace.write_lockfile(MINIMAL_LOCKFILE.as_str());
    workspace.write_file(
        &format!("{root}/delta/SKILL.md"),
        concat!(
            "---\n",
            "name: delta\n",
            "description: Leftover generated projection.\n",
            "---\n",
            "\n",
            "# Delta\n"
        ),
    );
    workspace.write_file(
        &format!("{root}/delta/.skillctl-projection.json"),
        concat!(
            "{\n",
            "  \"tool\": \"skillctl\",\n",
            "  \"generation_mode\": \"copy\",\n",
            "  \"physical_root\": \".agents/skills\"\n",
            "}\n"
        ),
    );
}

/// Install one local skill, then duplicate the managed import under two ids so
/// the resolver reports a same-name conflict for `release-notes`.
fn seed_same_name_import_conflict(workspace: &TestWorkspace) {
    workspace.write_manifest(concat!("version: 1\n", "\n", "targets:\n", "  - codex\n",));
    workspace.write_skill_source("conflict-source", "release-notes");

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args([
            "--no-input",
            "--name",
            "release-notes",
            "install",
            "conflict-source",
        ])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    let stored = workspace.workspace_import_root("release-notes");
    assert!(
        stored.is_dir(),
        "stored import exists at {}",
        stored.display()
    );
    fs::rename(&stored, workspace.workspace_import_root("first")).expect("import renamed");
    replace_in_file(
        &workspace.path().join(".agents/skillctl.yaml"),
        "- id: release-notes",
        "- id: first",
    );
    replace_in_file(
        &workspace.path().join(".agents/skillctl.lock"),
        "  release-notes:",
        "  first:",
    );

    copy_tree(
        &workspace.workspace_import_root("first"),
        &workspace.workspace_import_root("second"),
    );
    duplicate_manifest_import(workspace, "first", "second");
    duplicate_lockfile_import(workspace, "first", "second");
}

fn replace_in_file(path: &Path, needle: &str, replacement: &str) {
    let contents = fs::read_to_string(path).expect("file reads");
    assert!(
        contents.contains(needle),
        "expected '{needle}' in {}",
        path.display()
    );
    fs::write(path, contents.replacen(needle, replacement, 1)).expect("file writes");
}

fn duplicate_manifest_import(workspace: &TestWorkspace, from: &str, to: &str) {
    let path = workspace.path().join(".agents/skillctl.yaml");
    let contents = fs::read_to_string(&path).expect("manifest reads");
    let mut lines = contents.lines().collect::<Vec<_>>();
    let start = lines
        .iter()
        .position(|line| *line == format!("  - id: {from}"))
        .expect("import block starts");
    let end = start
        + 1
        + lines[start + 1..]
            .iter()
            .take_while(|line| line.starts_with("    "))
            .count();
    let block = lines[start..end].to_vec();
    let duplicated: Vec<String> = block
        .iter()
        .map(|line| line.replacen(&format!("id: {from}"), &format!("id: {to}"), 1))
        .collect();
    for (index, line) in duplicated.iter().enumerate() {
        lines.insert(end + index, line.as_str());
    }
    let mut output = lines.join("\n");
    output.push('\n');
    fs::write(&path, output).expect("manifest writes");
}

fn duplicate_lockfile_import(workspace: &TestWorkspace, from: &str, to: &str) {
    let path = workspace.path().join(".agents/skillctl.lock");
    let contents = fs::read_to_string(&path).expect("lockfile reads");
    let mut lines = contents.lines().collect::<Vec<_>>();
    let start = lines
        .iter()
        .position(|line| *line == format!("  {from}:"))
        .expect("lockfile entry starts");
    let end = start
        + 1
        + lines[start + 1..]
            .iter()
            .take_while(|line| line.starts_with("    "))
            .count();
    let block = lines[start..end].to_vec();
    let duplicated: Vec<String> = block
        .iter()
        .map(|line| line.replacen(&format!("  {from}:"), &format!("  {to}:"), 1))
        .collect();
    for (index, line) in duplicated.iter().enumerate() {
        lines.insert(end + index, line.as_str());
    }
    let mut output = lines.join("\n");
    output.push('\n');
    fs::write(&path, output).expect("lockfile writes");
}

fn copy_tree(source: &Path, destination: &Path) {
    fs::create_dir_all(destination).expect("destination directory exists");
    for entry in fs::read_dir(source).expect("source directory reads") {
        let entry = entry.expect("directory entry reads");
        let target = destination.join(entry.file_name());
        if entry.file_type().expect("entry type reads").is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).expect("file copies");
        }
    }
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("permissions update");
}
