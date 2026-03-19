use super::*;

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
            workspace
                .workspace_import_root("release-notes")
                .join(".agents/skills/release-notes/SKILL.md")
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
