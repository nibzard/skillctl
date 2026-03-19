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
    workspace.write_lockfile(MINIMAL_LOCKFILE);
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
    assert!(metadata["generated_at"].is_string());
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
    workspace.write_lockfile(MINIMAL_LOCKFILE);
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
        .stdout(predicate::str::contains("Pruned 1 stale projection"));

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
    workspace.write_lockfile(MINIMAL_LOCKFILE);
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
    workspace.write_lockfile(MINIMAL_LOCKFILE);
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
    assert_eq!(body["data"]["targets"][0]["visible"], true);
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
    workspace.write_lockfile(MINIMAL_LOCKFILE);
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
fn sync_refuses_to_overwrite_hand_authored_runtime_skill_directories() {
    let workspace = TestWorkspace::new();
    workspace.write_manifest(concat!(
        "version: 1\n",
        "\n",
        "targets:\n",
        "  - claude-code\n",
    ));
    workspace.write_lockfile(MINIMAL_LOCKFILE);
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
