//! Regression checks for projection ownership around diverged import stores
//! and for the lockfile pins that protect retained copies from `clean`.
//!
//! These tests pin two ownership contracts of `skillctl sync` and
//! `skillctl clean`:
//!
//! - a directory skillctl never generated is never reported as a preserved
//!   copy, even when the import store behind the plan diverged from its pin,
//! - one diverged import is reported once per sync even when several planned
//!   roots would project it, and
//! - a lockfile pin keeps the retained store copy alive through `clean`,
//!   because `skillctl remove` is the command that clears a pin and its copy
//!   together.

#[allow(dead_code)]
mod support;

use assert_cmd::Command;
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use std::{fs, io::Write as _, path::PathBuf};

use support::TestWorkspace;

/// Manifest that projects every target through symlinks.
const SYMLINK_CLAUDE_MANIFEST: &str = concat!(
    "version: 1\n",
    "\n",
    "projection:\n",
    "  mode: symlink\n",
    "  allow_unsafe_targets:\n",
    "    - claude-code\n",
    "\n",
    "targets:\n",
    "  - claude-code\n",
);

/// Symlink manifest whose targets resolve to distinct physical roots.
const SYMLINK_TWO_ROOTS_MANIFEST: &str = concat!(
    "version: 1\n",
    "\n",
    "projection:\n",
    "  mode: symlink\n",
    "  allow_unsafe_targets:\n",
    "    - claude-code\n",
    "    - codex\n",
    "\n",
    "targets:\n",
    "  - claude-code\n",
    "  - codex\n",
);

/// Workspace that installed one import the given symlink manifest projects.
struct SymlinkImportFixture {
    workspace: TestWorkspace,
}

impl SymlinkImportFixture {
    /// The symlink-mode warning makes install exit with 2 instead of 0.
    fn install(manifest: &str) -> Self {
        let workspace = TestWorkspace::new();
        workspace.write_file(".agents/skillctl.yaml", manifest);
        write_skill_source(&workspace, "git-source", "release-notes");
        workspace.init_git_repo("git-source");

        let output = Command::cargo_bin("skillctl")
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
            .output()
            .expect("install runs");
        assert!(
            output.stderr.is_empty(),
            "unexpected stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            output.status.code(),
            Some(2),
            "install succeeds with the symlink warning: {}",
            String::from_utf8_lossy(&output.stdout)
        );

        Self { workspace }
    }

    /// Append to the stored import through a projected link, the way an
    /// editor that follows the link writes the pinned bytes themselves.
    fn diverge_the_store(&self, projected_link: PathBuf, note: &str) {
        let stored_manifest =
            fs::read_link(&projected_link).expect("the projected manifest is a link");
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(&stored_manifest)
            .expect("the stored import is opened");
        writeln!(file, "{note}").expect("the note is written into the store");
    }

    fn run_json(&self, args: &[&str]) -> (i32, Value) {
        let output = Command::cargo_bin("skillctl")
            .expect("binary exists")
            .current_dir(self.workspace.path())
            .env("HOME", self.workspace.home_path())
            .args(args)
            .output()
            .expect("command runs");
        assert!(
            output.stderr.is_empty(),
            "unexpected stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let code = output.status.code().unwrap_or(-1);
        let body: Value = serde_json::from_slice(&output.stdout).expect("stdout is valid json");
        (code, body)
    }

    /// Warnings that report the diverged store, from the shared warning list.
    fn diverged_warnings(body: &Value) -> Vec<&Value> {
        body["warnings"]
            .as_array()
            .expect("warnings array exists")
            .iter()
            .filter(|warning| {
                warning
                    .as_str()
                    .is_some_and(|text| text.contains("no longer matches its pinned content"))
            })
            .collect()
    }
}

/// Write one skill below a git source directory, the layout `install`
/// discovers with `--name`.
fn write_skill_source(workspace: &TestWorkspace, source_dir: &str, skill_name: &str) {
    workspace.write_file(
        &format!("{source_dir}/.agents/skills/{skill_name}/SKILL.md"),
        &format!(
            concat!(
                "---\n",
                "name: {skill_name}\n",
                "description: Summarize release notes.\n",
                "---\n",
                "\n",
                "# {skill_name}\n"
            ),
            skill_name = skill_name,
        ),
    );
    workspace.write_file(
        &format!("{source_dir}/.agents/skills/{skill_name}/notes.md"),
        "# Notes\n",
    );
}

/// A diverged store must not turn a hand-authored directory into a preserved
/// copy: the named repairs repair the store, not a directory skillctl never
/// wrote, so the run reports the divergence the way a missing copy does and
/// leaves the directory untouched.
#[test]
fn sync_does_not_claim_a_hand_authored_directory_for_a_diverged_store() {
    let fixture = SymlinkImportFixture::install(SYMLINK_CLAUDE_MANIFEST);
    let projection_dir = fixture
        .workspace
        .path()
        .join(".claude/skills/release-notes");
    fixture.diverge_the_store(projection_dir.join("SKILL.md"), "# local note");

    fs::remove_dir_all(&projection_dir).expect("the generated copy is removed");
    fs::create_dir_all(&projection_dir).expect("a replacement directory exists");
    fs::write(projection_dir.join("SKILL.md"), "# hand authored\n")
        .expect("the hand-authored manifest is written");

    let (code, body) = fixture.run_json(&["--json", "sync"]);

    assert_eq!(code, 2, "the divergence adds a warning: {body}");
    assert_eq!(body["ok"], true, "the sync still reports success: {body}");
    assert!(
        body["data"]["preserved"]
            .as_array()
            .is_none_or(Vec::is_empty),
        "a directory skillctl never generated is not preserved: {body}"
    );

    let diverged = SymlinkImportFixture::diverged_warnings(&body);
    assert_eq!(
        diverged.len(),
        1,
        "the divergence is reported once: {}",
        body["warnings"]
    );
    let warning = diverged[0].as_str().expect("the warning is text");
    assert!(
        warning.contains("no projection was written for 'release-notes'"),
        "the warning says no projection was written: {warning}"
    );
    assert!(
        warning.contains("skillctl restore --frozen"),
        "the repair repairs the store: {warning}"
    );

    assert_eq!(
        fs::read_to_string(projection_dir.join("SKILL.md"))
            .expect("the hand-authored manifest is readable"),
        "# hand authored\n",
        "the hand-authored directory stays untouched"
    );
    assert!(
        !projection_dir.join(".skillctl-projection.json").exists(),
        "sync never marks the hand-authored directory as generated"
    );
}

/// One diverged import is reported once per sync even when several planned
/// roots would project it, so the warning list names one defect instead of
/// one copy of the same line per root.
#[test]
fn a_diverged_store_without_copies_warns_once_across_planned_roots() {
    let fixture = SymlinkImportFixture::install(SYMLINK_TWO_ROOTS_MANIFEST);
    fixture.diverge_the_store(
        fixture
            .workspace
            .path()
            .join(".agents/skills/release-notes/SKILL.md"),
        "# local note",
    );

    // Remove every projected copy, the state a `skillctl clean` leaves behind.
    for root in [".agents/skills", ".claude/skills"] {
        let copy = fixture.workspace.path().join(root).join("release-notes");
        if copy.exists() {
            fs::remove_dir_all(&copy).expect("the projected copy is removed");
        }
    }

    let (code, body) = fixture.run_json(&["--json", "sync"]);

    assert_eq!(code, 2, "the divergence adds a warning: {body}");
    assert_eq!(body["ok"], true, "the sync still reports success: {body}");
    let planned_roots = body["data"]["plan"]["physical_roots"]
        .as_array()
        .expect("the plan lists physical roots");
    assert!(
        planned_roots.len() >= 2,
        "the fixture needs several planned roots: {planned_roots:?}"
    );
    assert!(
        body["data"]["preserved"]
            .as_array()
            .is_none_or(Vec::is_empty),
        "no copy exists, so nothing is preserved: {body}"
    );
    let diverged = SymlinkImportFixture::diverged_warnings(&body);
    assert_eq!(
        diverged.len(),
        1,
        "one import diverged, so its warning appears once: {}",
        body["warnings"]
    );
}

/// A lockfile pin keeps the retained store copy alive through `clean`: the
/// pin is the contract a teammate's `restore --frozen` verifies, and
/// `skillctl remove` is the explicit command that clears a pin and its copy
/// together.
#[test]
fn clean_keeps_the_store_copy_of_an_import_the_lockfile_still_pins() {
    let workspace = TestWorkspace::new();
    workspace.write_manifest_for_targets(&["claude-code"]);
    write_skill_source(&workspace, "shared-skills", "release-notes");

    let output = Command::cargo_bin("skillctl")
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
        .output()
        .expect("install runs");
    assert!(
        output.stderr.is_empty(),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "copy mode installs without warnings: {}",
        String::from_utf8_lossy(&output.stdout)
    );

    let retained_copy = workspace_import_root(&workspace, "release-notes");
    assert!(
        retained_copy.is_dir(),
        "the install retains a store copy: {}",
        retained_copy.display()
    );
    let lockfile = fs::read_to_string(workspace.path().join(".agents/skillctl.lock"))
        .expect("the lockfile is readable");
    assert!(
        lockfile.contains("release-notes"),
        "the lockfile pins the import: {lockfile}"
    );

    // Drop the manifest entry and keep the pin, the state whose only clear
    // path is `skillctl remove`.
    workspace.write_manifest_for_targets(&["claude-code"]);

    let output = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--json", "clean"])
        .output()
        .expect("clean runs");
    assert!(
        output.stderr.is_empty(),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "clean succeeds: {}",
        String::from_utf8_lossy(&output.stdout)
    );

    assert!(
        retained_copy.is_dir(),
        "a remaining pin keeps the retained copy alive through clean"
    );

    let output = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--no-input", "remove", "release-notes"])
        .output()
        .expect("remove runs");
    assert!(
        output.stderr.is_empty(),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        output.status.code(),
        Some(2),
        "remove clears the pinned entry, warning about the stale pin: {stdout}"
    );
    assert!(
        stdout.contains("Removed release-notes"),
        "the removal names the skill: {stdout}"
    );

    assert!(
        !retained_copy.exists(),
        "remove clears the pin and its copy together"
    );
    let lockfile = fs::read_to_string(workspace.path().join(".agents/skillctl.lock"))
        .expect("the lockfile stays readable");
    assert!(
        !lockfile.contains("release-notes"),
        "the pin is gone: {lockfile}"
    );
}

/// Store copy path of one workspace-scope import, the path
/// `stored_import_root` resolves for this workspace.
fn workspace_import_root(workspace: &TestWorkspace, import_id: &str) -> PathBuf {
    let canonical = fs::canonicalize(workspace.path()).expect("workspace path canonicalizes");
    let mut hasher = Sha256::new();
    hasher.update(canonical.to_string_lossy().as_bytes());
    workspace
        .home_path()
        .join(".skillctl/store/imports/workspace")
        .join(format!("{:x}", hasher.finalize()))
        .join(import_id)
}
