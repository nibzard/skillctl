use super::*;

const CLAUDE_MANIFEST: &str = concat!("version: 1\n", "\n", "targets:\n", "  - claude-code\n",);

/// A manifest that projects one skill into two distinct roots, so a copy in
/// each root can hold a different edit of the same file.
const TWO_ROOT_MANIFEST: &str = concat!(
    "version: 1\n",
    "\n",
    "targets:\n",
    "  - claude-code\n",
    "  - codex\n",
);

struct UpdateFixture {
    workspace: TestWorkspace,
    installed_revision: String,
}

impl UpdateFixture {
    fn install() -> Self {
        Self::install_with(CLAUDE_MANIFEST, &[])
    }

    /// Install with an explicit manifest and extra files committed into the
    /// skill source at the revision the install pins.
    fn install_with(manifest: &str, extra_source_files: &[(&str, &str)]) -> Self {
        let workspace = TestWorkspace::new();
        workspace.write_manifest(manifest);
        workspace.write_skill_source("git-source", "release-notes");
        for (relative_path, contents) in extra_source_files {
            workspace.write_file(
                &format!("git-source/.agents/skills/release-notes/{relative_path}"),
                contents,
            );
        }
        workspace.init_git_repo("git-source");
        let repo_url = workspace.git_repo_url("git-source");

        Command::cargo_bin("skillctl")
            .expect("binary exists")
            .current_dir(workspace.path())
            .env("HOME", workspace.home_path())
            .args([
                "--no-input",
                "--name",
                "release-notes",
                "install",
                repo_url.as_str(),
            ])
            .assert()
            .success()
            .stderr(predicate::str::is_empty());

        let installed_revision = workspace.git_head("git-source");
        Self {
            workspace,
            installed_revision,
        }
    }

    fn advance_upstream(&self, description: &str) -> String {
        self.workspace.write_skill_source_at(
            "git-source",
            ".agents/skills/release-notes",
            "release-notes",
            description,
        );
        self.workspace.commit_all("git-source", "advance upstream");
        self.workspace.git_head("git-source")
    }

    fn run(&self, args: &[&str]) -> Assert {
        Command::cargo_bin("skillctl")
            .expect("binary exists")
            .current_dir(self.workspace.path())
            .env("HOME", self.workspace.home_path())
            .args(args)
            .assert()
            .stderr(predicate::str::is_empty())
    }

    fn run_json(&self, args: &[&str]) -> Value {
        let assert = self.run(
            &["--json"]
                .iter()
                .copied()
                .chain(args.iter().copied())
                .collect::<Vec<_>>(),
        );
        serde_json::from_slice(&assert.get_output().stdout).expect("stdout is valid json")
    }

    fn edit_projected_copy(&self, note: &str) {
        self.edit_copy_at(".claude/skills/release-notes/SKILL.md", note);
    }

    /// Edit one generated copy of the skill, wherever its root projects it.
    fn edit_copy_at(&self, relative_copy: &str, note: &str) {
        fs::write(
            self.workspace.path().join(relative_copy),
            concat!(
                "---\n",
                "name: release-notes\n",
                "description: Directly edited runtime copy.\n",
                "---\n",
                "\n",
                "# release-notes\n"
            )
            .replace("# release-notes", &format!("# release-notes\n\n{note}")),
        )
        .expect("projected copy edited");
    }

    fn lockfile_digest(&self) -> String {
        let contents =
            fs::read_to_string(self.workspace.path().join(".agents/skillctl.lock")).expect("ok");
        let mut hasher = Sha256::new();
        hasher.update(contents.as_bytes());
        format!("{:x}", hasher.finalize())
    }
}

use assert_cmd::assert::Assert;

#[test]
fn update_apply_moves_to_the_new_revision_and_keeps_bundle_identity() {
    let fixture = UpdateFixture::install();
    let new_revision = fixture.advance_upstream("Updated upstream release notes helper.");

    let body = fixture.run_json(&["update", "release-notes", "--apply"]);

    assert_eq!(body["ok"], true);
    let applied = &body["data"]["applied"][0];
    assert_eq!(applied["skill"], "release-notes");
    assert_eq!(applied["previous_revision"], fixture.installed_revision);
    assert_eq!(applied["resolved_revision"], new_revision.as_str());
    assert_eq!(applied["strategy"], "clean");
    assert_eq!(applied["diff"]["modified"], json!(["SKILL.md".to_string()]));

    let lockfile = fixture.workspace.read_lockfile_yaml();
    let entry = &lockfile["imports"]["release-notes"];
    assert_eq!(
        entry["revision"]["resolved"].as_str().unwrap(),
        new_revision.as_str()
    );
    let bundle_digest = entry["bundle"]["digest"].as_str().expect("bundle exists");
    assert!(bundle_digest.starts_with("sha256:"), "digest is canonical");

    // The recorded bundle must match a fresh capture of the stored content.
    let stored_root = fixture
        .workspace
        .workspace_import_root("release-notes")
        .join(".agents/skills/release-notes");
    let manifest_path = fixture
        .workspace
        .home_path()
        .join(".skillctl/store/artifacts")
        .join(bundle_digest.trim_start_matches("sha256:"))
        .join("bundle.json");
    assert!(
        manifest_path.is_file(),
        "the applied revision is retained as an artifact"
    );

    // The retained artifact reconstructs the exact pinned content.
    let artifact_content = fixture
        .workspace
        .home_path()
        .join(".skillctl/store/artifacts")
        .join(bundle_digest.trim_start_matches("sha256:"))
        .join("content");
    let projected = fixture
        .workspace
        .path()
        .join(".claude/skills/release-notes/SKILL.md");
    assert!(
        fs::read_to_string(artifact_content.join("SKILL.md"))
            .expect("artifact content exists")
            .contains("Updated upstream release notes helper."),
        "the artifact holds the applied revision"
    );
    assert!(
        fs::read_to_string(&projected)
            .expect("projection exists")
            .contains("Updated upstream release notes helper."),
        "the projection reflects the applied revision: {stored_root:?}",
    );

    let connection =
        Connection::open(fixture.workspace.home_path().join(".skillctl/state.db")).unwrap();
    let history_kinds: Vec<String> = connection
        .prepare(
            "SELECT kind FROM history_events WHERE skill_id = ?1 ORDER BY occurred_at ASC, id ASC",
        )
        .unwrap()
        .query_map(params!["release-notes"], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert!(
        history_kinds.contains(&"update-applied".to_string()),
        "an applied update is recorded in history: {history_kinds:?}"
    );
}

#[test]
#[cfg(unix)]
fn update_apply_stores_only_the_selected_subpath_from_a_source_with_symlinks() {
    let fixture = UpdateFixture::install();

    // A symlink outside the locked subpath must not block the applied update,
    // because the store is rewritten from the selected subtree only.
    std::os::unix::fs::symlink(
        ".agents/skills/release-notes",
        fixture.workspace.path().join("git-source/latest"),
    )
    .expect("symlinked source entry is created");
    fixture
        .workspace
        .commit_all("git-source", "add a symlinked pointer");
    let new_revision = fixture.workspace.git_head("git-source");

    let body = fixture.run_json(&["update", "release-notes", "--apply"]);
    assert_eq!(body["ok"], true, "update apply failed: {body:#?}");
    assert_eq!(
        body["data"]["applied"][0]["resolved_revision"],
        new_revision.as_str()
    );

    let stored_root = fixture.workspace.workspace_import_root("release-notes");
    assert!(
        stored_root
            .join(".agents/skills/release-notes/SKILL.md")
            .is_file(),
        "the selected skill subtree is stored under its subpath",
    );
    assert!(
        !stored_root.join(".git").exists(),
        "the checked-out repository metadata must not be stored: {}",
        stored_root.display(),
    );
    assert!(
        !stored_root.join("latest").exists(),
        "source entries outside the selected skill must not be stored: {}",
        stored_root.display(),
    );
}

#[test]
fn update_apply_rolls_back_when_the_transaction_fails_after_the_state_write() {
    let fixture = UpdateFixture::install();
    fixture.advance_upstream("Updated upstream release notes helper.");

    // The update check itself is recorded before the apply, so the snapshot
    // covers the dependency state and projections rather than the ledger.
    let snapshot = fixture.workspace.snapshot_paths(&[
        ".agents/skillctl.yaml",
        ".agents/skillctl.lock",
        "home/.skillctl/store/imports",
        ".claude/skills",
    ]);

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(fixture.workspace.path())
        .env("HOME", fixture.workspace.home_path())
        .env("SKILLCTL_FAILPOINT", "update-apply:applied-skill")
        .args(["update", "release-notes", "--apply"])
        .assert()
        .failure()
        .code(1)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("injected lifecycle failure"));

    let actual = fixture.workspace.snapshot_paths(&[
        ".agents/skillctl.yaml",
        ".agents/skillctl.lock",
        "home/.skillctl/store/imports",
        ".claude/skills",
    ]);
    assert_eq!(
        &actual, &snapshot,
        "a failed apply changes no dependency state"
    );

    let connection =
        Connection::open(fixture.workspace.home_path().join(".skillctl/state.db")).unwrap();
    let applied: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM history_events WHERE skill_id = ?1 AND kind = 'update-applied'",
            params!["release-notes"],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(applied, 0, "a failed apply records no applied update");
}

#[test]
fn an_interrupted_apply_leaves_an_explicit_recoverable_operation() {
    let fixture = UpdateFixture::install();
    let upstream_revision = fixture.advance_upstream("Updated upstream release notes helper.");

    let terminated = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(fixture.workspace.path())
        .env("HOME", fixture.workspace.home_path())
        .env("SKILLCTL_TERMINATE_AT", "update-apply:applied-skill")
        .args(["update", "release-notes", "--apply"])
        .output()
        .expect("terminated command launches");
    assert_eq!(
        terminated.status.code(),
        Some(70),
        "the child must stop at the applied-skill boundary"
    );

    // The interruption leaves either the old state or an explicit recoverable
    // operation, never a silently half-applied revision.
    let preview = fixture.run_json(&["doctor", "--repair-state"]);
    assert_eq!(preview["ok"], true, "repair preview succeeds: {preview}");
    let pending = preview["data"]["pending_operations"].as_array();
    assert!(
        pending.is_some_and(|operations| operations
            .iter()
            .any(|operation| operation["operation"] == "update-apply")),
        "the pending operation names the apply: {preview}"
    );

    let applied = fixture.run_json(&["doctor", "--repair-state", "--apply"]);
    assert_eq!(applied["ok"], true, "recovery applies: {applied}");

    let lockfile = fixture.workspace.read_lockfile_yaml();
    let revision = lockfile["imports"]["release-notes"]["revision"]["resolved"]
        .as_str()
        .unwrap_or("<missing>")
        .to_string();
    assert!(
        revision == fixture.installed_revision || revision == upstream_revision,
        "recovery leaves the pin on the installed revision {installed} or the applied upstream \
         revision {upstream_revision}, not {revision}",
        installed = fixture.installed_revision,
    );
}

#[test]
fn a_clean_apply_refuses_to_replace_unrecorded_edits() {
    let fixture = UpdateFixture::install();
    fixture.advance_upstream("Updated upstream release notes helper.");
    fixture.edit_projected_copy("keep my edit");

    let before = fixture.lockfile_digest();
    let body = fixture.run_json(&["update", "release-notes", "--apply"]);

    assert_eq!(body["ok"], false);
    let error = body["errors"][0].as_str().expect("error exists");
    assert!(
        error.contains("unrecorded local edits"),
        "unexpected error: {error}"
    );
    assert!(
        error.contains("--strategy overlay") && error.contains("--strategy discard"),
        "the error names both strategies: {error}"
    );
    assert_eq!(fixture.lockfile_digest(), before, "nothing was mutated");
}

#[test]
fn update_apply_preserves_edits_as_an_overlay() {
    let fixture = UpdateFixture::install();
    let new_revision = fixture.advance_upstream("Updated upstream release notes helper.");
    fixture.edit_projected_copy("keep my edit");

    let body = fixture.run_json(&[
        "update",
        "release-notes",
        "--apply",
        "--strategy",
        "overlay",
    ]);

    assert_eq!(body["ok"], true);
    let preserved = &body["data"]["preserved"];
    assert!(
        preserved
            .as_array()
            .expect("preserved list exists")
            .iter()
            .any(|entry| entry["reason"] == "moved-to-overlay"),
        "the edit is reported as overlay content: {preserved}"
    );

    let overlay = fixture
        .workspace
        .path()
        .join(".agents/overlays/release-notes/SKILL.md");
    assert!(
        fs::read_to_string(&overlay)
            .expect("overlay file exists")
            .contains("keep my edit"),
        "the edit survives in the managed overlay"
    );

    let lockfile = fixture.workspace.read_lockfile_yaml();
    let entry = &lockfile["imports"]["release-notes"];
    assert_eq!(
        entry["revision"]["resolved"].as_str().unwrap(),
        new_revision.as_str(),
        "the new revision was applied"
    );
    assert_ne!(
        entry["hashes"]["overlay"].as_str().unwrap(),
        "sha256:none",
        "the overlay is part of the effective version"
    );

    assert!(
        fs::read_to_string(
            fixture
                .workspace
                .path()
                .join(".claude/skills/release-notes/SKILL.md")
        )
        .expect("projection exists")
        .contains("keep my edit"),
        "the projected copy still shows the preserved edit"
    );
}

/// Two roots can hold two different edits of the same file. Both copies share
/// one overlay, so the move must keep both variants: the winner lives in the
/// overlay and drives the rebuilt copies, and the loser is backed up durably
/// and reported, never overwritten silently.
#[test]
fn overlay_preservation_keeps_both_variants_of_conflicting_edits() {
    let fixture = UpdateFixture::install_with(TWO_ROOT_MANIFEST, &[]);
    fixture.advance_upstream("Updated upstream release notes helper.");
    fixture.edit_copy_at(".claude/skills/release-notes/SKILL.md", "claude root edit");
    fixture.edit_copy_at(".agents/skills/release-notes/SKILL.md", "neutral root edit");

    let body = fixture.run_json(&[
        "update",
        "release-notes",
        "--apply",
        "--strategy",
        "overlay",
    ]);
    assert_eq!(body["ok"], true, "the apply keeps both variants: {body:#?}");

    let overlay = fs::read_to_string(
        fixture
            .workspace
            .path()
            .join(".agents/overlays/release-notes/SKILL.md"),
    )
    .expect("the overlay file exists");
    let claude_wins = overlay.contains("claude root edit");
    let neutral_wins = overlay.contains("neutral root edit");
    assert!(
        claude_wins ^ neutral_wins,
        "the overlay holds exactly one variant: {overlay}"
    );
    let (winning_note, losing_note) = if claude_wins {
        ("claude root edit", "neutral root edit")
    } else {
        ("neutral root edit", "claude root edit")
    };

    let discarded = body["data"]["discarded"]
        .as_array()
        .expect("the discarded list exists");
    let replaced = discarded
        .iter()
        .find(|entry| {
            entry["path"]
                .as_str()
                .is_some_and(|path| path.ends_with("overlays/release-notes/SKILL.md"))
        })
        .expect("the replaced overlay variant is reported")
        .clone();
    assert!(
        replaced["skill"] == "release-notes",
        "the replaced variant names its skill: {replaced}"
    );
    let backup = replaced["backup"].as_str().expect("the backup path exists");
    assert!(
        backup.contains(".skillctl/store/discards"),
        "the replaced variant lives in the durable store: {backup}"
    );
    assert!(
        fs::read_to_string(PathBuf::from(backup).join("SKILL.md"))
            .expect("the backed-up overlay variant exists")
            .contains(losing_note),
        "the losing edit stays recoverable at {backup}"
    );

    for copy in [
        ".claude/skills/release-notes/SKILL.md",
        ".agents/skills/release-notes/SKILL.md",
    ] {
        let contents = fs::read_to_string(fixture.workspace.path().join(copy))
            .expect("the rebuilt copy exists");
        assert!(
            contents.contains(winning_note) && !contents.contains(losing_note),
            "the rebuilt copy at {copy} materializes the overlay variant: {contents}"
        );
    }
}

/// The managed overlay can only replace imported files, so a copy that holds a
/// file the import never provided must be refused before the in-transaction
/// sync meets an overlay it cannot resolve. The refusal names the file and the
/// fork strategy that keeps it.
#[test]
fn the_overlay_strategy_refuses_files_the_import_does_not_provide() {
    let fixture = UpdateFixture::install();
    fixture.advance_upstream("Updated upstream release notes helper.");
    fixture
        .workspace
        .write_file(".claude/skills/release-notes/notes.md", "# local notes\n");
    let before = fixture.lockfile_digest();

    let body = fixture.run_json(&[
        "update",
        "release-notes",
        "--apply",
        "--strategy",
        "overlay",
    ]);

    assert_eq!(body["ok"], false);
    let error = body["errors"][0].as_str().expect("error exists");
    assert!(
        error.contains("notes.md"),
        "the refusal names the file it cannot map: {error}"
    );
    assert!(
        error.contains("update release-notes --apply --strategy fork"),
        "the refusal names the strategy that keeps the file: {error}"
    );
    assert_eq!(
        fixture.lockfile_digest(),
        before,
        "the refused apply mutates nothing"
    );
    assert!(
        fs::read_to_string(
            fixture
                .workspace
                .path()
                .join(".claude/skills/release-notes/notes.md")
        )
        .expect("the copy is restored")
        .contains("local notes"),
        "the unrepresentable file is not dropped"
    );
    assert!(
        !fixture
            .workspace
            .path()
            .join(".agents/overlays/release-notes")
            .exists(),
        "no poisoned overlay survives the refusal"
    );
}

/// An overlay entry replaces a file, so a deleted file cannot survive the
/// move: the copy would be rebuilt and the file would return without a word.
/// The refusal names the deletion instead of silently reverting the edit.
#[test]
fn the_overlay_strategy_refuses_files_the_copy_deleted() {
    let fixture = UpdateFixture::install_with(
        CLAUDE_MANIFEST,
        &[("reference.md", "release note reference\n")],
    );
    fixture.advance_upstream("Updated upstream release notes helper.");
    fs::remove_file(
        fixture
            .workspace
            .path()
            .join(".claude/skills/release-notes/reference.md"),
    )
    .expect("the projected reference is deleted");
    let before = fixture.lockfile_digest();

    let body = fixture.run_json(&[
        "update",
        "release-notes",
        "--apply",
        "--strategy",
        "overlay",
    ]);

    assert_eq!(body["ok"], false);
    let error = body["errors"][0].as_str().expect("error exists");
    assert!(
        error.contains("reference.md"),
        "the refusal names the deleted file: {error}"
    );
    assert!(
        error.contains("deletes files"),
        "the refusal explains what the copy did: {error}"
    );
    assert_eq!(
        fixture.lockfile_digest(),
        before,
        "the refused apply mutates nothing"
    );
    assert!(
        !fixture
            .workspace
            .path()
            .join(".claude/skills/release-notes/reference.md")
            .exists(),
        "the deletion edit is not silently reverted while refused"
    );
}

/// A file the import never provided cannot live in a managed overlay, but a
/// fork detaches into local canonical ownership, so the overlay only ferries
/// the file into the forked copy. The fork must keep it.
#[test]
fn a_fork_keeps_files_the_import_does_not_provide() {
    let fixture = UpdateFixture::install();
    fixture.advance_upstream("Updated upstream release notes helper.");
    fixture
        .workspace
        .write_file(".claude/skills/release-notes/notes.md", "# local notes\n");

    let body = fixture.run_json(&["update", "release-notes", "--apply", "--strategy", "fork"]);

    assert_eq!(body["ok"], true, "the fork applies: {body:#?}");
    assert!(
        fs::read_to_string(
            fixture
                .workspace
                .path()
                .join(".agents/skills/release-notes/notes.md")
        )
        .expect("the forked copy keeps the file")
        .contains("local notes"),
        "a user-created file survives the fork"
    );
    assert!(
        fixture.workspace.read_lockfile_yaml()["imports"]["release-notes"].is_null(),
        "forking releases the managed import"
    );
}

/// An overlay entry replaces a file, so a deleted file cannot ride a fork's
/// overlay ferry either: the fork rebuilds the copy from the stored import and
/// the file would return without a word. Both fork entries must give this
/// input the same refusal the overlay strategy gives it.
#[test]
fn a_fork_refuses_edits_that_delete_imported_files() {
    let fixture = UpdateFixture::install_with(
        CLAUDE_MANIFEST,
        &[("reference.md", "release note reference\n")],
    );
    fs::remove_file(
        fixture
            .workspace
            .path()
            .join(".claude/skills/release-notes/reference.md"),
    )
    .expect("the projected reference is deleted");
    let before = fixture.lockfile_digest();

    let body = fixture.run_json(&["fork", "release-notes"]);

    assert_eq!(
        body["ok"], false,
        "the direct fork refuses the deletion: {body:#?}"
    );
    let error = body["errors"][0].as_str().expect("error exists");
    assert!(
        error.contains("reference.md") && error.contains("deletes files"),
        "the refusal names the deleted file: {error}"
    );
    assert_eq!(
        fixture.lockfile_digest(),
        before,
        "the refused fork mutates nothing"
    );
    assert!(
        !fixture
            .workspace
            .path()
            .join(".agents/overlays/release-notes")
            .exists(),
        "no overlay survives the refusal"
    );

    // The same state through the update fork must hear the same refusal.
    fixture.advance_upstream("Updated upstream release notes helper.");
    let body = fixture.run_json(&["update", "release-notes", "--apply", "--strategy", "fork"]);
    assert_eq!(body["ok"], false, "the update fork refuses too: {body:#?}");
    let error = body["errors"][0].as_str().expect("error exists");
    assert!(
        error.contains("reference.md") && error.contains("deletes files"),
        "both fork entries share one refusal: {error}"
    );
}

/// A revision that deletes a file an overlay edits would leave an overlay no
/// sync can resolve, and the rollback would restore a tree where nothing looks
/// wrong. Every strategy must refuse up front and name the overlay file.
#[test]
fn an_update_refuses_a_revision_that_drops_an_overlay_edited_file() {
    let fixture = UpdateFixture::install_with(
        CLAUDE_MANIFEST,
        &[("templates/header.md", "# upstream header\n")],
    );
    fixture.run(&["override", "release-notes"]).success();
    fixture.workspace.write_file(
        ".agents/overlays/release-notes/templates/header.md",
        "# team header\n",
    );
    fixture.run(&["override", "release-notes"]).success();

    fs::remove_file(
        fixture
            .workspace
            .path()
            .join("git-source/.agents/skills/release-notes/templates/header.md"),
    )
    .expect("the upstream template is removed");
    fixture
        .workspace
        .commit_all("git-source", "remove the template upstream");

    let before = fixture.lockfile_digest();
    for strategy in ["clean", "overlay", "discard"] {
        let body =
            fixture.run_json(&["update", "release-notes", "--apply", "--strategy", strategy]);
        assert_eq!(
            body["ok"], false,
            "the {strategy} strategy must refuse the move: {body:#?}"
        );
        let error = body["errors"][0].as_str().expect("error exists");
        assert!(
            error.contains("templates/header.md")
                && error.contains("no longer contains")
                && error.contains("skillctl fork release-notes"),
            "the {strategy} refusal names the overlay file and its exits: {error}"
        );
    }

    assert_eq!(
        fixture.lockfile_digest(),
        before,
        "the refused moves leave the pin alone"
    );
    assert!(
        fs::read_to_string(
            fixture
                .workspace
                .path()
                .join(".agents/overlays/release-notes/templates/header.md")
        )
        .expect("the overlay edit survives")
        .contains("team header"),
        "the overlay edit is not dropped while refused"
    );
}

#[test]
fn update_apply_discards_edits_only_after_a_durable_backup() {
    let fixture = UpdateFixture::install();
    let new_revision = fixture.advance_upstream("Updated upstream release notes helper.");
    fixture.edit_projected_copy("temporary edit");

    let body = fixture.run_json(&[
        "update",
        "release-notes",
        "--apply",
        "--strategy",
        "discard",
    ]);

    assert_eq!(body["ok"], true);
    let discarded = &body["data"]["discarded"][0];
    let backup = discarded["backup"].as_str().expect("backup path exists");
    assert!(
        backup.contains(".skillctl/store/discards"),
        "the backup lives in the durable discard store: {backup}"
    );
    assert!(
        fs::read_to_string(PathBuf::from(backup).join("SKILL.md"))
            .expect("backed-up content exists")
            .contains("temporary edit"),
        "the discarded edit is recoverable from the backup"
    );

    let projected = fs::read_to_string(
        fixture
            .workspace
            .path()
            .join(".claude/skills/release-notes/SKILL.md"),
    )
    .expect("projection exists");
    assert!(
        projected.contains("Updated upstream release notes helper."),
        "the projection now holds the applied revision"
    );
    assert!(!projected.contains("temporary edit"));

    let lockfile = fixture.workspace.read_lockfile_yaml();
    assert_eq!(
        lockfile["imports"]["release-notes"]["revision"]["resolved"]
            .as_str()
            .unwrap(),
        new_revision.as_str()
    );

    let connection =
        Connection::open(fixture.workspace.home_path().join(".skillctl/state.db")).unwrap();
    let summaries: Vec<String> = connection
        .prepare(
            "SELECT summary FROM history_events WHERE skill_id = ?1 AND kind = 'cleanup' \
             ORDER BY id ASC",
        )
        .unwrap()
        .query_map(params!["release-notes"], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert!(
        summaries
            .iter()
            .any(|summary| summary.contains("backed up at")),
        "the discard is auditable in history: {summaries:?}"
    );
}

#[test]
fn rollback_returns_to_the_release_recorded_before_an_apply() {
    let fixture = UpdateFixture::install();
    fixture.advance_upstream("Updated upstream release notes helper.");
    fixture.run(&["update", "release-notes", "--apply"]);

    let body = fixture.run_json(&[
        "rollback",
        "release-notes",
        fixture.installed_revision.as_str(),
    ]);
    assert_eq!(body["ok"], true);
    assert_eq!(
        body["data"]["resolved_revision"],
        fixture.installed_revision.as_str()
    );

    let projected = fs::read_to_string(
        fixture
            .workspace
            .path()
            .join(".claude/skills/release-notes/SKILL.md"),
    )
    .expect("projection exists");
    assert!(
        projected.contains("Summarize release notes."),
        "the rolled-back projection holds the earlier release: {projected}"
    );
}

#[test]
fn a_blocked_skill_contributes_a_plan_and_the_apply_refuses_loudly() {
    let fixture = UpdateFixture::install();
    let new_revision = fixture.advance_upstream("Updated upstream release notes helper.");
    fixture.edit_projected_copy("keep my edit");
    let plan_path = fixture.workspace.path().join("update-plan.json");
    let before = fixture.lockfile_digest();

    let assert = fixture.run(&[
        "--json",
        "update",
        "release-notes",
        "--plan",
        "update-plan.json",
    ]);
    let output = assert.get_output().stdout.clone();
    assert.code(2);
    let body: Value = serde_json::from_slice(&output).expect("stdout is valid json");
    assert_eq!(
        body["data"]["plans"].as_array().map(Vec::len),
        Some(1),
        "the blocked skill contributes a plan instead of an empty document: {body}",
    );
    let warnings = body["warnings"].as_array().expect("warnings exist");
    assert!(
        warnings.iter().any(|warning| {
            let warning = warning.as_str().unwrap_or_default();
            warning.contains("release-notes")
                && warning.contains("--strategy overlay")
                && warning.contains("--strategy discard")
        }),
        "the warning names the strategies that resolve the edits: {warnings:?}",
    );

    let document: Value =
        serde_json::from_str(&fs::read_to_string(&plan_path).expect("plan document exists"))
            .expect("plan document is valid json");
    assert_eq!(document["strategy"], "clean");
    assert_eq!(document["plans"].as_array().map(Vec::len), Some(1));
    assert_eq!(
        document["plans"][0]["proposed"][0]["revision"],
        new_revision.as_str(),
        "the document records the revision move: {document}",
    );
    assert_eq!(
        fixture.lockfile_digest(),
        before,
        "planning changes nothing"
    );

    // The apply of that document refuses instead of reporting a silent no-op.
    let body = fixture.run_json(&["update", "--apply", "--plan-file", "update-plan.json"]);
    assert_eq!(body["ok"], false, "a clean apply cannot resolve the edits");
    let error = body["errors"][0].as_str().expect("error exists");
    assert!(
        error.contains("unrecorded local edits") && error.contains("--strategy overlay"),
        "unexpected error: {error}",
    );
    assert_eq!(fixture.lockfile_digest(), before, "nothing was mutated");
}

#[test]
fn an_empty_plan_document_apply_warns_instead_of_succeeding_silently() {
    let fixture = UpdateFixture::install();
    let plan_path = fixture.workspace.path().join("update-plan.json");

    // An up-to-date skill leaves the document with nothing to plan.
    fixture
        .run(&["update", "release-notes", "--plan", "update-plan.json"])
        .success();
    let document: Value =
        serde_json::from_str(&fs::read_to_string(&plan_path).expect("plan document exists"))
            .expect("plan document is valid json");
    assert_eq!(document["plans"].as_array().map(Vec::len), Some(0));

    let assert = fixture.run(&[
        "--json",
        "update",
        "--apply",
        "--plan-file",
        "update-plan.json",
    ]);
    let output = assert.get_output().stdout.clone();
    assert.code(2);
    let body: Value = serde_json::from_slice(&output).expect("stdout is valid json");
    assert_eq!(body["data"]["applied"], json!([]));
    assert_eq!(body["data"]["apply_skipped"], json!([]));
    assert!(
        body["warnings"]
            .as_array()
            .expect("warnings exist")
            .iter()
            .any(|warning| warning
                .as_str()
                .unwrap_or_default()
                .contains("contains no plans")),
        "the no-op apply is visible: {}",
        body["warnings"],
    );
}

#[test]
fn a_stale_plan_fails_before_any_mutation() {
    let fixture = UpdateFixture::install();
    let new_revision = fixture.advance_upstream("Updated upstream release notes helper.");
    let plan_path = fixture.workspace.path().join("update-plan.json");

    let body = fixture.run_json(&["update", "--plan", "update-plan.json"]);
    assert_eq!(body["ok"], true);
    let plan_document: Value =
        serde_json::from_str(&fs::read_to_string(&plan_path).expect("plan document exists"))
            .expect("plan document is valid json");
    assert_eq!(
        plan_document["plans"][0]["expected"][0]["revision"],
        fixture.installed_revision.as_str()
    );

    // Local state moves after planning: the plan is now stale.
    fixture
        .run(&["pin", "release-notes", new_revision.as_str()])
        .success();
    let before = fixture.lockfile_digest();

    let body = fixture.run_json(&["update", "--apply", "--plan-file", "update-plan.json"]);
    assert_eq!(body["ok"], false);
    let error = body["errors"][0].as_str().expect("error exists");
    assert!(
        error.contains("is stale") && error.contains("release-notes"),
        "unexpected error: {error}"
    );
    assert_eq!(
        fixture.lockfile_digest(),
        before,
        "a stale plan mutates nothing"
    );
}

#[test]
fn a_tampered_plan_document_is_rejected() {
    let fixture = UpdateFixture::install();
    fixture.advance_upstream("Updated upstream release notes helper.");
    let plan_path = fixture.workspace.path().join("update-plan.json");
    fixture.run(&["update", "--plan", "update-plan.json"]);

    let mut document: Value =
        serde_json::from_str(&fs::read_to_string(&plan_path).expect("plan document exists"))
            .expect("plan document is valid json");
    document["plans"][0]["proposed"][0]["revision"] =
        json!("0000000000000000000000000000000000000000");
    fs::write(&plan_path, serde_json::to_string_pretty(&document).unwrap())
        .expect("tampered plan written");

    let body = fixture.run_json(&["update", "--apply", "--plan-file", "update-plan.json"]);
    assert_eq!(body["ok"], false);
    let error = body["errors"][0].as_str().expect("error exists");
    assert!(
        error.contains("is tampered") && error.contains("digests to"),
        "unexpected error: {error}"
    );
    assert!(
        !fixture
            .workspace
            .path()
            .join(".agents/overlays")
            .join("release-notes")
            .exists()
    );
}

#[test]
fn update_apply_reports_skills_without_an_available_update_as_skipped() {
    let fixture = UpdateFixture::install();

    let body = fixture.run_json(&["update", "release-notes", "--apply"]);

    assert_eq!(body["ok"], true);
    assert_eq!(body["data"]["applied"], json!([]));
    let skipped = &body["data"]["apply_skipped"][0];
    assert_eq!(skipped["skill"], "release-notes");
    assert!(
        skipped["reason"]
            .as_str()
            .expect("reason exists")
            .contains("up-to-date"),
        "unexpected reason: {skipped}"
    );
}

#[test]
fn a_clean_apply_skips_a_forked_skill_instead_of_refusing() {
    let fixture = UpdateFixture::install();

    fixture.run(&["--json", "fork", "release-notes"]).success();
    let before = fixture.lockfile_digest();
    // Upstream moving must not turn the fork into something a clean apply owns.
    fixture.advance_upstream("Updated upstream release notes helper.");

    let assert = fixture
        .run(&["--json", "update", "release-notes", "--apply"])
        .code(2);
    let body: Value =
        serde_json::from_slice(&assert.get_output().stdout).expect("stdout is valid json");

    assert_eq!(
        body["ok"], true,
        "a forked skill must not fail a clean apply: {body:#?}",
    );
    assert_eq!(body["errors"], json!([]));
    assert_eq!(body["data"]["applied"], json!([]));
    assert_eq!(
        body["data"]["plans"][0]["outcome"], "detached",
        "the fork is still reported as detached: {body:#?}",
    );

    let skipped = &body["data"]["apply_skipped"][0];
    assert_eq!(skipped["skill"], "release-notes");
    assert!(
        skipped["reason"]
            .as_str()
            .expect("reason exists")
            .contains("detached"),
        "unexpected reason: {skipped}",
    );
    let warnings = body["warnings"]
        .as_array()
        .expect("warnings array exists")
        .iter()
        .filter_map(|warning| warning.as_str())
        .collect::<Vec<_>>();
    assert!(
        warnings
            .iter()
            .any(|warning| warning.contains("changed nothing")),
        "the skip must be reported as a warning: {warnings:?}",
    );
    assert!(
        warnings
            .iter()
            .all(|warning| !warning.contains("'--strategy fork'")),
        "nothing advises forking a skill that is already forked: {warnings:?}",
    );

    assert_eq!(
        fixture.lockfile_digest(),
        before,
        "the skipped fork is not mutated",
    );
    assert!(
        fixture
            .workspace
            .path()
            .join(".agents/skills/release-notes/SKILL.md")
            .is_file(),
        "the local canonical copy still exists",
    );
}

#[test]
fn a_forked_skill_does_not_block_a_clean_apply_for_other_skills() {
    let workspace = TestWorkspace::new();
    workspace.write_manifest(CLAUDE_MANIFEST);
    workspace.write_skill_source("git-source", "release-notes");
    workspace.write_skill_source("planner-source", "release-planner");
    workspace.init_git_repo("git-source");
    workspace.init_git_repo("planner-source");

    for (skill, source) in [
        ("release-notes", "git-source"),
        ("release-planner", "planner-source"),
    ] {
        Command::cargo_bin("skillctl")
            .expect("binary exists")
            .current_dir(workspace.path())
            .env("HOME", workspace.home_path())
            .args([
                "--no-input",
                "--name",
                skill,
                "install",
                workspace.git_repo_url(source).as_str(),
            ])
            .assert()
            .success()
            .stderr(predicate::str::is_empty());
    }

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--json", "fork", "release-notes"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    workspace.write_skill_source_at(
        "planner-source",
        ".agents/skills/release-planner",
        "release-planner",
        "Updated upstream release planner helper.",
    );
    workspace.commit_all("planner-source", "advance upstream");
    let new_revision = workspace.git_head("planner-source");

    // A workspace run must reach every skill, because the refusal scans the
    // whole plan list and not only the skill an apply targets.
    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--json", "--scope", "workspace", "update", "--apply"])
        .assert()
        .code(0)
        .stderr(predicate::str::is_empty());
    let body: Value =
        serde_json::from_slice(&assert.get_output().stdout).expect("stdout is valid json");

    assert_eq!(body["ok"], true, "the clean apply runs: {body:#?}");
    assert_eq!(
        body["errors"],
        json!([]),
        "the forked skill refuses nothing: {body:#?}",
    );
    let applied = body["data"]["applied"]
        .as_array()
        .expect("applied list exists");
    assert_eq!(
        applied
            .iter()
            .map(|entry| entry["skill"].as_str())
            .collect::<Vec<_>>(),
        vec![Some("release-planner")],
        "the other skill's update is applied: {body:#?}",
    );
    assert_eq!(
        applied[0]["resolved_revision"],
        new_revision.as_str(),
        "the applied skill reaches upstream: {body:#?}",
    );
    assert!(
        body["data"]["apply_skipped"]
            .as_array()
            .expect("skipped list exists")
            .iter()
            .any(|skipped| skipped["skill"] == "release-notes"
                && skipped["reason"]
                    .as_str()
                    .expect("reason exists")
                    .contains("detached")),
        "the fork is reported as skipped: {body:#?}",
    );
    assert_eq!(
        body["warnings"],
        json!([]),
        "a run that applied something warns about no skip: {body:#?}",
    );

    let lockfile = workspace.read_lockfile_yaml();
    assert!(
        lockfile["imports"]["release-notes"].is_null(),
        "the fork stays released: {lockfile:?}",
    );
    assert_eq!(
        lockfile["imports"]["release-planner"]["revision"]["resolved"]
            .as_str()
            .expect("resolved revision exists"),
        new_revision.as_str(),
        "the applied skill moves upstream: {lockfile:?}",
    );
}

/// The fork's overlay ferry rides the fork's own guarded transaction. A fork
/// that fails after the preservation — here because the canonical destination
/// is a hand-authored directory — must roll the overlay and its manifest
/// declaration back, because a declared overlay holding a file with no
/// imported base refuses every graph-resolving command for the whole
/// workspace.
#[test]
fn a_failed_fork_leaves_no_declared_overlay_behind() {
    let fixture = UpdateFixture::install_with(TWO_ROOT_MANIFEST, &[]);
    fixture.edit_copy_at(".claude/skills/release-notes/SKILL.md", "keep my edit");

    // A hand-authored canonical destination makes the fork refuse after the
    // preservation step has ferried the edit into a fresh overlay.
    let canonical = fixture
        .workspace
        .path()
        .join(".agents/skills/release-notes");
    let metadata = canonical.join(".skillctl-projection.json");
    assert!(
        metadata.is_file(),
        "the generated canonical copy exists before it is made hand-authored"
    );
    fs::remove_file(&metadata).expect("projection metadata is removed");
    let before = fixture.lockfile_digest();

    let body = fixture.run_json(&["fork", "release-notes"]);

    assert_eq!(
        body["ok"], false,
        "the fork refuses the hand-authored destination: {body:#?}"
    );
    let error = body["errors"][0].as_str().expect("error exists");
    assert!(
        error.contains("hand-authored"),
        "the refusal names the hand-authored destination: {error}"
    );
    assert!(
        !fixture
            .workspace
            .path()
            .join(".agents/overlays/release-notes")
            .exists(),
        "the ferried overlay is rolled back with the failed fork"
    );
    let manifest = fs::read_to_string(fixture.workspace.path().join(".agents/skillctl.yaml"))
        .expect("the manifest is readable");
    assert!(
        !manifest.contains("overrides"),
        "the failed fork declares no overlay: {manifest}"
    );
    assert_eq!(
        fixture.lockfile_digest(),
        before,
        "the failed fork leaves the pin alone"
    );

    // The workspace still resolves, so nothing dead-ends the next command.
    let listed = fixture.run_json(&["list"]);
    assert_eq!(
        listed["ok"], true,
        "the workspace still resolves: {listed:#?}"
    );
}

#[test]
fn update_apply_can_fork_into_local_ownership_instead_of_applying() {
    let fixture = UpdateFixture::install();
    fixture.advance_upstream("Updated upstream release notes helper.");

    let body = fixture.run_json(&["update", "release-notes", "--apply", "--strategy", "fork"]);

    assert_eq!(body["ok"], true);
    let applied = &body["data"]["applied"][0];
    assert_eq!(applied["skill"], "release-notes");
    assert_eq!(applied["strategy"], "fork");

    let lockfile = fixture.workspace.read_lockfile_yaml();
    assert!(
        lockfile["imports"]["release-notes"].is_null(),
        "forking releases the managed import: {lockfile:?}"
    );
    assert!(
        fixture
            .workspace
            .path()
            .join(".agents/skills/release-notes/SKILL.md")
            .is_file(),
        "the local canonical copy exists"
    );
}

#[test]
fn fork_keeps_edits_when_the_projection_root_is_the_canonical_root() {
    // The neutral layout projects imports into `.agents/skills`, which is also
    // the fork destination, so a fork must not drop the edits of the copy it
    // replaces.
    let workspace = TestWorkspace::new();
    workspace.write_manifest(concat!(
        "version: 1\n",
        "\n",
        "targets:\n",
        "  - codex\n",
        "  - gemini-cli\n",
        "  - opencode\n",
    ));
    workspace.write_skill_source("git-source", "release-notes");
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
        .stderr(predicate::str::is_empty());

    let projected = workspace
        .path()
        .join(".agents/skills/release-notes/SKILL.md");
    let mut contents = fs::read_to_string(&projected).expect("projected copy exists");
    contents.push_str("\nkeep my edit\n");
    fs::write(&projected, contents).expect("projected copy edited");
    workspace.write_skill_source_at(
        "git-source",
        ".agents/skills/release-notes",
        "release-notes",
        "Updated upstream release notes helper.",
    );
    workspace.commit_all("git-source", "advance upstream");

    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args([
            "--json",
            "update",
            "release-notes",
            "--apply",
            "--strategy",
            "fork",
        ])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    let body: Value =
        serde_json::from_slice(&assert.get_output().stdout).expect("stdout is valid json");
    assert_eq!(body["ok"], true, "the fork applies: {body}");

    assert!(
        fs::read_to_string(&projected)
            .expect("the forked copy exists")
            .contains("keep my edit"),
        "forking keeps the unrecorded edits"
    );
    assert!(
        workspace.read_lockfile_yaml()["imports"]["release-notes"].is_null(),
        "forking releases the managed import"
    );
}

/// The documented update-then-customize order: install at an earlier revision,
/// apply an update, then overlay a local edit and re-pin it.
///
/// Rolling back to the revision the overlay predates used to dead-end with
/// "recorded effective version ... no longer matches the current overlay
/// state", while `pin` performed the same move. See T122.
struct UpdatedThenCustomizedFixture {
    fixture: UpdateFixture,
    /// Revision the import was installed at, before the applied update.
    previous_revision: String,
    /// Revision the applied update moved the import to.
    applied_revision: String,
}

impl UpdatedThenCustomizedFixture {
    fn new() -> Self {
        let fixture = UpdateFixture::install();
        let previous_revision = fixture.installed_revision.clone();
        let applied_revision = fixture.advance_upstream("Updated upstream release notes helper.");

        fixture
            .run(&["update", "release-notes", "--apply"])
            .success();
        fixture.run(&["override", "release-notes"]).success();
        fixture.workspace.write_file(
            ".agents/overlays/release-notes/SKILL.md",
            concat!(
                "---\n",
                "name: release-notes\n",
                "description: Team-tailored release notes skill.\n",
                "---\n",
                "\n",
                "# Release notes, team edition\n",
            ),
        );
        fixture.run(&["override", "release-notes"]).success();

        Self {
            fixture,
            previous_revision,
            applied_revision,
        }
    }
}

#[test]
fn rollback_restores_a_revision_recorded_before_the_current_overlay() {
    let state = UpdatedThenCustomizedFixture::new();
    let workspace = &state.fixture.workspace;

    let body = state.fixture.run_json(&[
        "rollback",
        "release-notes",
        state.previous_revision.as_str(),
    ]);
    assert_eq!(
        body["ok"], true,
        "rollback must restore a revision the overlay predates: {body:#?}"
    );
    assert_eq!(
        body["data"]["resolved_revision"],
        state.previous_revision.as_str()
    );
    let restored_effective_version = body["data"]["effective_version_hash"]
        .as_str()
        .expect("the restored effective version is reported")
        .to_string();
    let recorded_effective_version = body["data"]["recorded_effective_version"]
        .as_str()
        .expect("the recorded effective version is reported");
    assert_ne!(
        restored_effective_version, recorded_effective_version,
        "the overlay edit changes the effective version of that revision"
    );

    let lockfile = workspace.read_lockfile_yaml();
    let entry = &lockfile["imports"]["release-notes"];
    assert_eq!(
        entry["revision"]["resolved"].as_str().unwrap(),
        state.previous_revision.as_str()
    );
    assert_eq!(
        entry["hashes"]["effective_version"].as_str().unwrap(),
        restored_effective_version.as_str()
    );

    assert!(
        fs::read_to_string(
            workspace
                .workspace_import_root("release-notes")
                .join(".agents/skills/release-notes/SKILL.md")
        )
        .expect("the stored import exists")
        .contains("Summarize release notes."),
        "the stored import holds the earlier release again"
    );

    let overlay = fs::read_to_string(
        workspace
            .path()
            .join(".agents/overlays/release-notes/SKILL.md"),
    )
    .expect("the overlay edit survives the rollback");
    assert!(overlay.contains("team edition"));
    assert!(
        fs::read_to_string(
            workspace
                .path()
                .join(".claude/skills/release-notes/SKILL.md")
        )
        .expect("the projection exists")
        .contains("team edition"),
        "the projection materializes the earlier release with the current overlay"
    );

    let connection = Connection::open(workspace.home_path().join(".skillctl/state.db")).unwrap();
    let install_row: (String, String) = connection
        .query_row(
            "SELECT resolved_revision, effective_version_hash \
             FROM install_records WHERE skill_id = ?1",
            params!["release-notes"],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("install record exists");
    assert_eq!(install_row.0, state.previous_revision);
    assert_eq!(install_row.1, restored_effective_version);

    let rollbacks: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM rollback_records WHERE skill_id = ?1 AND to_reference = ?2",
            params!["release-notes", state.previous_revision.as_str()],
            |row| row.get(0),
        )
        .expect("rollback records are readable");
    assert_eq!(rollbacks, 1, "the rollback is recorded in local history");
}

#[test]
fn rollback_and_pin_perform_the_identical_transition_to_a_pre_overlay_revision() {
    let state = UpdatedThenCustomizedFixture::new();
    let workspace = &state.fixture.workspace;

    let rollback = state.fixture.run_json(&[
        "rollback",
        "release-notes",
        state.previous_revision.as_str(),
    ]);
    assert_eq!(rollback["ok"], true, "the rollback applies: {rollback:#?}");
    let rolled_back_lockfile = fs::read_to_string(workspace.path().join(".agents/skillctl.lock"))
        .expect("lockfile is readable");

    // Move away with pin, then return to the same revision with pin alone. The
    // resulting state is what `pin` builds for that revision, so it must match
    // what `rollback` built above.
    state
        .fixture
        .run(&["pin", "release-notes", state.applied_revision.as_str()])
        .success();
    let pin = state
        .fixture
        .run_json(&["pin", "release-notes", state.previous_revision.as_str()]);
    assert_eq!(pin["ok"], true, "the pin applies: {pin:#?}");
    let pinned_lockfile = fs::read_to_string(workspace.path().join(".agents/skillctl.lock"))
        .expect("lockfile is readable");

    assert_eq!(
        rollback["data"]["effective_version_hash"], pin["data"]["effective_version_hash"],
        "rollback and pin record the same effective version"
    );
    assert_eq!(
        rollback["data"]["resolved_revision"],
        pin["data"]["resolved_revision"]
    );
    assert_eq!(
        rolled_back_lockfile, pinned_lockfile,
        "rollback and pin leave the lockfile in the same state"
    );
}

#[test]
fn an_applied_update_leaves_the_tui_dashboard_consistent_with_the_install() {
    let fixture = UpdateFixture::install();
    let previous_revision = fixture.installed_revision.clone();
    let new_revision = fixture.advance_upstream("Updated upstream release notes helper.");

    let body = fixture.run_json(&["update", "release-notes", "--apply"]);
    assert_eq!(body["ok"], true);
    assert_eq!(
        body["data"]["applied"][0]["resolved_revision"],
        new_revision.as_str()
    );

    // The apply moves the recorded pin with the import instead of leaving it on
    // the revision the install no longer holds.
    let connection =
        Connection::open(fixture.workspace.home_path().join(".skillctl/state.db")).unwrap();
    let pin_row: (String, String, String) = connection
        .query_row(
            "SELECT requested_reference, resolved_revision, effective_version_hash \
             FROM pins WHERE skill_id = ?1",
            params!["release-notes"],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("pin record exists");
    assert_eq!(pin_row.0, "HEAD");
    assert_eq!(pin_row.1, new_revision);
    assert_eq!(
        pin_row.2,
        body["data"]["applied"][0]["effective_version_hash"]
            .as_str()
            .expect("the apply reports an effective version"),
        "the pin records the effective version the apply installed"
    );

    // The stored check keeps the observation the apply acted on; the dashboard
    // fits it to the installed revision instead of replaying it.
    let stored_check_row: (String, String) = connection
        .query_row(
            "SELECT outcome, latest_revision FROM update_checks WHERE skill_id = ?1 \
             ORDER BY checked_at DESC, id DESC LIMIT 1",
            params!["release-notes"],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("update check exists");
    assert_eq!(stored_check_row.0, "update-available");
    assert_eq!(stored_check_row.1, new_revision);

    let assert = fixture.run(&["tui"]);
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(
        stdout.contains(&format!("installed: git @ {new_revision}")),
        "dashboard was {stdout}"
    );
    assert!(
        stdout.contains(&format!("pin: HEAD -> {new_revision}")),
        "the pin line names the applied revision: {stdout}"
    );
    assert!(
        stdout.contains("update: up-to-date"),
        "the update line reports the reconciled state: {stdout}"
    );
    assert!(
        !stdout.contains("update-available"),
        "the dashboard must not offer the revision the install holds: {stdout}"
    );
    assert!(
        !stdout.contains(&format!(
            "newer than the pinned revision {previous_revision}"
        )),
        "no recorded note may contradict the applied revision: {stdout}"
    );

    let tui_body = fixture.run_json(&["tui"]);
    let card = tui_body["data"]["skills"]
        .as_array()
        .expect("skill cards exist")
        .iter()
        .find(|card| card["skill"] == "release-notes")
        .expect("the dashboard holds a release-notes card");
    assert_eq!(
        card["installed"]["resolved_revision"],
        new_revision.as_str()
    );
    assert_eq!(card["pin"]["resolved_revision"], new_revision.as_str());
    assert_eq!(card["pin"]["effective_version_hash"], pin_row.2);
    assert_eq!(card["update"]["outcome"], "up-to-date");
    assert_eq!(card["update"]["pinned_revision"], new_revision.as_str());
    assert_eq!(card["update"]["latest_revision"], new_revision.as_str());
}

#[test]
fn rollback_refits_the_stored_up_to_date_check_to_the_older_revision() {
    let fixture = UpdateFixture::install();
    let previous_revision = fixture.installed_revision.clone();
    let new_revision = fixture.advance_upstream("Updated upstream release notes helper.");

    fixture
        .run(&["update", "release-notes", "--apply"])
        .success();
    // Record the check that certifies the applied revision as the newest
    // upstream revision.
    let check = fixture.run_json(&["update", "release-notes"]);
    assert_eq!(
        check["data"]["plans"][0]["outcome"], "up-to-date",
        "the check certifies the applied revision: {check:#?}"
    );

    let rollback = fixture.run_json(&["rollback", "release-notes", previous_revision.as_str()]);
    assert_eq!(rollback["ok"], true, "the rollback applies: {rollback:#?}");

    // The stored row keeps the observation it recorded. Only the fitted view
    // changes, because the row is the surviving record of what upstream held.
    let connection =
        Connection::open(fixture.workspace.home_path().join(".skillctl/state.db")).unwrap();
    let stored_check_row: (String, String) = connection
        .query_row(
            "SELECT outcome, latest_revision FROM update_checks WHERE skill_id = ?1 \
             ORDER BY checked_at DESC, id DESC LIMIT 1",
            params!["release-notes"],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("update check exists");
    assert_eq!(stored_check_row.0, "up-to-date");
    assert_eq!(stored_check_row.1, new_revision);

    let assert = fixture.run(&["tui"]);
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(
        stdout.contains(&format!("installed: git @ {previous_revision}")),
        "dashboard was {stdout}"
    );
    assert!(
        stdout.contains("update: update-available"),
        "the update line must not certify a revision the install left: {stdout}"
    );
    assert!(
        stdout.contains(&format!(", latest {new_revision}")),
        "the update line names the recorded upstream revision: {stdout}"
    );
    assert!(
        !stdout.contains("update: up-to-date"),
        "the dashboard must not call the rolled-back install up to date: {stdout}"
    );
    assert!(
        !stdout.contains("pinned revision already matches upstream"),
        "the recorded note must not survive a revision it no longer covers: {stdout}"
    );
    assert!(
        stdout.contains(&format!("the install holds {previous_revision} instead")),
        "the note names the fit: {stdout}"
    );

    let tui_body = fixture.run_json(&["tui"]);
    let card = tui_body["data"]["skills"]
        .as_array()
        .expect("skill cards exist")
        .iter()
        .find(|card| card["skill"] == "release-notes")
        .expect("the dashboard holds a release-notes card");
    assert_eq!(
        card["installed"]["resolved_revision"],
        previous_revision.as_str()
    );
    assert_eq!(card["update"]["outcome"], "update-available");
    assert_eq!(
        card["update"]["pinned_revision"],
        previous_revision.as_str()
    );
    assert_eq!(card["update"]["latest_revision"], new_revision.as_str());

    let list = fixture.run_json(&["list"]);
    let skill = list["data"]["skills"]
        .as_array()
        .expect("skills array exists")
        .iter()
        .find(|skill| skill["skill"] == "release-notes" && skill["scope"] == "workspace")
        .cloned()
        .expect("release-notes is listed in workspace scope");
    assert_eq!(skill["resolved_revision"], previous_revision.as_str());
    assert_eq!(
        skill["latest_update"]["outcome"], "update-available",
        "list reports the same fitted view as the dashboard: {skill:#?}"
    );
}

#[test]
fn list_json_fits_the_stored_update_check_to_the_applied_revision() {
    let fixture = UpdateFixture::install();
    let new_revision = fixture.advance_upstream("Updated upstream release notes helper.");

    let body = fixture.run_json(&["update", "release-notes", "--apply"]);
    assert_eq!(body["ok"], true);
    assert_eq!(
        body["data"]["applied"][0]["resolved_revision"],
        new_revision.as_str()
    );

    // The stored check keeps the observation the apply acted on, so the fitted
    // payload comes from list reading it against the install record.
    let connection =
        Connection::open(fixture.workspace.home_path().join(".skillctl/state.db")).unwrap();
    let stored_check_row: (String, String) = connection
        .query_row(
            "SELECT outcome, latest_revision FROM update_checks WHERE skill_id = ?1 \
             ORDER BY checked_at DESC, id DESC LIMIT 1",
            params!["release-notes"],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("update check exists");
    assert_eq!(stored_check_row.0, "update-available");
    assert_eq!(stored_check_row.1, new_revision);

    let listed_skill = |body: &Value| {
        body["data"]["skills"]
            .as_array()
            .expect("skills array exists")
            .iter()
            .find(|skill| skill["skill"] == "release-notes" && skill["scope"] == "workspace")
            .cloned()
            .expect("release-notes is listed in workspace scope")
    };

    let list = fixture.run_json(&["list"]);
    let skill = listed_skill(&list);
    assert_eq!(skill["resolved_revision"], new_revision.as_str());
    assert_eq!(
        skill["latest_update"]["outcome"], "up-to-date",
        "list must not offer the applied revision as a waiting update: {skill:#?}"
    );
    assert!(
        skill["latest_update"]["checked_at"]
            .as_str()
            .is_some_and(|checked_at| !checked_at.is_empty()),
        "the fitted check keeps its recorded timestamp: {skill:#?}"
    );

    // A check for a revision the install does not hold still reports a waiting
    // update, so the fit never masks a genuinely newer upstream.
    let newer_revision = fixture.advance_upstream("Advanced again after the apply.");
    let recheck = fixture.run_json(&["update", "release-notes"]);
    assert_eq!(recheck["data"]["plans"][0]["outcome"], "update-available");
    assert_eq!(
        recheck["data"]["plans"][0]["latest_revision"],
        newer_revision.as_str()
    );

    let list_after_recheck = fixture.run_json(&["list"]);
    let skill_after_recheck = listed_skill(&list_after_recheck);
    assert_eq!(
        skill_after_recheck["resolved_revision"],
        new_revision.as_str()
    );
    assert_eq!(
        skill_after_recheck["latest_update"]["outcome"], "update-available",
        "a waiting update stays visible in list: {skill_after_recheck:#?}"
    );
}

#[test]
fn a_missing_git_binary_reports_the_external_command_not_a_temp_path() {
    let fixture = UpdateFixture::install();
    let new_revision = fixture.advance_upstream("Advanced for the missing-binary pin.");

    // A pin checks its revision out with git before the guarded transaction
    // opens. When git itself cannot launch, the failure names the git binary
    // the way install does, instead of blaming the staging checkout path the
    // user cannot inspect.
    let output = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(fixture.workspace.path())
        .env("HOME", fixture.workspace.home_path())
        .env("PATH", "")
        .args([
            "--json",
            "--no-input",
            "pin",
            "release-notes",
            new_revision.as_str(),
        ])
        .output()
        .expect("pin runs");
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(
        !output.status.success(),
        "the pin must fail without git on PATH: {text}"
    );
    assert!(
        text.contains("failed to run external command 'git'"),
        "expected the external-command failure, found: {text}"
    );
    assert!(
        !text.contains("clone pinned git source"),
        "the failure must not blame the checkout path: {text}"
    );
}
