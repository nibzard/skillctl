use assert_cmd::Command;
use predicates::prelude::*;
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

const MINIMAL_LOCKFILE: &str = concat!(
    "version: 1\n",
    "\n",
    "state:\n",
    "  manifest_version: 1\n",
    "  local_state_version: 1\n",
);

#[test]
fn root_help_lists_core_commands() {
    let mut cmd = Command::cargo_bin("skillctl").expect("binary exists");

    cmd.arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("init"))
        .stdout(predicate::str::contains("install"))
        .stdout(predicate::str::contains("update"))
        .stdout(predicate::str::contains("doctor"));
}

#[test]
fn init_help_is_available() {
    let mut cmd = Command::cargo_bin("skillctl").expect("binary exists");

    cmd.args(["init", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Initialize"));
}

#[test]
fn install_help_is_available() {
    let mut cmd = Command::cargo_bin("skillctl").expect("binary exists");

    cmd.args(["install", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Install"));
}

#[test]
fn bare_invocation_prints_help() {
    let mut cmd = Command::cargo_bin("skillctl").expect("binary exists");

    cmd.assert()
        .success()
        .stdout(predicate::str::contains("Usage:"))
        .stdout(predicate::str::contains("skillctl"))
        .stderr(predicate::str::is_empty());
}

#[test]
fn init_dispatches_to_runtime_and_bootstraps_the_workspace() {
    let workspace = TestWorkspace::new();
    let mut cmd = Command::cargo_bin("skillctl").expect("binary exists");

    cmd.current_dir(workspace.path())
        .arg("init")
        .assert()
        .success()
        .stdout(predicate::str::contains("skillctl workspace"))
        .stderr(predicate::str::is_empty());
}

#[test]
fn json_output_uses_stable_response_contract_for_command_errors() {
    let mut cmd = Command::cargo_bin("skillctl").expect("binary exists");

    let assert = cmd
        .args(["--json", "install", "./missing-source"])
        .assert()
        .failure()
        .code(3)
        .stderr(predicate::str::is_empty());

    let output = assert.get_output();
    let body: Value = serde_json::from_slice(&output.stdout).expect("stdout is valid json");

    assert_eq!(
        body,
        json!({
            "ok": false,
            "command": "install",
            "warnings": [],
            "errors": ["install source './missing-source' is invalid: source must be a Git URL, existing local directory, or existing local archive"],
            "data": {}
        })
    );
}

#[test]
fn global_execution_flags_are_accepted_before_the_subcommand() {
    let workspace = TestWorkspace::new();
    workspace.write_skill_source("shared-skills", "release-notes");
    let mut cmd = Command::cargo_bin("skillctl").expect("binary exists");

    let assert = cmd
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args([
            "--json",
            "--no-input",
            "--name",
            "release-notes",
            "--scope",
            "workspace",
            "--target",
            "codex",
            "--cwd",
            ".",
            "install",
            "shared-skills",
        ])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    let output = assert.get_output();
    let body: Value = serde_json::from_slice(&output.stdout).expect("stdout is valid json");

    assert_eq!(body["command"], "install");
    assert_eq!(body["ok"], true);
    assert_eq!(body["data"]["source"]["type"], "local-path");
    assert_eq!(body["data"]["selected"][0]["name"], "release-notes");
    assert_eq!(body["data"]["installed"][0]["scope"], "workspace");
    assert!(
        workspace
            .path()
            .join(".agents/skills/release-notes/SKILL.md")
            .is_file()
    );
}

#[test]
fn install_alias_help_is_available() {
    let mut cmd = Command::cargo_bin("skillctl").expect("binary exists");

    cmd.args(["i", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Install"));
}

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
            .home_path()
            .join(".skillctl/store/imports/release-notes/.agents/skills/release-notes/SKILL.md")
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
            "SELECT scope, skill_id, source_kind FROM install_records",
            [],
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
fn init_bootstraps_the_default_workspace_layout() {
    let workspace = TestWorkspace::new();

    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .arg("init")
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    assert
        .stdout(predicate::str::contains("Initialized skillctl workspace"))
        .stdout(predicate::str::contains(".agents/skills"))
        .stdout(predicate::str::contains(".agents/overlays"))
        .stdout(predicate::str::contains(".agents/skillctl.yaml"))
        .stdout(predicate::str::contains(".agents/skillctl.lock"))
        .stdout(predicate::str::contains("Skipped local git excludes"));

    assert!(workspace.path().join(".agents/skills").is_dir());
    assert!(workspace.path().join(".agents/overlays").is_dir());
    assert_eq!(
        fs::read_to_string(workspace.path().join(".agents/skillctl.yaml"))
            .expect("manifest exists"),
        concat!(
            "version: 1\n",
            "\n",
            "targets:\n",
            "  - codex\n",
            "  - gemini-cli\n",
            "  - opencode\n",
        )
    );
    assert_eq!(
        fs::read_to_string(workspace.path().join(".agents/skillctl.lock"))
            .expect("lockfile exists"),
        MINIMAL_LOCKFILE
    );
}

#[test]
fn init_updates_git_info_exclude_without_touching_gitignore_and_is_idempotent() {
    let workspace = TestWorkspace::new();
    fs::create_dir_all(workspace.path().join(".git/info")).expect("git info directory exists");
    fs::write(
        workspace.path().join(".git/info/exclude"),
        "# existing rule\n/.cache/\n",
    )
    .expect("exclude file exists");
    fs::write(
        workspace.path().join(".gitignore"),
        "node_modules/\ncoverage/\n",
    )
    .expect("gitignore exists");

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .arg("init")
        .assert()
        .success()
        .stderr(predicate::str::is_empty())
        .stdout(predicate::str::contains("Updated local git excludes"));

    let exclude_after_first =
        fs::read_to_string(workspace.path().join(".git/info/exclude")).expect("exclude readable");
    assert!(
        exclude_after_first.contains("# existing rule\n/.cache/\n"),
        "preserves existing exclude content",
    );
    assert!(
        exclude_after_first.contains("/.claude/skills/\n"),
        "adds claude projection root",
    );
    assert!(
        exclude_after_first.contains("/.github/skills/\n"),
        "adds github projection root",
    );
    assert!(
        exclude_after_first.contains("/.gemini/skills/\n"),
        "adds gemini projection root",
    );
    assert!(
        exclude_after_first.contains("/.opencode/skills/\n"),
        "adds opencode projection root",
    );
    assert_eq!(
        fs::read_to_string(workspace.path().join(".gitignore")).expect("gitignore readable"),
        "node_modules/\ncoverage/\n"
    );

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .arg("init")
        .assert()
        .success()
        .stderr(predicate::str::is_empty())
        .stdout(predicate::str::contains("No changes were required"));

    let exclude_after_second =
        fs::read_to_string(workspace.path().join(".git/info/exclude")).expect("exclude readable");
    assert_eq!(exclude_after_first, exclude_after_second);
}

#[test]
fn init_json_output_describes_created_and_skipped_items() {
    let workspace = TestWorkspace::new();
    fs::create_dir_all(workspace.path().join(".git/info")).expect("git info directory exists");

    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .args(["--json", "init"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    let output = assert.get_output();
    let body: Value = serde_json::from_slice(&output.stdout).expect("stdout is valid json");

    assert_eq!(
        body,
        json!({
            "ok": true,
            "command": "init",
            "warnings": [],
            "errors": [],
            "data": {
                "created": [
                    ".agents/skills",
                    ".agents/overlays",
                    ".agents/skillctl.yaml",
                    ".agents/skillctl.lock"
                ],
                "skipped": [],
                "git_exclude": {
                    "path": ".git/info/exclude",
                    "created": [
                        "/.claude/skills/",
                        "/.github/skills/",
                        "/.gemini/skills/",
                        "/.opencode/skills/"
                    ],
                    "skipped": []
                }
            }
        })
    );
}

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
        .arg("sync")
        .assert()
        .failure()
        .code(3)
        .stderr(predicate::str::contains(
            "refusing to overwrite hand-authored runtime skill directory",
        ));
}

struct TestWorkspace {
    path: PathBuf,
}

impl TestWorkspace {
    fn new() -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time moved backwards")
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("skillctl-test-{}-{}", std::process::id(), unique));
        fs::create_dir_all(&path).expect("workspace exists");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn home_path(&self) -> PathBuf {
        let path = self.path.join("home");
        fs::create_dir_all(&path).expect("home directory exists");
        path
    }

    fn write_manifest(&self, contents: &str) {
        self.write_file(".agents/skillctl.yaml", contents);
    }

    fn write_lockfile(&self, contents: &str) {
        self.write_file(".agents/skillctl.lock", contents);
    }

    fn write_workspace_skill(
        &self,
        skill_name: &str,
        description: &str,
        extra_files: &[(&str, &str)],
    ) {
        let skill_root = self.path.join(".agents/skills").join(skill_name);
        fs::create_dir_all(&skill_root).expect("skill source root exists");
        fs::write(
            skill_root.join("SKILL.md"),
            format!(
                concat!(
                    "---\n",
                    "name: {skill_name}\n",
                    "description: {description}\n",
                    "---\n",
                    "\n",
                    "# {skill_name}\n"
                ),
                skill_name = skill_name,
                description = description,
            ),
        )
        .expect("skill manifest exists");

        for (relative_path, contents) in extra_files {
            let path = skill_root.join(relative_path);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("extra file parent exists");
            }
            fs::write(path, contents).expect("extra file written");
        }
    }

    fn write_file(&self, relative_path: &str, contents: &str) {
        let path = self.path.join(relative_path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("parent directory exists");
        }
        fs::write(path, contents).expect("file written");
    }

    fn write_skill_source(&self, source_dir: &str, skill_name: &str) {
        self.write_skill_source_at(
            source_dir,
            &format!(".agents/skills/{skill_name}"),
            skill_name,
            "Summarize release notes.",
        );
    }

    fn write_skill_source_at(
        &self,
        source_dir: &str,
        relative_skill_root: &str,
        skill_name: &str,
        description: &str,
    ) {
        let skill_root = self.path.join(source_dir).join(relative_skill_root);
        fs::create_dir_all(&skill_root).expect("skill source root exists");
        fs::write(
            skill_root.join("SKILL.md"),
            format!(
                concat!(
                    "---\n",
                    "name: {skill_name}\n",
                    "description: {description}\n",
                    "---\n",
                    "\n",
                    "# {skill_name}\n"
                ),
                skill_name = skill_name,
                description = description,
            ),
        )
        .expect("skill manifest exists");
    }
}

impl Drop for TestWorkspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}
