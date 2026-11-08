use super::*;

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
    workspace.write_lockfile(MINIMAL_LOCKFILE.as_str());
    workspace.write_workspace_skill(
        "release-notes",
        "Summarize release notes.",
        &[("notes.md", "# Notes\n")],
    );

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
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
    assert_rfc3339_timestamp(
        metadata["generated_at"]
            .as_str()
            .expect("generated_at is a string"),
    );
}

/// Assert one timestamp is deterministic UTC RFC3339, the shape the state
/// store enforces and the projection metadata now shares.
fn assert_rfc3339_timestamp(value: &str) {
    let bytes = value.as_bytes();
    assert!(
        bytes.len() == 20
            && bytes[4] == b'-'
            && bytes[7] == b'-'
            && bytes[10] == b'T'
            && bytes[13] == b':'
            && bytes[16] == b':'
            && bytes[19] == b'Z'
            && bytes.iter().enumerate().all(|(index, byte)| {
                matches!(index, 4 | 7 | 10 | 13 | 16 | 19) || byte.is_ascii_digit()
            }),
        "generated_at must be RFC3339 UTC, found '{value}'",
    );
}

#[test]
fn sync_blocks_materialization_when_stored_import_content_is_tampered() {
    let workspace = TestWorkspace::new();
    workspace.write_manifest(concat!("version: 1\n", "\n", "targets:\n", "  - codex\n",));
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
        .stdout(predicate::str::contains("Installed 1 skill"));

    let stored_manifest = find_stored_import_manifest(&workspace.home_path().join(".skillctl"))
        .expect("stored import manifest exists under HOME/.skillctl");
    let original =
        fs::read_to_string(&stored_manifest).expect("stored import manifest is readable");
    fs::write(
        &stored_manifest,
        format!("{original}\ninjected by a tamper test\n"),
    )
    .expect("stored import manifest is writable");

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .arg("sync")
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "stored import 'release-notes' content hash mismatch",
        ));
}

fn find_stored_import_manifest(root: &Path) -> Option<PathBuf> {
    let entries = fs::read_dir(root).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = find_stored_import_manifest(&path) {
                return Some(found);
            }
        } else if path.file_name().and_then(|name| name.to_str()) == Some("SKILL.md") {
            return Some(path);
        }
    }
    None
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
    workspace.write_lockfile(MINIMAL_LOCKFILE.as_str());
    workspace.write_workspace_skill("release-notes", "Summarize release notes.", &[]);

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
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
        .stdout(predicate::str::contains("Pruned 2 stale projections"));

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
    assert!(
        !workspace
            .path()
            .join(".opencode/skills/release-notes")
            .exists(),
        "sync should prune the generated copy left in the root that stopped \
         being planned",
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

    let moved_root_prune_events: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM history_events WHERE skill_id = ?1 AND kind = ?2",
            params!["release-notes", "prune"],
            |row| row.get(0),
        )
        .expect("moved-root prune history query succeeds");
    assert_eq!(
        moved_root_prune_events, 1,
        "sync must record the prune of the copy left in the unplanned root",
    );
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
    workspace.write_lockfile(MINIMAL_LOCKFILE.as_str());
    workspace.write_workspace_skill(
        "release-notes",
        "Summarize release notes.",
        &[("notes.md", "# Notes\n")],
    );

    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
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
    workspace.write_lockfile(MINIMAL_LOCKFILE.as_str());
    workspace.write_workspace_skill("release-notes", "Summarize release notes.", &[]);

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
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
        .env("HOME", workspace.home_path())
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
        "  local_state_version: 2\n",
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
fn doctor_reports_declared_capabilities_for_imported_skills() {
    let workspace = TestWorkspace::new();
    workspace.write_file(
        "shared-skills/.agents/skills/release-notes/SKILL.md",
        concat!(
            "---\n",
            "name: release-notes\n",
            "description: Summarize release notes.\n",
            "capabilities:\n",
            "  - network\n",
            "  - shell\n",
            "---\n",
            "\n",
            "# Release Notes\n",
        ),
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
        .find(|issue| {
            issue["code"] == "declared-capability-risk" && issue["skill"] == "release-notes"
        })
        .expect("declared capability issue exists");

    assert_eq!(issue["safety"]["capabilities"], json!(["network", "shell"]));
}

#[test]
fn doctor_reports_missing_required_credentials_for_active_skills() {
    let workspace = TestWorkspace::new();
    workspace.write_file(
        "shared-skills/.agents/skills/release-notes/SKILL.md",
        concat!(
            "---\n",
            "name: release-notes\n",
            "description: Summarize release notes.\n",
            "credentials:\n",
            "  - name: OPENAI_API_KEY\n",
            "    purpose: Access the OpenAI API\n",
            "  - name: GITHUB_TOKEN\n",
            "    optional: true\n",
            "---\n",
            "\n",
            "# Release Notes\n",
        ),
    );

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .env_remove("OPENAI_API_KEY")
        .env_remove("GITHUB_TOKEN")
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
        .env_remove("OPENAI_API_KEY")
        .env_remove("GITHUB_TOKEN")
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
        .find(|issue| {
            issue["code"] == "missing-required-credential" && issue["skill"] == "release-notes"
        })
        .expect("missing-required-credential issue exists");

    assert_eq!(
        issue["safety"]["credentials"],
        json!([
            {
                "name": "OPENAI_API_KEY",
                "purpose": "Access the OpenAI API"
            },
            {
                "name": "GITHUB_TOKEN",
                "optional": true
            }
        ])
    );
}

#[test]
fn doctor_reports_missing_required_tools_for_active_skills() {
    let workspace = TestWorkspace::new();
    let empty_bin = workspace.path().join("empty-bin");
    fs::create_dir_all(&empty_bin).expect("empty bin exists");
    workspace.write_file(
        "shared-skills/.agents/skills/release-notes/SKILL.md",
        concat!(
            "---\n",
            "name: release-notes\n",
            "description: Summarize release notes.\n",
            "dependencies:\n",
            "  tools:\n",
            "    - ffmpeg\n",
            "    - name: gh\n",
            "      optional: true\n",
            "---\n",
            "\n",
            "# Release Notes\n",
        ),
    );

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .env("PATH", &empty_bin)
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
        .env("PATH", &empty_bin)
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
        .find(|issue| issue["code"] == "missing-required-tool" && issue["skill"] == "release-notes")
        .expect("missing-required-tool issue exists");

    assert_eq!(
        issue["safety"]["dependencies"]["tools"],
        json!([
            {
                "name": "ffmpeg"
            },
            {
                "name": "gh",
                "optional": true
            }
        ])
    );
}

#[test]
fn inspect_reports_preflight_summary_for_active_skills() {
    let workspace = TestWorkspace::new();
    let empty_bin = workspace.path().join("empty-bin");
    fs::create_dir_all(&empty_bin).expect("empty bin exists");
    workspace.write_manifest(concat!(
        "version: 1\n",
        "\n",
        "targets:\n",
        "  - claude-code\n",
    ));
    workspace.write_file(
        "shared-skills/.agents/skills/release-notes/SKILL.md",
        concat!(
            "---\n",
            "name: release-notes\n",
            "description: Summarize release notes.\n",
            "capabilities:\n",
            "  - network\n",
            "credentials:\n",
            "  - name: OPENAI_API_KEY\n",
            "dependencies:\n",
            "  tools:\n",
            "    - ffmpeg\n",
            "---\n",
            "\n",
            "# Release Notes\n",
        ),
    );

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .env("PATH", &empty_bin)
        .env_remove("OPENAI_API_KEY")
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
        .env("PATH", &empty_bin)
        .env_remove("OPENAI_API_KEY")
        .args(["--json", "inspect", "release-notes"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    let body: Value =
        serde_json::from_slice(&assert.get_output().stdout).expect("stdout is valid json");

    assert_eq!(body["command"], "inspect");
    assert_eq!(body["ok"], true);
    assert_eq!(body["data"]["skill"], "release-notes");
    assert_eq!(body["data"]["status"], "selected");
    assert_eq!(body["data"]["candidate"]["source_class"], "imported");
    assert_eq!(body["data"]["trust"]["review_required"], true);
    assert_eq!(body["data"]["safety"]["capabilities"], json!(["network"]));
    assert_eq!(
        body["data"]["missing_credentials"],
        json!(["OPENAI_API_KEY"])
    );
    assert_eq!(body["data"]["missing_tools"], json!(["ffmpeg"]));
    assert_eq!(body["data"]["targets"][0]["target"], "claude-code");
    assert_eq!(body["data"]["targets"][0]["visible"], true);
}

#[test]
fn inspect_reports_missing_status_for_unknown_skills() {
    let workspace = TestWorkspace::new();

    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--json", "inspect", "missing-skill"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    let body: Value =
        serde_json::from_slice(&assert.get_output().stdout).expect("stdout is valid json");

    assert_eq!(body["command"], "inspect");
    assert_eq!(body["ok"], true);
    assert_eq!(body["data"]["skill"], "missing-skill");
    assert_eq!(body["data"]["status"], "missing");
    assert_eq!(body["data"]["issues"], json!([]));
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
    // The projection still holds the imported copy, so observed state must not
    // report the new canonical winner as installed for the target.
    assert_eq!(body["data"]["targets"][0]["visible"], false);
    assert_eq!(body["data"]["targets"][0]["observed_presence"], "present");
    assert_eq!(body["data"]["targets"][0]["observed_matches_winner"], false);
    assert_ne!(
        body["data"]["targets"][0]["observed_digest"],
        body["data"]["targets"][0]["expected_digest"]
    );
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
    workspace.write_lockfile(MINIMAL_LOCKFILE.as_str());
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
        .env("HOME", workspace.home_path())
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
fn sync_prunes_stale_generated_projections_in_the_reused_canonical_root() {
    let workspace = TestWorkspace::new();
    workspace.write_manifest(concat!(
        "version: 1\n",
        "\n",
        "targets:\n",
        "  - codex\n",
        "  - gemini-cli\n",
        "  - opencode\n",
    ));
    workspace.write_lockfile(MINIMAL_LOCKFILE.as_str());
    workspace.write_workspace_skill("release-notes", "Summarize release notes.", &[]);
    workspace.write_file(
        ".agents/skills/stale-skill/SKILL.md",
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
        ".agents/skills/stale-skill/.skillctl-projection.json",
        concat!(
            "{\n",
            "  \"tool\": \"skillctl\",\n",
            "  \"generation_mode\": \"copy\",\n",
            "  \"physical_root\": \".agents/skills\"\n",
            "}\n"
        ),
    );

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .arg("sync")
        .assert()
        .success()
        .stderr(predicate::str::is_empty())
        .stdout(predicate::str::contains("Pruned 1 stale projection"))
        .stdout(predicate::str::contains(
            "Reused canonical root: .agents/skills",
        ));

    assert!(
        !workspace.path().join(".agents/skills/stale-skill").exists(),
        "sync must prune stale generated copies from the reused canonical root",
    );
    assert!(
        workspace
            .path()
            .join(".agents/skills/release-notes/SKILL.md")
            .is_file(),
        "sync must keep canonical local skills in the reused canonical root",
    );
    assert!(
        !workspace
            .path()
            .join(".agents/skills/release-notes/.skillctl-projection.json")
            .exists(),
        "canonical local skills must not receive generated metadata",
    );
}

#[test]
fn remove_prunes_generated_projection_left_in_the_reused_canonical_root() {
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

    assert!(
        workspace
            .path()
            .join(".agents/skills/delta/.skillctl-projection.json")
            .is_file(),
        "install should leave a generated copy in the reused canonical root",
    );

    let remove_assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--json", "remove", "delta"])
        .assert()
        // Exit code 2 is success with warnings: removal retains the released
        // artifact bundle for recovery.
        .code(2)
        .stderr(predicate::str::is_empty());
    let remove_body: Value = serde_json::from_slice(&remove_assert.get_output().stdout)
        .expect("remove stdout is valid json");
    assert_eq!(remove_body["ok"], true, "remove body: {remove_body:#?}");

    assert!(
        !workspace.path().join(".agents/skills/delta").exists(),
        "remove must delete the generated copy left in the reused canonical root",
    );

    let connection = Connection::open(workspace.home_path().join(".skillctl/state.db"))
        .expect("state database opens");
    let prune_events: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM history_events WHERE skill_id = ?1 AND kind = ?2",
            params!["delta", "prune"],
            |row| row.get(0),
        )
        .expect("prune history query succeeds");
    assert_eq!(
        prune_events, 1,
        "remove must record the canonical-root prune in history",
    );
}

#[test]
fn disable_prunes_generated_projection_left_in_the_reused_canonical_root() {
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

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .arg("disable")
        .arg("delta")
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    assert!(
        !workspace.path().join(".agents/skills/delta").exists(),
        "disable must prune the generated copy left in the reused canonical root",
    );

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .arg("enable")
        .arg("delta")
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    assert!(
        workspace
            .path()
            .join(".agents/skills/delta/SKILL.md")
            .is_file(),
        "enable must materialize the projection in the reused canonical root again",
    );
}

/// Replace the manifest target list while preserving the import and override
/// blocks. Symlink mode also moves the acknowledgement to the new target.
fn switch_manifest_targets(workspace: &TestWorkspace, replacement: &str, symlink: bool) {
    let manifest_path = workspace.path().join(".agents/skillctl.yaml");
    let manifest = fs::read_to_string(&manifest_path).expect("manifest exists");

    let preserved_blocks = ["imports:", "overrides:"];
    let mut preserved = String::new();
    let mut preserving = false;
    for line in manifest.lines() {
        if preserving {
            if line.starts_with(' ') || line.is_empty() {
                preserved.push_str(line);
                preserved.push('\n');
                continue;
            }
            preserving = false;
        }
        if preserved_blocks.contains(&line) {
            preserving = true;
            preserved.push('\n');
            preserved.push_str(line);
            preserved.push('\n');
        }
    }

    let mut switched = String::from("version: 1\n");
    switched.push_str(&preserved);
    if symlink {
        switched.push_str(&format!(
            "\nprojection:\n  mode: symlink\n  allow_unsafe_targets:\n    - {replacement}\n",
        ));
    }
    switched.push_str(&format!("\ntargets:\n  - {replacement}\n"));
    fs::write(&manifest_path, switched).expect("manifest target update");
}

#[test]
fn sync_prunes_generated_copies_from_roots_that_stopped_being_planned() {
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

    assert!(
        workspace
            .path()
            .join(".agents/skills/delta/.skillctl-projection.json")
            .is_file(),
        "install should leave a generated copy in the reused canonical root",
    );

    switch_manifest_targets(&workspace, "claude-code", false);

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .arg("sync")
        .assert()
        .success()
        .stderr(predicate::str::is_empty())
        .stdout(predicate::str::contains(
            "Materialized 1 generated projection",
        ))
        .stdout(predicate::str::contains("Pruned 1 stale projection"));

    assert!(
        !workspace.path().join(".agents/skills/delta").exists(),
        "sync must prune the generated copy from the root that stopped being planned",
    );
    assert!(
        workspace
            .path()
            .join(".claude/skills/delta/SKILL.md")
            .is_file(),
        "sync must materialize the import into the newly planned root",
    );

    let connection = Connection::open(workspace.home_path().join(".skillctl/state.db"))
        .expect("state database opens");
    let prune_events: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM history_events WHERE skill_id = ?1 AND kind = ?2",
            params!["delta", "prune"],
            |row| row.get(0),
        )
        .expect("prune history query succeeds");
    assert_eq!(
        prune_events, 1,
        "sync must record the unplanned-root prune in history",
    );
}

#[test]
fn sync_prunes_generated_copies_from_unplanned_user_roots_but_keeps_the_bundled_skill() {
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
        .args([
            "--scope",
            "user",
            "--no-input",
            "--name",
            "delta",
            "install",
            "shared-skills",
        ])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    let user_neutral_root = workspace.home_path().join(".agents/skills");
    assert!(
        user_neutral_root
            .join("delta/.skillctl-projection.json")
            .is_file(),
        "install should leave a generated copy in the user neutral root",
    );
    assert!(
        user_neutral_root
            .join("skillctl/.skillctl-projection.json")
            .is_file(),
        "the bundled skill should be projected into the user neutral root",
    );

    switch_manifest_targets(&workspace, "claude-code", false);

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--scope", "user", "sync"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty())
        .stdout(predicate::str::contains("Pruned 1 stale projection"));

    assert!(
        !user_neutral_root.join("delta").exists(),
        "user sync must prune the import copy from the unplanned user root",
    );
    assert!(
        workspace
            .home_path()
            .join(".claude/skills/delta/SKILL.md")
            .is_file(),
        "user sync must materialize the import into the newly planned user root",
    );
    assert!(
        user_neutral_root.join("skillctl/SKILL.md").is_file(),
        "the bundled skill must stay in the unplanned user root",
    );
}

#[test]
fn sync_prunes_generated_copies_from_unplanned_roots_in_symlink_mode() {
    let workspace = TestWorkspace::new();
    workspace.write_manifest(concat!(
        "version: 1\n",
        "\n",
        "projection:\n",
        "  mode: symlink\n",
        "  allow_unsafe_targets:\n",
        "    - codex\n",
        "    - gemini-cli\n",
        "    - opencode\n",
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
        // Exit code 2 is success with warnings: symlink-mode risk notices.
        .code(2)
        .stderr(predicate::str::is_empty());

    assert!(
        fs::symlink_metadata(workspace.path().join(".agents/skills/delta/SKILL.md"))
            .expect("symlinked projection exists")
            .file_type()
            .is_symlink(),
        "install should project the import through symlinks",
    );

    switch_manifest_targets(&workspace, "claude-code", true);

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .arg("sync")
        .assert()
        .code(2)
        .stderr(predicate::str::is_empty())
        .stdout(predicate::str::contains("Pruned 1 stale projection"));

    assert!(
        !workspace.path().join(".agents/skills/delta").exists(),
        "sync must prune the symlinked copy from the unplanned root",
    );
    assert!(
        fs::symlink_metadata(workspace.path().join(".claude/skills/delta/SKILL.md"))
            .expect("new projection exists")
            .file_type()
            .is_symlink(),
        "sync must materialize the import into the newly planned root in symlink mode",
    );
}

#[test]
fn sync_skips_unplanned_documented_roots_that_are_not_directories() {
    let workspace = TestWorkspace::new();
    workspace.write_manifest(concat!(
        "version: 1\n",
        "\n",
        "targets:\n",
        "  - claude-code\n",
    ));
    workspace.write_lockfile(MINIMAL_LOCKFILE.as_str());
    workspace.write_workspace_skill("release-notes", "Summarize release notes.", &[]);
    // A stray file at a documented root path must not fail the prune scan.
    workspace.write_file(".github/skills", "not a directory");

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .arg("sync")
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    assert!(
        workspace.path().join(".github/skills").is_file(),
        "sync must leave the stray file untouched",
    );
    assert!(
        workspace
            .path()
            .join(".claude/skills/release-notes/SKILL.md")
            .is_file(),
        "sync must still materialize the planned root",
    );
}

#[test]
fn sync_preserves_edits_and_hand_authored_directories_in_unplanned_roots() {
    let workspace = TestWorkspace::new();
    workspace.write_manifest(concat!(
        "version: 1\n",
        "\n",
        "targets:\n",
        "  - claude-code\n",
    ));
    workspace.write_lockfile(MINIMAL_LOCKFILE.as_str());
    workspace.write_workspace_skill("release-notes", "Summarize release notes.", &[]);
    workspace.write_file(
        ".agents/skills/hand-made/SKILL.md",
        concat!(
            "---\n",
            "name: hand-made\n",
            "description: Hand-authored runtime skill.\n",
            "---\n",
            "\n",
            "# Hand made\n"
        ),
    );
    workspace.write_file(
        ".agents/skills/edited-copy/SKILL.md",
        concat!(
            "---\n",
            "name: edited-copy\n",
            "description: Generated copy with local edits.\n",
            "---\n",
            "\n",
            "# Edited\n"
        ),
    );
    workspace.write_file(
        ".agents/skills/edited-copy/.skillctl-projection.json",
        concat!(
            "{\n",
            "  \"tool\": \"skillctl\",\n",
            "  \"generation_mode\": \"copy\",\n",
            "  \"physical_root\": \".agents/skills\",\n",
            "  \"content_digest\": \"sha256:0000000000000000000000000000000000000000000000000000000000000000\"\n",
            "}\n"
        ),
    );

    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .arg("sync")
        .assert()
        // Exit code 2 is success with warnings: the preserved copy is one.
        .code(2)
        .stderr(predicate::str::is_empty());

    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    assert!(
        stdout.contains("preserved '.agents/skills/edited-copy'"),
        "sync must report the preserved edited copy in the unplanned root: {stdout}",
    );

    assert!(
        workspace
            .path()
            .join(".agents/skills/hand-made/SKILL.md")
            .is_file(),
        "hand-authored directories in unplanned roots must stay untouched",
    );
    assert!(
        workspace
            .path()
            .join(".agents/skills/edited-copy/SKILL.md")
            .is_file(),
        "generated copies with unrecorded edits must be preserved",
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
    workspace.write_lockfile(MINIMAL_LOCKFILE.as_str());
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
        .env("HOME", workspace.home_path())
        .arg("sync")
        .assert()
        .failure()
        .code(3)
        .stderr(predicate::str::contains(
            "refusing to overwrite hand-authored runtime skill directory",
        ));
}

/// Paths a workspace sync may change, plus the local state database.
const IDEMPOTENCE_SNAPSHOT_PATHS: &[&str] = &[
    ".agents",
    ".claude",
    ".gemini",
    ".opencode",
    ".github",
    "home/.skillctl/state.db",
];

fn history_event_count(workspace: &TestWorkspace, skill: &str) -> i64 {
    let connection = Connection::open(workspace.home_path().join(".skillctl/state.db"))
        .expect("state database opens");
    connection
        .query_row(
            "SELECT COUNT(*) FROM history_events WHERE skill_id = ?1",
            params![skill],
            |row| row.get(0),
        )
        .expect("history count query succeeds")
}

#[test]
fn sync_without_changes_leaves_the_working_tree_and_history_untouched() {
    let workspace = TestWorkspace::new();
    workspace.write_manifest(concat!(
        "version: 1\n",
        "\n",
        "targets:\n",
        "  - codex\n",
        "  - gemini-cli\n",
        "  - opencode\n",
        "  - claude-code\n",
    ));
    workspace.write_skill_source("git-source", "delta");
    workspace.init_git_repo("git-source");

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .env("SOURCE_DATE_EPOCH", "1770000000")
        .args([
            "--no-input",
            "--name",
            "delta",
            "install",
            workspace.git_repo_url("git-source").as_str(),
        ])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    assert!(
        workspace
            .path()
            .join(".agents/skills/delta/.skillctl-projection.json")
            .is_file(),
        "install should leave a generated copy in the reused canonical root",
    );

    let before = workspace.snapshot_paths(IDEMPOTENCE_SNAPSHOT_PATHS);
    let history_before = history_event_count(&workspace, "delta");

    // A second run on a different clock must change nothing.
    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .env("SOURCE_DATE_EPOCH", "1770000123")
        .arg("sync")
        .assert()
        .success()
        .stderr(predicate::str::is_empty())
        .stdout(predicate::str::contains(
            "No generated projections were required",
        ));

    assert_eq!(
        workspace.snapshot_paths(IDEMPOTENCE_SNAPSHOT_PATHS),
        before,
        "an unchanged repo must stay byte-identical across sync runs",
    );
    assert_eq!(
        history_event_count(&workspace, "delta"),
        history_before,
        "an unchanged root must not append projection history events",
    );
}

#[test]
fn sync_reports_how_many_generated_projections_already_matched_the_plan() {
    let workspace = TestWorkspace::new();
    workspace.write_manifest(concat!(
        "version: 1\n",
        "\n",
        "targets:\n",
        "  - codex\n",
        "  - gemini-cli\n",
        "  - opencode\n",
        "  - claude-code\n",
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
        .args(["--json", "sync"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    let body: Value =
        serde_json::from_slice(&assert.get_output().stdout).expect("stdout is valid json");

    assert_eq!(body["data"]["materialized_skills"], 0);
    assert_eq!(body["data"]["unchanged_skills"], 2);
    assert_eq!(
        body["data"]["generated_roots"][0]["materialized"],
        json!([])
    );
    assert_eq!(
        body["data"]["generated_roots"][0]["unchanged"],
        json!(["delta"])
    );

    let summary = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .arg("sync")
        .assert()
        .success()
        .stderr(predicate::str::is_empty())
        .get_output()
        .stdout
        .clone();
    let summary = String::from_utf8_lossy(&summary).into_owned();
    assert!(
        summary.contains("No generated projections were required."),
        "unexpected summary: {summary}",
    );
    assert!(
        summary.contains("2 generated projections already matched the plan"),
        "unexpected summary: {summary}",
    );
}

#[test]
fn sync_rematerializes_and_records_history_again_after_the_canonical_skill_changes() {
    let workspace = TestWorkspace::new();
    workspace.write_manifest(concat!(
        "version: 1\n",
        "\n",
        "targets:\n",
        "  - codex\n",
        "  - claude-code\n",
    ));
    workspace.write_lockfile(MINIMAL_LOCKFILE.as_str());
    workspace.write_workspace_skill("release-notes", "Summarize release notes.", &[]);

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .env("SOURCE_DATE_EPOCH", "1770000000")
        .arg("sync")
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    // The native root receives the generated copy, and an edit to the
    // canonical skill must rewrite it with a fresh generation timestamp.
    assert!(
        workspace
            .path()
            .join(".claude/skills/release-notes/SKILL.md")
            .is_file(),
        "the native root should receive a generated copy",
    );

    workspace.write_workspace_skill(
        "release-notes",
        "Summarize release notes with edits.",
        &[("notes.md", "# Notes\n")],
    );

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .env("SOURCE_DATE_EPOCH", "1770000456")
        .arg("sync")
        .assert()
        .success()
        .stderr(predicate::str::is_empty())
        .stdout(predicate::str::contains(
            "Materialized 1 generated projection",
        ));

    let metadata: Value = serde_json::from_str(
        &fs::read_to_string(
            workspace
                .path()
                .join(".claude/skills/release-notes/.skillctl-projection.json"),
        )
        .expect("projection metadata exists"),
    )
    .expect("projection metadata is valid json");
    assert_eq!(metadata["generated_at"], "2026-02-02T02:47:36Z");
}

/// Rewrite selected fields in one projection metadata file, the way an older
/// release would have written them.
fn rewrite_metadata_fields(workspace: &TestWorkspace, relative_dir: &str, fields: &[(&str, &str)]) {
    let metadata_path = workspace
        .path()
        .join(relative_dir)
        .join(".skillctl-projection.json");
    let mut metadata: Value = serde_json::from_str(
        &fs::read_to_string(&metadata_path).expect("projection metadata exists"),
    )
    .expect("projection metadata is valid json");
    let object = metadata
        .as_object_mut()
        .expect("projection metadata is an object");
    for (field, value) in fields {
        object.insert((*field).to_string(), Value::String((*value).to_string()));
    }
    fs::write(
        &metadata_path,
        serde_json::to_string_pretty(&metadata).expect("projection metadata serializes"),
    )
    .expect("projection metadata is written");
}

fn recorded_metadata_field(workspace: &TestWorkspace, relative_dir: &str, field: &str) -> String {
    let metadata: Value = serde_json::from_str(
        &fs::read_to_string(
            workspace
                .path()
                .join(relative_dir)
                .join(".skillctl-projection.json"),
        )
        .expect("projection metadata exists"),
    )
    .expect("projection metadata is valid json");
    metadata[field]
        .as_str()
        .unwrap_or_else(|| panic!("{field} is a string"))
        .to_string()
}

#[test]
fn sync_keeps_a_projection_current_whose_metadata_stores_epoch_seconds() {
    let workspace = TestWorkspace::new();
    workspace.write_manifest(concat!(
        "version: 1\n",
        "\n",
        "targets:\n",
        "  - codex\n",
        "  - claude-code\n",
    ));
    workspace.write_lockfile(MINIMAL_LOCKFILE.as_str());
    workspace.write_workspace_skill("release-notes", "Summarize release notes.", &[]);

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .env("SOURCE_DATE_EPOCH", "1770000000")
        .arg("sync")
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    // Stand in for a projection committed by an older release, which stored
    // unix seconds instead of RFC3339.
    rewrite_metadata_fields(
        &workspace,
        ".claude/skills/release-notes",
        &[("generated_at", "1770000000")],
    );

    let before = workspace.snapshot_paths(IDEMPOTENCE_SNAPSHOT_PATHS);
    let history_before = history_event_count(&workspace, "release-notes");

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .env("SOURCE_DATE_EPOCH", "1770000456")
        .arg("sync")
        .assert()
        .success()
        .stderr(predicate::str::is_empty())
        .stdout(predicate::str::contains(
            "No generated projections were required",
        ));

    assert_eq!(
        workspace.snapshot_paths(IDEMPOTENCE_SNAPSHOT_PATHS),
        before,
        "a legacy generated_at format alone must not rewrite a committed projection",
    );
    assert_eq!(
        history_event_count(&workspace, "release-notes"),
        history_before,
        "a current projection must not append projection history events",
    );
    assert_eq!(
        recorded_metadata_field(&workspace, ".claude/skills/release-notes", "generated_at"),
        "1770000000",
        "the legacy timestamp must survive the run that accepted it",
    );
}

#[test]
fn sync_keeps_a_committed_projection_current_across_a_tool_version_bump() {
    let workspace = TestWorkspace::new();
    workspace.write_manifest(concat!(
        "version: 1\n",
        "\n",
        "targets:\n",
        "  - codex\n",
        "  - claude-code\n",
    ));
    workspace.write_lockfile(MINIMAL_LOCKFILE.as_str());
    workspace.write_workspace_skill("release-notes", "Summarize release notes.", &[]);

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .env("SOURCE_DATE_EPOCH", "1770000000")
        .arg("sync")
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    // Stand in for a projection committed by the previous release: it records
    // that release's version and a unix-seconds timestamp.
    rewrite_metadata_fields(
        &workspace,
        ".claude/skills/release-notes",
        &[("version", "0.0.9"), ("generated_at", "1770000000")],
    );

    let before = workspace.snapshot_paths(IDEMPOTENCE_SNAPSHOT_PATHS);
    let history_before = history_event_count(&workspace, "release-notes");

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .env("SOURCE_DATE_EPOCH", "1770000456")
        .arg("sync")
        .assert()
        .success()
        .stderr(predicate::str::is_empty())
        .stdout(predicate::str::contains(
            "No generated projections were required",
        ));

    assert_eq!(
        workspace.snapshot_paths(IDEMPOTENCE_SNAPSHOT_PATHS),
        before,
        "a tool version bump alone must not rewrite a committed projection",
    );
    assert_eq!(
        history_event_count(&workspace, "release-notes"),
        history_before,
        "a current projection must not append projection history events",
    );
    assert_eq!(
        recorded_metadata_field(&workspace, ".claude/skills/release-notes", "version"),
        "0.0.9",
        "the metadata keeps recording the build that wrote the copy",
    );
    assert_eq!(
        recorded_metadata_field(&workspace, ".claude/skills/release-notes", "generated_at"),
        "1770000000",
        "the committed timestamp must survive the run that accepted it",
    );
}

#[test]
fn sync_without_changes_is_idempotent_in_symlink_mode() {
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
    workspace.write_lockfile(MINIMAL_LOCKFILE.as_str());
    workspace.write_workspace_skill(
        "release-notes",
        "Summarize release notes.",
        &[("notes.md", "# Notes\n")],
    );

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .env("SOURCE_DATE_EPOCH", "1770000000")
        .arg("sync")
        .assert()
        // Exit code 2 is success with warnings: symlink-mode risk notices.
        .code(2)
        .stderr(predicate::str::is_empty());

    let before = workspace.snapshot_paths(&[".claude"]);

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .env("SOURCE_DATE_EPOCH", "1770000123")
        .arg("sync")
        .assert()
        .code(2)
        .stderr(predicate::str::is_empty())
        .stdout(predicate::str::contains(
            "No generated projections were required",
        ));

    assert_eq!(
        workspace.snapshot_paths(&[".claude"]),
        before,
        "symlinked copies that match the plan must stay untouched",
    );
}

/// Run skillctl as JSON in a workspace and return the response body.
fn run_json_body(workspace: &TestWorkspace, args: &[&str]) -> Value {
    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(args)
        .assert()
        .success()
        .stderr(predicate::str::is_empty())
        .stdout(predicate::str::is_empty().not());
    serde_json::from_slice(&assert.get_output().stdout).expect("stdout is valid json")
}

/// Rewrite the lockfile the way a build before the upstream fix wrote it for a
/// commit pin, recording the moved-to commit as an observed upstream revision.
fn reinstate_legacy_upstream_record(workspace: &TestWorkspace, revision: &str) {
    let lockfile = fs::read_to_string(workspace.path().join(".agents/skillctl.lock"))
        .expect("lockfile exists");
    let legacy = lockfile.replace(
        &format!("      resolved: {revision}\n"),
        &format!("      resolved: {revision}\n      upstream: {revision}\n"),
    );
    assert_ne!(
        legacy, lockfile,
        "the lockfile must record the resolved revision to carry the legacy value",
    );
    workspace.write_lockfile(&legacy);
}

/// Rewrite one generated projection metadata document the way a build before
/// the upstream fix wrote it for a commit pin, reporting the pinned commit as
/// an observed upstream revision.
fn reinstate_legacy_upstream_metadata(
    workspace: &TestWorkspace,
    relative_path: &str,
    revision: &str,
) {
    let mut metadata = projection_metadata(workspace, relative_path);
    metadata["source"]["upstream_revision"] = Value::String(revision.to_string());
    let serialized = serde_json::to_string_pretty(&metadata).expect("metadata serializes");
    workspace.write_file(relative_path, &format!("{serialized}\n"));
}

/// Read one generated projection metadata document.
fn projection_metadata(workspace: &TestWorkspace, relative_path: &str) -> Value {
    serde_json::from_str(
        &fs::read_to_string(workspace.path().join(relative_path))
            .expect("projection metadata exists"),
    )
    .expect("projection metadata is valid json")
}

#[test]
fn sync_and_explain_withhold_a_legacy_upstream_record_from_a_commit_pinned_import() {
    let workspace = TestWorkspace::new();
    workspace.write_skill_source("git-source", "delta");
    workspace.init_git_repo("git-source");
    let repo_url = workspace.git_repo_url("git-source");

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args([
            "--no-input",
            "--name",
            "delta",
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
        .args(["--json", "pin", "delta", pinned_commit.as_str()])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    assert!(
        workspace.read_lockfile_yaml()["imports"]["delta"]["revision"]["upstream"]
            .as_str()
            .is_none(),
        "a commit pin records no upstream observation",
    );

    reinstate_legacy_upstream_record(&workspace, &pinned_commit);
    reinstate_legacy_upstream_metadata(
        &workspace,
        ".agents/skills/delta/.skillctl-projection.json",
        &pinned_commit,
    );

    let sync_body = run_json_body(&workspace, &["--json", "sync"]);
    assert_eq!(
        sync_body["data"]["materialized_skills"], 1,
        "a commit-pinned copy whose recorded upstream stops being reported \
         re-materializes once",
    );

    let metadata =
        projection_metadata(&workspace, ".agents/skills/delta/.skillctl-projection.json");
    assert_eq!(metadata["source"]["requested_ref"], pinned_commit.as_str());
    assert_eq!(
        metadata["source"]["resolved_revision"],
        pinned_commit.as_str()
    );
    assert!(
        metadata["source"]["upstream_revision"].is_null(),
        "the recorded value must not be presented as an observation: {metadata:#?}",
    );

    let sync_body = run_json_body(&workspace, &["--json", "sync"]);
    assert_eq!(
        sync_body["data"]["materialized_skills"], 0,
        "the rewritten copy is current again, so it re-materializes exactly once",
    );

    let explain_body = run_json_body(&workspace, &["--json", "explain", "delta"]);
    let explain_text = explain_body["data"].to_string();
    assert!(
        !explain_text.contains("upstream_revision"),
        "explain must not report an upstream revision for a commit pin: {explain_text:#?}",
    );

    // A movable reference keeps reporting what an operation observed, so the
    // withheld value is scoped to references that name one immutable revision.
    workspace.write_skill_source_at(
        "git-source",
        ".agents/skills/delta",
        "delta",
        "Updated upstream delta helper.",
    );
    workspace.commit_all("git-source", "advance delta");
    let latest_commit = workspace.git_head("git-source");
    assert_ne!(latest_commit, pinned_commit);

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--json", "pin", "delta", "main"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    run_json_body(&workspace, &["--json", "sync"]);

    let metadata =
        projection_metadata(&workspace, ".agents/skills/delta/.skillctl-projection.json");
    assert_eq!(metadata["source"]["requested_ref"], "main");
    assert_eq!(
        metadata["source"]["resolved_revision"],
        latest_commit.as_str(),
        "a branch pin follows the branch tip",
    );
    assert_eq!(
        metadata["source"]["upstream_revision"],
        latest_commit.as_str(),
        "a branch pin reports the revision it observed",
    );
}

#[test]
fn sync_runs_on_the_documented_defaults_and_names_init_in_a_manifest_free_workspace() {
    let workspace = TestWorkspace::new();
    workspace.write_workspace_skill("bare", "Bare skill.", &[]);

    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .arg("sync")
        .assert()
        .code(2)
        .stderr(predicate::str::is_empty());
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout);
    assert!(
        stdout.contains("Reused canonical root: .agents/skills"),
        "the documented default targets keep reading the canonical root: {stdout}",
    );
    assert!(
        stdout.contains("warning:"),
        "sync must say that it ran on defaults: {stdout}",
    );
    assert!(
        stdout.contains("skillctl init"),
        "the warning must name the repair: {stdout}",
    );

    assert!(
        !workspace.path().join(".agents/skillctl.yaml").exists(),
        "a manifest-free sync applies defaults; it must not write the manifest",
    );
    assert!(
        !workspace
            .path()
            .join(".agents/skills/bare/.skillctl-projection.json")
            .exists(),
        "the canonical authoring directory must not receive generated metadata",
    );

    let json_assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--json", "sync"])
        .assert()
        .code(2)
        .stderr(predicate::str::is_empty());
    let body: Value =
        serde_json::from_slice(&json_assert.get_output().stdout).expect("stdout is valid json");
    assert_eq!(body["ok"], true, "sync on defaults must stay ok: {body:#?}");
    assert!(
        body["warnings"]
            .as_array()
            .expect("warnings array exists")
            .iter()
            .any(|warning| warning
                .as_str()
                .expect("warning text")
                .contains("skillctl init")),
        "the machine contract must carry the missing-manifest warning: {body:#?}",
    );

    let explain_assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--json", "explain", "bare"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    let explain_body: Value =
        serde_json::from_slice(&explain_assert.get_output().stdout).expect("stdout is valid json");
    assert_eq!(
        explain_body["data"]["targets"]
            .as_array()
            .expect("targets array exists")
            .len(),
        3,
        "the defaults must match the targets skillctl init writes: {explain_body:#?}",
    );
}

#[test]
fn sync_still_fails_on_a_present_but_invalid_manifest() {
    let workspace = TestWorkspace::new();
    workspace.write_workspace_skill("bare", "Bare skill.", &[]);
    workspace.write_manifest(concat!(
        "version: 1\n",
        "\n",
        "targets:\n",
        "  - not-a-runtime\n",
    ));

    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .arg("sync")
        .assert()
        .failure()
        .code(3);

    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
    assert!(
        stderr.contains(".agents/skillctl.yaml"),
        "the refusal must name the manifest that failed validation: {stderr}",
    );
}
