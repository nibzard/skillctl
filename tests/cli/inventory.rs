use super::*;

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
            == workspace
                .workspace_import_root("release-notes")
                .display()
                .to_string(),
    );
    assert!(
        path_body["data"]["active_source_root"]
            .as_str()
            .expect("active source root exists")
            == workspace
                .workspace_import_root("release-notes")
                .join(".agents/skills/release-notes")
                .display()
                .to_string(),
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
