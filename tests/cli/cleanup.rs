use super::*;

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
        !workspace.workspace_import_root("release-notes").exists(),
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
        "home/.skillctl/store/imports",
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
    let stale_skill_manifest = workspace
        .workspace_import_root_relative("stale-skill")
        .join("SKILL.md");
    workspace.write_file(
        stale_skill_manifest.to_string_lossy().as_ref(),
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
            .any(|entry| entry["path"]
                == workspace
                    .workspace_import_root("stale-skill")
                    .display()
                    .to_string()),
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
        workspace.workspace_import_root("release-notes").is_dir(),
        "active immutable imports should not be deleted by clean",
    );
    assert!(
        !workspace.workspace_import_root("stale-skill").exists(),
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

    let stale_skill_manifest = workspace
        .workspace_import_root_relative("stale-skill")
        .join("SKILL.md");
    workspace.write_file(
        stale_skill_manifest.to_string_lossy().as_ref(),
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
