use super::*;

#[test]
fn init_bootstraps_the_default_workspace_layout() {
    let workspace = TestWorkspace::new();

    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
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
        .env("HOME", workspace.home_path())
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
        .env("HOME", workspace.home_path())
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
        .env("HOME", workspace.home_path())
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
