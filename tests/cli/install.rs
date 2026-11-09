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
fn install_accepts_archives_with_a_single_top_level_directory() {
    let workspace = TestWorkspace::new();
    workspace.write_manifest(concat!(
        "version: 1\n",
        "\n",
        "targets:\n",
        "  - claude-code\n",
    ));
    workspace.write_skill_source_at(
        "archive-src",
        "skills/packed-skill",
        "packed-skill",
        "Pack.",
    );
    write_archive(&workspace, "archive-src", "skills", "packed.tar.gz");

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args([
            "--no-input",
            "--name",
            "packed-skill",
            "install",
            "packed.tar.gz",
        ])
        .assert()
        .success()
        .stderr(predicate::str::is_empty())
        .stdout(predicate::str::contains("Installed 1 skill"));

    let manifest = fs::read_to_string(workspace.path().join(".agents/skillctl.yaml"))
        .expect("manifest exists");
    assert!(
        manifest.contains("path: skills/packed-skill"),
        "manifest should record the documented top-level layout: {manifest}",
    );
    assert!(
        workspace
            .path()
            .join(".claude/skills/packed-skill/SKILL.md")
            .is_file(),
        "generated projection should exist",
    );
}

#[test]
fn install_accepts_an_archive_containing_only_one_skill_directory() {
    let workspace = TestWorkspace::new();
    workspace.write_manifest(concat!(
        "version: 1\n",
        "\n",
        "targets:\n",
        "  - claude-code\n",
    ));
    workspace.write_skill_source_at("archive-src", "only-skill", "only-skill", "Do one thing.");
    write_archive(&workspace, "archive-src", "only-skill", "only-skill.tar.gz");

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args([
            "--no-input",
            "--name",
            "only-skill",
            "install",
            "only-skill.tar.gz",
        ])
        .assert()
        .success()
        .stderr(predicate::str::is_empty())
        .stdout(predicate::str::contains("Installed 1 skill"));

    let lockfile = fs::read_to_string(workspace.path().join(".agents/skillctl.lock"))
        .expect("lockfile exists");
    assert!(
        lockfile.contains("subpath: skills/only-skill"),
        "lockfile should store the packaged single-skill subpath: {lockfile}",
    );
    assert!(
        workspace
            .path()
            .join(".claude/skills/only-skill/SKILL.md")
            .is_file(),
        "generated projection should exist",
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
#[cfg(unix)]
fn install_from_a_git_source_with_a_symlink_outside_the_skill_stores_only_the_subpath() {
    let workspace = TestWorkspace::new();
    workspace.write_manifest(concat!(
        "version: 1\n",
        "\n",
        "targets:\n",
        "  - claude-code\n",
    ));
    workspace.write_skill_source("git-source", "release-notes");
    workspace.write_file("git-source/docs/guide.md", "# Guide\n");
    std::os::unix::fs::symlink(
        "guide.md",
        workspace.path().join("git-source/docs/current.md"),
    )
    .expect("symlinked source entry is created");
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
        .stderr(predicate::str::is_empty())
        .stdout(predicate::str::contains("Installed 1 skill"));

    let stored_root = workspace.workspace_import_root("release-notes");
    assert!(
        stored_root
            .join(".agents/skills/release-notes/SKILL.md")
            .is_file(),
        "the selected skill subtree is stored under its subpath",
    );
    assert!(
        !stored_root.join(".git").exists(),
        "the cloned repository metadata must not be stored: {}",
        stored_root.display(),
    );
    assert!(
        !stored_root.join("docs").exists(),
        "source entries outside the selected skill must not be stored: {}",
        stored_root.display(),
    );
    assert!(
        workspace
            .path()
            .join(".claude/skills/release-notes/SKILL.md")
            .is_file(),
        "generated projection should exist",
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
    workspace.write_lockfile(MINIMAL_LOCKFILE.as_str());
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
        .stdout(predicate::str::contains("Telemetry is enabled by default."))
        .stdout(predicate::str::contains(concat!(
            "Install and update events are classified locally, and nothing is ",
            "transmitted today because no emission backend exists.",
        )))
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
    let notice_seen_at = telemetry_row.1.expect("notice timestamp exists");
    assert_rfc3339_utc(&notice_seen_at);
    assert_rfc3339_utc(&telemetry_row.2);

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
fn install_reports_declared_capabilities_for_imported_skills() {
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

    assert_eq!(
        body["data"]["candidates"][0]["safety"]["capabilities"],
        json!(["network", "shell"])
    );
    assert_eq!(
        body["data"]["installed"][0]["safety"]["capabilities"],
        json!(["network", "shell"])
    );
    assert!(
        body["warnings"]
            .as_array()
            .expect("warnings array exists")
            .iter()
            .any(|warning| warning
                .as_str()
                .expect("warning exists")
                .contains("declares capabilities: network, shell")),
        "unexpected warnings: {body:#?}",
    );
}

#[test]
fn install_warns_when_required_credentials_are_missing() {
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

    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .env_remove("OPENAI_API_KEY")
        .env_remove("GITHUB_TOKEN")
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

    assert_eq!(
        body["data"]["installed"][0]["safety"]["credentials"],
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
    assert!(
        body["warnings"]
            .as_array()
            .expect("warnings array exists")
            .iter()
            .any(|warning| warning
                .as_str()
                .expect("warning exists")
                .contains("requires credentials OPENAI_API_KEY that are not set")),
        "unexpected warnings: {body:#?}",
    );
}

#[test]
fn install_warns_when_required_tools_are_missing() {
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

    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .env("PATH", &empty_bin)
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

    assert_eq!(
        body["data"]["installed"][0]["safety"]["dependencies"]["tools"],
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
    assert!(
        body["warnings"]
            .as_array()
            .expect("warnings array exists")
            .iter()
            .any(|warning| warning
                .as_str()
                .expect("warning exists")
                .contains("requires tools ffmpeg that are not available on PATH")),
        "unexpected warnings: {body:#?}",
    );
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
    assert_eq!(status_body["data"]["classification"], "local");
    assert_eq!(status_body["data"]["emission_backend"], "none");

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
    let enable_notice_seen_at = enable_body["data"]["notice_seen_at"]
        .as_str()
        .expect("notice timestamp is present")
        .to_owned();
    assert_rfc3339_utc(&enable_notice_seen_at);
    assert_rfc3339_utc(
        enable_body["data"]["updated_at"]
            .as_str()
            .expect("updated timestamp is present"),
    );
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
        enable_notice_seen_at.as_str(),
        "the first-run notice timestamp stays at when it was first seen"
    );
    assert_rfc3339_utc(
        disable_body["data"]["updated_at"]
            .as_str()
            .expect("updated timestamp is present"),
    );
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
    assert_eq!(telemetry_row.1, enable_notice_seen_at);
    assert_rfc3339_utc(&telemetry_row.2);

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
fn telemetry_status_and_enable_output_state_local_classification_only() {
    let workspace = TestWorkspace::new();

    let status_assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["telemetry", "status"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    let status_stdout =
        String::from_utf8(status_assert.get_output().stdout.clone()).expect("stdout is utf-8");
    assert!(
        status_stdout.contains("Install and update events are classified locally,"),
        "stdout was {status_stdout}"
    );
    assert!(
        status_stdout.contains("nothing is transmitted today because no emission backend exists."),
        "stdout was {status_stdout}"
    );

    let enable_assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["telemetry", "enable"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    let enable_stdout =
        String::from_utf8(enable_assert.get_output().stdout.clone()).expect("stdout is utf-8");
    assert!(
        enable_stdout.contains("Telemetry consent enabled."),
        "stdout was {enable_stdout}"
    );
    assert!(
        enable_stdout.contains("nothing is transmitted today because no emission backend exists."),
        "stdout was {enable_stdout}"
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

/// Compress one top-level entry of a source tree so install tests model the
/// layouts a real archive carries.
fn write_archive(workspace: &TestWorkspace, source_dir: &str, entry: &str, archive_name: &str) {
    let output = ProcessCommand::new("tar")
        .arg("-czf")
        .arg(archive_name)
        .arg("-C")
        .arg(source_dir)
        .arg(entry)
        .current_dir(workspace.path())
        .output()
        .expect("tar launches");
    assert!(
        output.status.success(),
        "tar failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

const SUPPORTED_DISCOVERY_SCHEMA: &str =
    "https://schemas.agentskills.io/discovery/0.2.0/schema.json";
const DISCOVERY_INDEX_PATH: &str = "/.well-known/agent-skills/index.json";

/// Digest of one committed discovery fixture, so an index declares what a real
/// publisher would.
fn code_review_skill_md_digest() -> String {
    fixture_digest("tests/fixtures/discovery/code-review-SKILL.md")
}

/// Digest of the committed discovery archive fixture.
fn code_review_tar_gz_digest() -> String {
    fixture_digest("tests/fixtures/discovery/code-review.tar.gz")
}

/// One `skill-md` artifact served as a discovered entry.
fn skill_md_artifact(name: &str, description: &str) -> (String, String) {
    let body = format!("---\nname: {name}\ndescription: {description}\n---\n\n# {name}\n");
    let digest = {
        let mut hasher = Sha256::new();
        hasher.update(body.as_bytes());
        format!("sha256:{:x}", hasher.finalize())
    };
    (body, digest)
}

/// One discovery index entry as raw JSON.
fn discovery_entry(name: &str, kind: &str, url: &str, digest: &str) -> String {
    format!(
        "{{\"name\":\"{name}\",\"type\":\"{kind}\",\"description\":\"{description}\",\
         \"url\":\"{url}\",\"digest\":\"{digest}\"}}",
        description = "Published through a well-known index.",
    )
}

/// A discovery index document declaring every given entry.
fn discovery_index(entries: &[String]) -> String {
    format!(
        "{{\"$schema\":\"{SUPPORTED_DISCOVERY_SCHEMA}\",\"skills\":[{}]}}",
        entries.join(",")
    )
}

#[test]
fn install_persists_a_well_known_skill_md_entry_as_a_managed_import() {
    let (body, digest) = skill_md_artifact("code-review", "Review code for defects.");
    let origin = serve(vec![
        route(
            DISCOVERY_INDEX_PATH,
            200,
            discovery_index(&[discovery_entry(
                "code-review",
                "skill-md",
                "/download/code-review.md",
                &digest,
            )]),
        ),
        route("/download/code-review.md", 200, body),
    ]);
    let workspace = TestWorkspace::new();
    workspace.write_manifest(concat!(
        "version: 1\n",
        "\n",
        "targets:\n",
        "  - claude-code\n",
    ));

    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .env("SOURCE_DATE_EPOCH", "1770000000")
        .args([
            "--json",
            "--no-input",
            "--name",
            "code-review",
            "install",
            origin.as_str(),
        ])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    let output: Value =
        serde_json::from_slice(&assert.get_output().stdout).expect("stdout is valid json");
    assert_eq!(
        output["data"]["installed"][0]["resolved_revision"], digest,
        "the verified artifact digest is the pinned revision: {output:#?}"
    );
    assert_eq!(output["data"]["source"]["type"], "well-known");

    let manifest = fs::read_to_string(workspace.path().join(".agents/skillctl.yaml"))
        .expect("manifest exists");
    assert!(
        manifest.contains("id: code-review"),
        "manifest was {manifest}"
    );
    assert!(
        manifest.contains("type: well-known"),
        "manifest should record the well-known source kind: {manifest}",
    );
    assert!(
        manifest.contains(&format!("ref: {digest}")),
        "manifest should pin the verified digest: {manifest}",
    );

    let lockfile = fs::read_to_string(workspace.path().join(".agents/skillctl.lock"))
        .expect("lockfile exists");
    assert!(
        lockfile.contains("type: well-known"),
        "lockfile should record the well-known source kind: {lockfile}",
    );
    assert!(
        lockfile.contains("discovery:\n      skill: code-review"),
        "lockfile should remember the selected index entry: {lockfile}",
    );
    assert!(
        lockfile.contains("type: skill-md"),
        "lockfile should record the declared artifact type: {lockfile}",
    );
    assert!(
        lockfile.contains("url: http://127.0.0.1:")
            && lockfile.contains("/download/code-review.md"),
        "lockfile should record the resolved artifact URL: {lockfile}",
    );
    assert!(
        lockfile.contains(&format!("digest: {digest}")),
        "lockfile should record the declared artifact digest: {lockfile}",
    );
    assert!(
        lockfile.contains(&format!("resolved: {digest}")),
        "lockfile should pin the verified digest: {lockfile}",
    );

    assert!(
        workspace
            .workspace_import_root("code-review")
            .join("skills/code-review/SKILL.md")
            .is_file(),
        "stored import checkout should hold the staged artifact",
    );
    assert!(
        workspace
            .path()
            .join(".claude/skills/code-review/SKILL.md")
            .is_file(),
        "generated projection should exist",
    );

    let connection = Connection::open(workspace.home_path().join(".skillctl/state.db"))
        .expect("state database opens");
    let source_kind: String = connection
        .query_row(
            "SELECT source_kind FROM install_records WHERE skill_id = 'code-review'",
            [],
            |row| row.get(0),
        )
        .expect("install record exists");
    assert_eq!(source_kind, "well-known");

    // The recorded state must keep resolving: verify stays clean after install.
    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--json", "verify"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
}

#[test]
fn install_persists_a_well_known_archive_entry() {
    let origin = serve(vec![
        route(
            DISCOVERY_INDEX_PATH,
            200,
            discovery_index(&[discovery_entry(
                "code-review",
                "archive",
                "/download/code-review.tar.gz",
                &code_review_tar_gz_digest(),
            )]),
        ),
        route_bytes(
            "/download/code-review.tar.gz",
            200,
            fixture_bytes("tests/fixtures/discovery/code-review.tar.gz"),
        ),
    ]);
    let workspace = TestWorkspace::new();
    workspace.write_manifest(concat!(
        "version: 1\n",
        "\n",
        "targets:\n",
        "  - claude-code\n",
    ));

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args([
            "--no-input",
            "--name",
            "code-review",
            "install",
            origin.as_str(),
        ])
        .assert()
        .success()
        .stderr(predicate::str::is_empty())
        .stdout(predicate::str::contains("Installed 1 skill"));

    let lockfile = fs::read_to_string(workspace.path().join(".agents/skillctl.lock"))
        .expect("lockfile exists");
    assert!(
        lockfile.contains("type: archive"),
        "lockfile should record the declared artifact type: {lockfile}",
    );
    assert!(
        lockfile.contains("subpath: .agents/skills/code-review"),
        "lockfile should store the unpacked layout subpath: {lockfile}",
    );
    assert!(
        workspace
            .workspace_import_root("code-review")
            .join(".agents/skills/code-review/references/style.md")
            .is_file(),
        "every unpacked file should be stored",
    );
    assert!(
        workspace
            .path()
            .join(".claude/skills/code-review/SKILL.md")
            .is_file(),
        "generated projection should exist",
    );
}

#[test]
fn install_selects_one_well_known_entry_by_exact_name() {
    let (other_body, other_digest) = skill_md_artifact("release-notes", "Summarize releases.");
    let origin = serve(vec![
        route(
            DISCOVERY_INDEX_PATH,
            200,
            discovery_index(&[
                discovery_entry(
                    "code-review",
                    "skill-md",
                    "/download/code-review.md",
                    &code_review_skill_md_digest(),
                ),
                discovery_entry(
                    "release-notes",
                    "skill-md",
                    "/download/release-notes.md",
                    &other_digest,
                ),
            ]),
        ),
        route(
            "/download/code-review.md",
            200,
            String::from_utf8(fixture_bytes(
                "tests/fixtures/discovery/code-review-SKILL.md",
            ))
            .expect("fixture is valid utf-8"),
        ),
        route("/download/release-notes.md", 200, other_body),
    ]);
    let workspace = TestWorkspace::new();

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args([
            "--no-input",
            "--name",
            "release-notes",
            "install",
            origin.as_str(),
        ])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    let manifest = fs::read_to_string(workspace.path().join(".agents/skillctl.yaml"))
        .expect("manifest exists");
    assert!(
        manifest.contains("id: release-notes"),
        "the selected entry is installed: {manifest}"
    );
    assert!(
        !manifest.contains("code-review"),
        "the unselected entry must not be installed: {manifest}",
    );
}

#[test]
fn install_accepts_a_well_known_index_url() {
    let (body, digest) = skill_md_artifact("code-review", "Review code for defects.");
    let index_path = "/published/skills.json";
    let origin = serve(vec![
        route(
            index_path,
            200,
            discovery_index(&[discovery_entry(
                "code-review",
                "skill-md",
                "/download/code-review.md",
                &digest,
            )]),
        ),
        route("/download/code-review.md", 200, body),
    ]);
    let workspace = TestWorkspace::new();

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args([
            "--no-input",
            "--name",
            "code-review",
            "install",
            format!("{origin}{index_path}").as_str(),
        ])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    let lockfile = fs::read_to_string(workspace.path().join(".agents/skillctl.lock"))
        .expect("lockfile exists");
    assert!(
        lockfile.contains(&format!("url: {origin}{index_path}")),
        "the lockfile should pin the index URL that was installed from: {lockfile}",
    );
}

#[test]
fn install_names_the_declared_entries_when_a_well_known_name_is_unknown() {
    let (body, digest) = skill_md_artifact("code-review", "Review code for defects.");
    let origin = serve(vec![
        route(
            DISCOVERY_INDEX_PATH,
            200,
            discovery_index(&[discovery_entry(
                "code-review",
                "skill-md",
                "/download/code-review.md",
                &digest,
            )]),
        ),
        route("/download/code-review.md", 200, body),
    ]);
    let workspace = TestWorkspace::new();

    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args([
            "--json",
            "--no-input",
            "--name",
            "demo-skill",
            "install",
            origin.as_str(),
        ])
        .assert()
        .failure()
        .code(3)
        .stderr(predicate::str::is_empty());
    let output: Value =
        serde_json::from_slice(&assert.get_output().stdout).expect("stdout is valid json");

    let errors = output["errors"]
        .as_array()
        .expect("errors array exists")
        .iter()
        .filter_map(|error| error.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        errors.contains("declares no skill named 'demo-skill'"),
        "unexpected errors: {output:#?}",
    );
    assert!(
        errors.contains("code-review"),
        "the declared names should be listed: {output:#?}",
    );
    assert!(
        !workspace.path().join(".agents").exists(),
        "a refused install must not create workspace control files",
    );
}

#[test]
fn install_requires_input_when_a_well_known_index_has_no_selector() {
    let (body, digest) = skill_md_artifact("code-review", "Review code for defects.");
    let origin = serve(vec![
        route(
            DISCOVERY_INDEX_PATH,
            200,
            discovery_index(&[discovery_entry(
                "code-review",
                "skill-md",
                "/download/code-review.md",
                &digest,
            )]),
        ),
        route("/download/code-review.md", 200, body),
    ]);
    let workspace = TestWorkspace::new();

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--no-input", "install", origin.as_str()])
        .assert()
        .failure()
        .code(5)
        .stderr(predicate::str::contains(
            "interactive input is required for command 'install'",
        ));

    assert!(
        !workspace.path().join(".agents").exists(),
        "a refused install must not create workspace control files",
    );
}

#[test]
fn install_refuses_a_well_known_artifact_that_does_not_match_its_digest() {
    // The index advertises the committed archive digest while the origin serves
    // other bytes, the way a stale index does.
    let origin = serve(vec![
        route(
            DISCOVERY_INDEX_PATH,
            200,
            discovery_index(&[discovery_entry(
                "code-review",
                "archive",
                "/download/code-review.tar.gz",
                &code_review_tar_gz_digest(),
            )]),
        ),
        route(
            "/download/code-review.tar.gz",
            200,
            "these bytes are not the declared artifact".to_string(),
        ),
    ]);
    let workspace = TestWorkspace::new();

    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args([
            "--json",
            "--no-input",
            "--name",
            "code-review",
            "install",
            origin.as_str(),
        ])
        .assert()
        .failure()
        .code(3)
        .stderr(predicate::str::is_empty());
    let output: Value =
        serde_json::from_slice(&assert.get_output().stdout).expect("stdout is valid json");

    let errors = output["errors"]
        .as_array()
        .expect("errors array exists")
        .iter()
        .filter_map(|error| error.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        errors.contains("does not match its declared digest"),
        "unexpected errors: {output:#?}",
    );
    assert!(
        errors.contains("was not used"),
        "the refusal should say the content was not used: {output:#?}",
    );
    assert!(
        !workspace.path().join(".agents").exists(),
        "a refused install must not create workspace control files",
    );
}

#[test]
fn a_well_known_install_re_reads_the_index_for_update_and_list() {
    let (body, digest) = skill_md_artifact("code-review", "Review code for defects.");
    let origin = serve(vec![
        route(
            DISCOVERY_INDEX_PATH,
            200,
            discovery_index(&[discovery_entry(
                "code-review",
                "skill-md",
                "/download/code-review.md",
                &digest,
            )]),
        ),
        route("/download/code-review.md", 200, body),
    ]);
    let workspace = TestWorkspace::new();
    workspace.write_manifest(concat!(
        "version: 1\n",
        "\n",
        "targets:\n",
        "  - claude-code\n",
    ));

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args([
            "--no-input",
            "--name",
            "code-review",
            "install",
            origin.as_str(),
        ])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    // The update check re-reads the discovery index, and the loopback origin
    // above still declares the digest the import pins.
    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--json", "update"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    let output: Value =
        serde_json::from_slice(&assert.get_output().stdout).expect("stdout is valid json");

    let plans = output["data"]["plans"]
        .as_array()
        .expect("plans array exists");
    let plan = plans
        .iter()
        .find(|plan| plan["skill"] == "code-review")
        .expect("the well-known import is checked");
    assert_eq!(plan["source"]["type"], "well-known");
    assert_eq!(plan["pinned_revision"], digest);
    assert_eq!(plan["latest_revision"], digest);
    assert_eq!(plan["outcome"], "up-to-date");
    assert!(
        plan["notes"]
            .as_array()
            .expect("notes array exists")
            .iter()
            .any(|note| note
                .as_str()
                .unwrap_or_default()
                .contains("pinned artifact digest already matches the discovery index")),
        "the check should name the digest it re-read: {plan:#?}",
    );
    assert_eq!(
        output["data"]["telemetry"]["events"][0]["source_visibility"], "suppressed-local",
        "a loopback discovery origin classifies as local, not private: {output:#?}",
    );
    assert_eq!(
        output["data"]["telemetry"]["events"][0]["suppression_reason"], "local-source",
        "the loopback origin is suppressed as a local source: {output:#?}",
    );

    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--json", "list"])
        .assert()
        .success();
    let output: Value =
        serde_json::from_slice(&assert.get_output().stdout).expect("stdout is valid json");
    let listed = output["data"]["skills"]
        .as_array()
        .expect("skills array exists")
        .iter()
        .find(|skill| skill["skill"] == "code-review")
        .expect("the well-known import is listed");
    assert_eq!(
        listed["source"]["type"], "well-known",
        "list should report the recorded source kind: {output:#?}"
    );

    // Pin and rollback stay git-only, the way local sources are refused.
    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["pin", "code-review", "main"])
        .assert()
        .failure()
        .code(3)
        .stderr(predicate::str::contains(
            "only supports pin and rollback for git imports",
        ));
}

#[test]
fn restore_refetches_a_well_known_import_from_its_recorded_artifact_url() {
    let (body, digest) = skill_md_artifact("code-review", "Review code for defects.");
    let origin = serve(vec![
        route(
            DISCOVERY_INDEX_PATH,
            200,
            discovery_index(&[discovery_entry(
                "code-review",
                "skill-md",
                "/download/code-review.md",
                &digest,
            )]),
        ),
        route("/download/code-review.md", 200, body),
    ]);
    let workspace = TestWorkspace::new();
    workspace.write_manifest(concat!(
        "version: 1\n",
        "\n",
        "targets:\n",
        "  - claude-code\n",
    ));

    let install = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args([
            "--no-input",
            "--name",
            "code-review",
            "install",
            origin.as_str(),
        ])
        .assert()
        .success();

    let stored_import = workspace.workspace_import_root("code-review");
    let lockfile_before = fs::read_to_string(workspace.path().join(".agents/skillctl.lock"))
        .expect("lockfile exists");
    drop(install);

    // A fresh checkout holds neither the import cache nor a retained artifact.
    fs::remove_dir_all(&stored_import).expect("import cache is removed");
    fs::remove_dir_all(workspace.home_path().join(".skillctl/store/artifacts"))
        .expect("artifact store is removed");

    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--json", "restore", "--frozen"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    let output: Value =
        serde_json::from_slice(&assert.get_output().stdout).expect("stdout is valid json");
    assert_eq!(
        output["data"]["imports"][0]["restored_from"], "source-at-locked-revision",
        "the restore should re-fetch the recorded artifact: {output:#?}"
    );
    assert_eq!(
        output["data"]["imports"][0]["source_type"], "well-known",
        "the restore should report the recorded source kind: {output:#?}"
    );

    assert!(
        stored_import.join("skills/code-review/SKILL.md").is_file(),
        "the import cache should be rebuilt"
    );
    assert_eq!(
        fs::read_to_string(workspace.path().join(".agents/skillctl.lock"))
            .expect("lockfile exists"),
        lockfile_before,
        "a frozen restore must not rewrite the lockfile"
    );
}

#[test]
fn install_interactively_selects_one_well_known_entry() {
    let (other_body, other_digest) = skill_md_artifact("release-notes", "Summarize releases.");
    let (body, digest) = skill_md_artifact("code-review", "Review code for defects.");
    let origin = serve(vec![
        route(
            DISCOVERY_INDEX_PATH,
            200,
            discovery_index(&[
                discovery_entry(
                    "code-review",
                    "skill-md",
                    "/download/code-review.md",
                    &digest,
                ),
                discovery_entry(
                    "release-notes",
                    "skill-md",
                    "/download/release-notes.md",
                    &other_digest,
                ),
            ]),
        ),
        route("/download/code-review.md", 200, body),
        route("/download/release-notes.md", 200, other_body),
    ]);
    let workspace = TestWorkspace::new();

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--interactive", "install", origin.as_str()])
        .write_stdin("2\n1\n")
        .assert()
        .success()
        .stderr(predicate::str::is_empty())
        .stdout(predicate::str::contains("Discovered 2 skills at"))
        .stdout(predicate::str::contains("1. code-review (skill-md)"))
        .stdout(predicate::str::contains("2. release-notes (skill-md)"))
        .stdout(predicate::str::contains(
            "Select one skill by number or exact name",
        ))
        .stdout(predicate::str::contains("Select scope"))
        .stdout(predicate::str::contains(
            "Installed 1 skill (release-notes)",
        ));

    let manifest = fs::read_to_string(workspace.path().join(".agents/skillctl.yaml"))
        .expect("manifest exists");
    assert!(
        manifest.contains("id: release-notes"),
        "the prompted entry is installed: {manifest}"
    );
    assert!(
        !manifest.contains("code-review"),
        "the unselected entry must not be installed: {manifest}",
    );
}
