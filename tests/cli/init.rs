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
        .stdout(predicate::str::contains("Skipped git excludes"));

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
        MINIMAL_LOCKFILE.as_str()
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
        .stdout(predicate::str::contains(
            "Updated git excludes in .git/info/exclude",
        ));

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

/// Manifest that routes Git exclusions into `.gitignore`.
const GITIGNORE_EXCLUDE_MANIFEST: &str = concat!(
    "version: 1\n",
    "\n",
    "projection:\n",
    "  git_exclude: gitignore\n",
    "\n",
    "targets:\n",
    "  - codex\n",
    "  - gemini-cli\n",
    "  - opencode\n",
);

/// Manifest that opts out of Git exclusions entirely.
const NONE_EXCLUDE_MANIFEST: &str = concat!(
    "version: 1\n",
    "\n",
    "projection:\n",
    "  git_exclude: none\n",
    "\n",
    "targets:\n",
    "  - codex\n",
    "  - gemini-cli\n",
    "  - opencode\n",
);

#[test]
fn init_honors_a_git_exclude_mode_of_gitignore() {
    let workspace = TestWorkspace::new();
    workspace.write_manifest(GITIGNORE_EXCLUDE_MANIFEST);
    fs::create_dir_all(workspace.path().join(".git/info")).expect("git info directory exists");
    fs::write(workspace.path().join(".git/info/exclude"), "# local rule\n")
        .expect("exclude file exists");
    fs::write(workspace.path().join(".gitignore"), "node_modules/\n").expect("gitignore exists");

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .arg("init")
        .assert()
        .success()
        .stderr(predicate::str::is_empty())
        .stdout(predicate::str::contains(
            "Updated git excludes in .gitignore",
        ));

    assert_eq!(
        fs::read_to_string(workspace.path().join(".gitignore")).expect("gitignore readable"),
        concat!(
            "node_modules/\n",
            "/.claude/skills/\n",
            "/.github/skills/\n",
            "/.gemini/skills/\n",
            "/.opencode/skills/\n",
        ),
        "gitignore mode appends the generated runtime roots to .gitignore",
    );
    assert_eq!(
        fs::read_to_string(workspace.path().join(".git/info/exclude")).expect("exclude readable"),
        "# local rule\n",
        "gitignore mode must leave the local exclude file alone",
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

    assert_eq!(
        fs::read_to_string(workspace.path().join(".gitignore")).expect("gitignore readable"),
        concat!(
            "node_modules/\n",
            "/.claude/skills/\n",
            "/.github/skills/\n",
            "/.gemini/skills/\n",
            "/.opencode/skills/\n",
        ),
        "a second init adds nothing",
    );
}

#[test]
fn init_honors_a_git_exclude_mode_of_none() {
    let workspace = TestWorkspace::new();
    workspace.write_manifest(NONE_EXCLUDE_MANIFEST);
    fs::create_dir_all(workspace.path().join(".git/info")).expect("git info directory exists");
    fs::write(
        workspace.path().join(".git/info/exclude"),
        "# existing rule\n",
    )
    .expect("exclude file exists");

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
        body["data"]["git_exclude"],
        json!({"path": null, "created": [], "skipped": []}),
        "opting out reports no exclude work: {body:#?}",
    );

    assert_eq!(
        fs::read_to_string(workspace.path().join(".git/info/exclude")).expect("exclude readable"),
        "# existing rule\n",
        "none mode must leave the local exclude file untouched",
    );
    assert!(
        !workspace.path().join(".gitignore").exists(),
        "none mode must not create .gitignore either",
    );
}

#[test]
fn init_reports_the_worktree_exclude_file_it_updates() {
    let workspace = TestWorkspace::new();
    workspace.write_file("main/README.md", "initial\n");
    workspace.init_git_repo("main");
    let worktree_path = workspace.path().join("wt");
    workspace.run_git(
        &workspace.path().join("main"),
        &[
            "worktree",
            "add",
            "--detach",
            worktree_path.to_str().expect("worktree path is utf-8"),
        ],
    );
    assert!(
        worktree_path.join(".git").is_file(),
        "a linked worktree keeps .git as a file",
    );

    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(&worktree_path)
        .env("HOME", workspace.home_path())
        .args(["--json", "init"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    let output = assert.get_output();
    let body: Value = serde_json::from_slice(&output.stdout).expect("stdout is valid json");
    let reported = body["data"]["git_exclude"]["path"]
        .as_str()
        .unwrap_or_else(|| panic!("git_exclude.path is a string: {body:#?}"))
        .to_string();
    assert!(
        reported.ends_with(".git/worktrees/wt/info/exclude"),
        "the reported path names the worktree metadata, found '{reported}'",
    );
    assert!(
        Path::new(&reported).is_file(),
        "the reported path exists, unlike a plain '.git/info/exclude' in the worktree: \
         '{reported}'",
    );
    assert!(
        !worktree_path.join(".git/info/exclude").exists(),
        "no misleading path was created inside the worktree",
    );
    assert_eq!(
        body["data"]["git_exclude"]["created"]
            .as_array()
            .map(Vec::len),
        Some(4),
        "all generated runtime roots were excluded: {body:#?}",
    );
}
