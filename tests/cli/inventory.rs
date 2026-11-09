use super::*;

#[test]
fn list_respects_explicit_scope_selection() {
    let workspace = TestWorkspace::new();
    let home_path = workspace.home_path();
    workspace.write_manifest(concat!("version: 1\n", "\n", "targets:\n", "  - codex\n",));
    workspace.write_skill_source("shared-skills", "release-notes");

    initialize_runtime_state(&workspace);

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", &home_path)
        .args([
            "--json",
            "--no-input",
            "--name",
            "release-notes",
            "--scope",
            "workspace",
            "install",
            "shared-skills",
        ])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    let workspace_list = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", &home_path)
        .args(["--json", "--scope", "workspace", "list"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    let workspace_body: Value =
        serde_json::from_slice(&workspace_list.get_output().stdout).expect("stdout is json");

    assert_eq!(
        workspace_body["data"]["skills"]
            .as_array()
            .expect("skills array exists")
            .len(),
        1,
        "workspace list should exclude user-scoped installs: {workspace_body:#?}",
    );
    assert_eq!(
        workspace_body["data"]["skills"][0]["skill"],
        "release-notes"
    );
    assert_eq!(workspace_body["data"]["skills"][0]["scope"], "workspace");

    let user_list = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", &home_path)
        .args(["--json", "--scope", "user", "list"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    let user_body: Value =
        serde_json::from_slice(&user_list.get_output().stdout).expect("stdout is json");

    assert_eq!(
        user_body["data"]["skills"]
            .as_array()
            .expect("skills array exists")
            .len(),
        1,
        "user list should exclude workspace installs: {user_body:#?}",
    );
    assert_eq!(user_body["data"]["skills"][0]["skill"], "skillctl");
    assert_eq!(user_body["data"]["skills"][0]["scope"], "user");
}

#[test]
fn validate_ignores_generated_projection_roots_in_canonical_counts() {
    let workspace = TestWorkspace::new();
    let home_path = workspace.home_path();
    workspace.write_manifest(concat!("version: 1\n", "\n", "targets:\n", "  - codex\n",));
    workspace.write_skill_source("shared-skills", "release-notes");

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", &home_path)
        .args([
            "--json",
            "--no-input",
            "--name",
            "release-notes",
            "--scope",
            "workspace",
            "install",
            "shared-skills",
        ])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    let validate = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", &home_path)
        .args(["--json", "validate"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    let validate_body: Value =
        serde_json::from_slice(&validate.get_output().stdout).expect("stdout is json");

    assert_eq!(
        validate_body["data"]["summary"]["checked_skill_count"], 1,
        "validate should only count the managed import, not its generated codex projection: {validate_body:#?}",
    );

    let doctor = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", &home_path)
        .args(["--json", "--scope", "user", "doctor"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    let doctor_body: Value =
        serde_json::from_slice(&doctor.get_output().stdout).expect("stdout is json");

    assert_eq!(
        doctor_body["data"]["summary"]["checked_skill_count"], 2,
        "doctor should count one workspace import plus the bundled user skill: {doctor_body:#?}",
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
    assert_rfc3339_utc(&install_row.2);

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
    assert_rfc3339_utc(&rollback_row.0);
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

/// `pin` and `rollback` in a workspace that holds a canonical local skill but
/// no manifest.
///
/// Both commands act on an installed import, so the state store answers before
/// the manifest is read. A name no install covers hears the resolution refusal
/// `enable`, `disable`, and `remove` give it, not a raw manifest read error.
#[test]
fn pin_and_rollback_refuse_the_unmanaged_name_in_a_manifest_free_workspace() {
    let workspace = TestWorkspace::new();
    workspace.write_workspace_skill("bare", "Bare skill.", &[]);
    let home = workspace.home_path();

    for args in [
        vec!["pin", "bare", "main"],
        vec!["rollback", "bare", "abc123"],
        vec!["--scope", "workspace", "pin", "bare", "main"],
        vec!["--scope", "user", "rollback", "bare", "abc123"],
    ] {
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
            "{args:?} reports the refusal on stderr, not as a report"
        );
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        assert!(
            stderr.contains("skill 'bare' is not installed"),
            "{args:?} must name the name no install covers: {stderr}"
        );
        assert!(
            !stderr.contains("skillctl.yaml"),
            "{args:?} must not report the manifest at all: {stderr}"
        );
        assert!(
            !stderr.contains("os error"),
            "{args:?} must not surface the raw filesystem error: {stderr}"
        );
    }

    // The refusals leave the bare workspace bare: no manifest or lockfile is
    // created for a command that never ran.
    assert!(
        !workspace.path().join(".agents/skillctl.yaml").exists(),
        "no manifest may appear"
    );
    assert!(
        !workspace.path().join(".agents/skillctl.lock").exists(),
        "no lockfile may appear"
    );
    assert!(
        workspace
            .path()
            .join(".agents/skills/bare/SKILL.md")
            .is_file(),
        "the canonical local skill stays untouched"
    );

    // The machine contract carries the same refusal.
    let output = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", &home)
        .args(["--json", "pin", "bare", "main"])
        .output()
        .expect("command runs");
    assert_eq!(output.status.code(), Some(3), "the JSON run refuses too");
    let body: Value = serde_json::from_slice(&output.stdout).expect("stdout is valid json");
    assert_eq!(
        body["ok"], false,
        "the refusal must not claim success: {body:#?}"
    );
    assert!(
        body["errors"]
            .as_array()
            .expect("errors array exists")
            .iter()
            .any(|error| error
                .as_str()
                .expect("error is a string")
                .contains("skill 'bare' is not installed")),
        "the JSON error must name the unmanaged skill: {body:#?}"
    );
}

/// `pin` and `rollback` in a workspace that holds a real install but lost its
/// manifest.
///
/// The state store resolves the name, so the refusal names the state that
/// actually blocks the command: no declaration exists to rewrite. `skillctl
/// init` and the committed manifest in a team checkout are both named repairs.
#[test]
fn pin_and_rollback_name_init_when_an_install_survives_a_lost_manifest() {
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

    let lockfile_before = fs::read_to_string(workspace.path().join(".agents/skillctl.lock"))
        .expect("the install pinned a lockfile");
    fs::remove_file(workspace.path().join(".agents/skillctl.yaml"))
        .expect("the manifest is removed");

    for args in [
        vec!["pin", "release-notes", "main"],
        vec!["rollback", "release-notes", "abc123"],
    ] {
        let output = Command::cargo_bin("skillctl")
            .expect("binary exists")
            .current_dir(workspace.path())
            .env("HOME", workspace.home_path())
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
            "{args:?} reports the refusal on stderr, not as a report"
        );
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        let command = args[0];
        assert!(
            stderr.contains(".agents/skillctl.yaml"),
            "{command} must name the missing manifest: {stderr}"
        );
        assert!(
            stderr.contains(&format!("'{command}'")),
            "{command} must name the command that needs declared state: {stderr}"
        );
        assert!(
            stderr.contains("skillctl init"),
            "{command} must name the repair: {stderr}"
        );
        assert!(
            !stderr.contains("os error"),
            "{command} must not surface the raw filesystem error: {stderr}"
        );
    }

    // A refused command changes nothing the lockfile pins.
    assert_eq!(
        fs::read_to_string(workspace.path().join(".agents/skillctl.lock"))
            .expect("the lockfile survives"),
        lockfile_before,
        "the refusal must leave the pin exactly as it was"
    );
}

/// `pin` and `rollback` in a workspace that holds a manifest and an install
/// but lost its lockfile.
///
/// Both commands rewrite one lockfile entry, so the missing file is the state
/// that blocks them. The refusal names both repairs: the committed lockfile
/// for a team checkout, `skillctl init` for a workspace that pinned nothing
/// yet. An empty lockfile from `init` would drop the pin instead of repairing
/// it, so the raw read error never surfaces on its own.
#[test]
fn pin_and_rollback_name_both_repairs_when_the_manifest_survives_a_lost_lockfile() {
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

    fs::remove_file(workspace.path().join(".agents/skillctl.lock"))
        .expect("the lockfile is removed");

    for args in [
        vec!["pin", "release-notes", "main"],
        vec!["rollback", "release-notes", "abc123"],
    ] {
        let output = Command::cargo_bin("skillctl")
            .expect("binary exists")
            .current_dir(workspace.path())
            .env("HOME", workspace.home_path())
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
            "{args:?} reports the refusal on stderr, not as a report"
        );
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        let command = args[0];
        assert!(
            stderr.contains(".agents/skillctl.lock"),
            "{command} must name the missing lockfile: {stderr}"
        );
        assert!(
            stderr.contains(&format!("'{command}'")),
            "{command} must name the command that needs pinned state: {stderr}"
        );
        assert!(
            stderr.contains("retrieve the committed .agents/skillctl.lock"),
            "{command} must name the team-checkout repair: {stderr}"
        );
        assert!(
            stderr.contains("skillctl init"),
            "{command} must name the new-workspace repair: {stderr}"
        );
        assert!(
            !stderr.contains("os error"),
            "{command} must not surface the raw filesystem error: {stderr}"
        );
    }

    // The refusal leaves the workspace as it found it: no lockfile appears for
    // commands that never ran.
    assert!(
        !workspace.path().join(".agents/skillctl.lock").exists(),
        "no lockfile may appear"
    );
    assert!(
        workspace.path().join(".agents/skillctl.yaml").is_file(),
        "the manifest survives untouched"
    );

    // The machine contract carries the same refusal.
    let output = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--json", "pin", "release-notes", "main"])
        .output()
        .expect("command runs");
    assert_eq!(output.status.code(), Some(3), "the JSON run refuses too");
    let body: Value = serde_json::from_slice(&output.stdout).expect("stdout is valid json");
    assert_eq!(
        body["ok"], false,
        "the refusal must not claim success: {body:#?}"
    );
    assert!(
        body["errors"]
            .as_array()
            .expect("errors array exists")
            .iter()
            .any(|error| error
                .as_str()
                .expect("error is a string")
                .contains("retrieve the committed .agents/skillctl.lock")),
        "the JSON error must name the team-checkout repair: {body:#?}"
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
        "home/.skillctl/store/imports",
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
    assert_rfc3339_utc(&install_row.2);

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
            == portable(&workspace.workspace_import_root("release-notes")),
    );
    assert!(
        path_body["data"]["active_source_root"]
            .as_str()
            .expect("active source root exists")
            == portable(
                &workspace
                    .workspace_import_root("release-notes")
                    .join(".agents/skills/release-notes")
            ),
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

/// Run one command with `--json` and return its assert and parsed payload.
fn run_json(workspace: &TestWorkspace, args: &[&str]) -> (assert_cmd::assert::Assert, Value) {
    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(args)
        .assert();
    let body: Value =
        serde_json::from_slice(&assert.get_output().stdout).expect("stdout is valid json");
    (assert, body)
}

/// Run one command without `--json` and return its stdout as text.
fn run_human(workspace: &TestWorkspace, args: &[&str]) -> String {
    let output = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(args)
        .assert()
        .success()
        .stderr(predicate::str::is_empty())
        .get_output()
        .stdout
        .clone();
    String::from_utf8(output).expect("human stdout is valid utf-8")
}

#[test]
fn path_reports_a_canonical_local_skill_that_was_never_installed() {
    let workspace = TestWorkspace::new();
    workspace.write_manifest(concat!("version: 1\n", "\n", "targets:\n", "  - codex\n",));
    workspace.write_workspace_skill("bare", "A first-party skill with no install.", &[]);

    initialize_runtime_state(&workspace);

    let (assert, body) = run_json(&workspace, &["--json", "path", "bare"]);
    assert.success().stderr(predicate::str::is_empty());
    assert_eq!(body["command"], "path");
    assert_eq!(
        body["ok"], true,
        "a canonical local skill resolves: {body:#?}"
    );
    assert_eq!(body["data"]["skill"], "bare");
    assert_eq!(body["data"]["scope"], "workspace");
    assert_eq!(body["data"]["managed_import"], false);
    assert_eq!(body["data"]["source"]["type"], "canonical-local");
    assert_eq!(body["data"]["canonical_root"], ".agents/skills/bare");
    assert_eq!(body["data"]["stored_source_root"], Value::Null);
    assert_eq!(body["data"]["active_source_root"], Value::Null);
    assert_eq!(body["data"]["overlay_root"], Value::Null);

    // The neutral plan reuses the canonical root, so every target plans the
    // authored directory itself.
    let planned_roots = body["data"]["planned_roots"]
        .as_array()
        .expect("planned roots exist");
    assert!(
        !planned_roots.is_empty(),
        "a canonical skill plans at least one root: {body:#?}",
    );
    for root in planned_roots {
        assert_eq!(root["path"], ".agents/skills/bare", "{body:#?}");
    }

    let stdout = run_human(&workspace, &["path", "bare"]);
    assert!(
        stdout.contains("Resolved filesystem paths for bare."),
        "path keeps its summary line: {stdout}",
    );
    assert!(
        stdout.contains("  canonical: .agents/skills/bare"),
        "the human output names the canonical root: {stdout}",
    );
    assert!(
        stdout.contains("  source: canonical-local"),
        "the human output names the source class: {stdout}",
    );
    assert!(
        stdout.contains("  planned root (codex): .agents/skills/bare"),
        "the human output names a planned root: {stdout}",
    );

    // `explain` resolves the same skill, so the two advertised debug commands
    // must agree that the name resolves.
    let (assert, explain) = run_json(&workspace, &["--json", "explain", "bare"]);
    assert.success().stderr(predicate::str::is_empty());
    assert_eq!(explain["data"]["status"], "selected", "{explain:#?}");
    assert_eq!(explain["data"]["winner"]["root"], ".agents/skills/bare");
}

#[test]
fn path_refusal_documents_what_it_consulted_and_hands_over_to_explain() {
    let workspace = TestWorkspace::new();
    workspace.write_manifest(concat!("version: 1\n", "\n", "targets:\n", "  - codex\n",));
    initialize_runtime_state(&workspace);

    let (assert, body) = run_json(&workspace, &["--json", "path", "missing"]);
    assert.failure().code(3).stderr(predicate::str::is_empty());
    let message = body["errors"][0]
        .as_str()
        .expect("the refusal carries an error message");
    assert!(
        message.contains("skill 'missing' is not installed"),
        "the refusal names the skill: {message}",
    );
    assert!(
        message.contains("no canonical local skill exists at '.agents/skills/missing'"),
        "the refusal names the canonical root it checked: {message}",
    );
    assert!(
        message.contains("run 'skillctl explain missing'"),
        "the refusal directs the user to explain: {message}",
    );

    // A user-scope selector narrows the request, and canonical local skills
    // belong to the workspace, so the selector keeps its refusal.
    workspace.write_workspace_skill("bare", "A first-party skill with no install.", &[]);
    let (assert, body) = run_json(&workspace, &["--json", "--scope", "user", "path", "bare"]);
    assert.failure().code(3).stderr(predicate::str::is_empty());
    let message = body["errors"][0]
        .as_str()
        .expect("the refusal carries an error message");
    assert!(
        message.contains("not installed at user scope"),
        "the refusal names the selected scope: {message}",
    );
    assert!(
        message.contains("canonical local skills live in the workspace at '.agents/skills'"),
        "the refusal says where canonical local skills live: {message}",
    );
}

#[test]
fn path_does_not_report_a_generated_projection_root_as_a_canonical_skill() {
    let workspace = TestWorkspace::new();
    workspace.write_manifest(concat!("version: 1\n", "\n", "targets:\n", "  - codex\n",));
    workspace.write_workspace_skill("generated", "A generated copy of an import.", &[]);
    workspace.write_file(
        ".agents/skills/generated/.skillctl-projection.json",
        "{\n  \"tool\": \"skillctl\",\n  \"skill_name\": \"generated\"\n}\n",
    );
    initialize_runtime_state(&workspace);

    let (assert, body) = run_json(&workspace, &["--json", "path", "generated"]);
    assert.failure().code(3).stderr(predicate::str::is_empty());
    let message = body["errors"][0]
        .as_str()
        .expect("the refusal carries an error message");
    assert!(
        message.contains("'.agents/skills/generated' is a generated projection"),
        "the refusal says the directory is generated, not canonical: {message}",
    );
    assert!(
        message.contains("run 'skillctl explain generated'"),
        "the refusal directs the user to explain: {message}",
    );
}

#[test]
fn history_reports_an_empty_ledger_for_a_canonical_local_skill() {
    let workspace = TestWorkspace::new();
    workspace.write_manifest(concat!(
        "version: 1\n",
        "\n",
        "targets:\n",
        "  - claude-code\n",
    ));
    workspace.write_workspace_skill("canon", "A first-party skill with no install.", &[]);
    initialize_runtime_state(&workspace);

    run_json(&workspace, &["--json", "sync"])
        .0
        .success()
        .stderr(predicate::str::is_empty());

    // Sync materializes the canonical skill, but the ledger follows managed
    // imports, so it records no event for it.
    assert!(
        workspace
            .path()
            .join(".claude/skills/canon/SKILL.md")
            .is_file(),
        "sync materializes the canonical skill",
    );
    let connection = Connection::open(workspace.home_path().join(".skillctl/state.db"))
        .expect("state database opens");
    let canon_events: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM history_events WHERE skill_id = ?1",
            params!["canon"],
            |row| row.get(0),
        )
        .expect("history count query succeeds");
    assert_eq!(
        canon_events, 0,
        "sync records no ledger event for a canonical local skill",
    );

    let (assert, body) = run_json(&workspace, &["--json", "history", "canon"]);
    assert.success().stderr(predicate::str::is_empty());
    assert_eq!(
        body["ok"], true,
        "a canonical local skill resolves instead of refusing: {body:#?}"
    );
    assert_eq!(body["data"]["skill"], "canon");
    assert_eq!(body["data"]["scope"], "workspace");
    assert_eq!(body["data"]["source"], "canonical-local");
    assert_eq!(body["data"]["canonical_root"], ".agents/skills/canon");
    assert_eq!(
        body["data"]["entries"]
            .as_array()
            .expect("entries exist")
            .len(),
        0,
        "the canonical local report lists no events: {body:#?}",
    );
    let note = body["data"]["note"]
        .as_str()
        .expect("the empty report documents its scope");
    assert!(
        note.contains("managed imports"),
        "the note names the ledger scope: {note}",
    );
    assert!(
        note.contains("run 'skillctl explain canon'"),
        "the note directs the user to explain: {note}",
    );

    let stdout = run_human(&workspace, &["history", "canon"]);
    assert!(
        stdout.contains("No history entries recorded for canon."),
        "history keeps its summary line: {stdout}",
    );
    assert!(
        stdout.contains("  canonical local skill at .agents/skills/canon"),
        "the human output names the canonical root: {stdout}",
    );
    assert!(
        stdout.contains("run 'skillctl explain canon' for its projections"),
        "the human output says why sync records no event: {stdout}",
    );

    // `path` and `explain` resolve the same skill, so all three advertised
    // debug commands agree that the name resolves.
    run_json(&workspace, &["--json", "path", "canon"])
        .0
        .success()
        .stderr(predicate::str::is_empty());
    let (assert, explain) = run_json(&workspace, &["--json", "explain", "canon"]);
    assert.success().stderr(predicate::str::is_empty());
    assert_eq!(explain["data"]["status"], "selected", "{explain:#?}");
}

#[test]
fn history_refusal_documents_the_ledger_scope_and_hands_over_to_explain() {
    let workspace = TestWorkspace::new();
    workspace.write_manifest(concat!("version: 1\n", "\n", "targets:\n", "  - codex\n",));
    initialize_runtime_state(&workspace);

    let (assert, body) = run_json(&workspace, &["--json", "history", "missing"]);
    assert.failure().code(3).stderr(predicate::str::is_empty());
    let message = body["errors"][0]
        .as_str()
        .expect("the refusal carries an error message");
    assert!(
        message.contains("skill 'missing' has no recorded history"),
        "the refusal names the skill: {message}",
    );
    assert!(
        message.contains("no canonical local skill exists at '.agents/skills/missing'"),
        "the refusal names the canonical root it checked: {message}",
    );
    assert!(
        message.contains("the history ledger records lifecycle events for managed imports"),
        "the refusal documents the ledger scope: {message}",
    );
    assert!(
        message.contains("run 'skillctl explain missing'"),
        "the refusal directs the user to explain: {message}",
    );

    // A user-scope selector narrows the request, and canonical local skills
    // belong to the workspace, so the selector keeps its refusal.
    workspace.write_workspace_skill("bare", "A first-party skill with no install.", &[]);
    let (assert, body) = run_json(
        &workspace,
        &["--json", "--scope", "user", "history", "bare"],
    );
    assert.failure().code(3).stderr(predicate::str::is_empty());
    let message = body["errors"][0]
        .as_str()
        .expect("the refusal carries an error message");
    assert!(
        message.contains("has no recorded history at user scope"),
        "the refusal names the selected scope: {message}",
    );
    assert!(
        message.contains("canonical local skills live in the workspace at '.agents/skills'"),
        "the refusal says where canonical local skills live: {message}",
    );
    assert!(
        message.contains("run 'skillctl explain bare'"),
        "the refusal directs the user to explain: {message}",
    );
}

#[test]
fn history_does_not_treat_a_generated_projection_root_as_a_canonical_skill() {
    let workspace = TestWorkspace::new();
    workspace.write_manifest(concat!("version: 1\n", "\n", "targets:\n", "  - codex\n",));
    workspace.write_workspace_skill("generated", "A generated copy of an import.", &[]);
    workspace.write_file(
        ".agents/skills/generated/.skillctl-projection.json",
        "{\n  \"tool\": \"skillctl\",\n  \"skill_name\": \"generated\"\n}\n",
    );
    initialize_runtime_state(&workspace);

    let (assert, body) = run_json(&workspace, &["--json", "history", "generated"]);
    assert.failure().code(3).stderr(predicate::str::is_empty());
    let message = body["errors"][0]
        .as_str()
        .expect("the refusal carries an error message");
    assert!(
        message.contains("'.agents/skills/generated' is a generated projection"),
        "the refusal says the directory is generated, not canonical: {message}",
    );
    assert!(
        message.contains("run 'skillctl explain generated'"),
        "the refusal directs the user to explain: {message}",
    );
}

#[test]
fn history_lists_ledger_events_recorded_for_a_canonical_local_skill() {
    let workspace = TestWorkspace::new();
    workspace.write_manifest(concat!(
        "version: 1\n",
        "\n",
        "targets:\n",
        "  - claude-code\n",
    ));
    workspace.write_workspace_skill("canon", "A first-party skill with no install.", &[]);
    initialize_runtime_state(&workspace);
    run_json(&workspace, &["--json", "sync"])
        .0
        .success()
        .stderr(predicate::str::is_empty());

    // `clean` records a cleanup event that names the canonical skill, so the
    // ledger itself starts covering the name.
    run_json(&workspace, &["--json", "clean"])
        .0
        .success()
        .stderr(predicate::str::is_empty());

    let (assert, body) = run_json(&workspace, &["--json", "history", "canon"]);
    assert.success().stderr(predicate::str::is_empty());
    let entries = body["data"]["entries"]
        .as_array()
        .expect("entries exist")
        .to_vec();
    assert_eq!(
        entries.len(),
        1,
        "the recorded cleanup event is the only canon row: {body:#?}",
    );
    assert_eq!(entries[0]["kind"], "cleanup");
    assert_eq!(entries[0]["skill_id"], "canon");
    assert_eq!(
        body["data"]["source"],
        Value::Null,
        "a covered name reports its ledger rows, not the canonical fallback: {body:#?}",
    );
}
