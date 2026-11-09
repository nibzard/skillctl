//! Git source acquisition under an environment an enclosing git context
//! exports.
//!
//! A git hook or a linked worktree exports `GIT_DIR`, `GIT_WORK_TREE`, and
//! `GIT_INDEX_FILE`, and those variables override repository discovery. Source
//! staging must scrub them, or the git commands skillctl runs inside its
//! staging clone target the repository that exported them instead: the remote
//! registration and the fetch land in the enclosing repository, and the staging
//! tree stays empty.

use super::*;

#[test]
fn restore_reacquires_the_locked_revision_inside_an_exported_git_environment() {
    let author = TestWorkspace::new();
    author.write_manifest(concat!("version: 1\n", "\n", "targets:\n  - claude-code\n",));
    author.write_skill_source("git-source", "release-notes");
    author.init_git_repo("git-source");
    let repo_url = author.git_repo_url("git-source");

    let install = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(author.path())
        .env("HOME", author.home_path())
        .args([
            "--json",
            "--no-input",
            "--name",
            "release-notes",
            "install",
            &repo_url,
        ])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    let body: Value =
        serde_json::from_slice(&install.get_output().stdout).expect("stdout is valid json");
    assert_eq!(body["ok"], true, "install failed: {body:#?}");

    // A second, unrelated repository stands in for the repository that owns
    // the exported environment. Its history is unrelated to the import.
    let enclosing = TestWorkspace::new();
    enclosing.write_skill_source("enclosing-source", "unrelated");
    enclosing.init_git_repo("enclosing-source");
    let enclosing_git_dir = enclosing.path().join("enclosing-source/.git");

    // A fresh checkout with a fresh home holds only the committed portable
    // state, so the frozen restore has to reacquire from the recorded source.
    let clone = TestWorkspace::new();
    for relative in [".agents/skillctl.yaml", ".agents/skillctl.lock"] {
        let contents = fs::read_to_string(author.path().join(relative))
            .unwrap_or_else(|error| panic!("{relative} is readable: {error}"));
        clone.write_file(relative, &contents);
    }

    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(clone.path())
        .env("HOME", clone.home_path())
        .env("GIT_DIR", &enclosing_git_dir)
        .env("GIT_WORK_TREE", enclosing.path().join("enclosing-source"))
        .env("GIT_INDEX_FILE", enclosing_git_dir.join("index"))
        .args(["--json", "restore", "--frozen"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    let body: Value =
        serde_json::from_slice(&assert.get_output().stdout).expect("stdout is valid json");
    assert_eq!(body["ok"], true, "restore failed: {body:#?}");
    assert_eq!(
        body["data"]["imports"][0]["restored_from"], "source-at-locked-revision",
        "the restore must acquire from the recorded git source: {body:#?}",
    );
    assert!(
        clone
            .path()
            .join(".claude/skills/release-notes/SKILL.md")
            .is_file(),
        "the restored skill projects into the clone"
    );

    // The enclosing repository must stay untouched: without the scrub, the
    // staging `git remote add` and `git fetch` would land inside it.
    assert_eq!(
        enclosing.git_stdout("enclosing-source", &["remote"]),
        "",
        "the exported GIT_DIR must not redirect skillctl's git commands",
    );
}
