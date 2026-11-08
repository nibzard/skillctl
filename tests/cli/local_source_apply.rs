//! Acceptance tests for update apply on local-path and archive imports.
//!
//! A local-path or archive import has no upstream revision to move to, but its
//! generated copies can still hold unrecorded edits. These tests pin the
//! behavior of the repair strategies `sync` and `doctor` recommend for that
//! case.

use super::*;
use assert_cmd::assert::Assert;

/// Projects imports into the shared neutral root `.agents/skills`.
const NEUTRAL_MANIFEST: &str = concat!(
    "version: 1\n",
    "\n",
    "targets:\n",
    "  - codex\n",
    "  - gemini-cli\n",
    "  - opencode\n",
);

/// Source shape of the imported skill under test.
enum ImportSource {
    /// A plain directory next to the workspace.
    LocalPath,
    /// A compressed archive of the same tree.
    Archive,
}

struct LocalSourceFixture {
    workspace: TestWorkspace,
    /// Generated copy the test edits.
    projected_root: PathBuf,
}

impl LocalSourceFixture {
    fn install(source: ImportSource) -> Self {
        let workspace = TestWorkspace::new();
        workspace.write_manifest(NEUTRAL_MANIFEST);
        workspace.write_skill_source("local-source", "alpha");

        let source_path = match source {
            ImportSource::LocalPath => workspace.path().join("local-source"),
            ImportSource::Archive => {
                let archive = workspace.path().join("alpha-source.tar.gz");
                let status = ProcessCommand::new("tar")
                    .args([
                        "-czf",
                        archive.to_str().expect("archive path is utf-8"),
                        ".",
                    ])
                    .current_dir(workspace.path().join("local-source"))
                    .status()
                    .expect("tar launches");
                assert!(status.success(), "fixture archive is created");
                archive
            }
        };

        Command::cargo_bin("skillctl")
            .expect("binary exists")
            .current_dir(workspace.path())
            .env("HOME", workspace.home_path())
            .args([
                "--no-input",
                "--name",
                "alpha",
                "install",
                source_path.to_str().expect("source path is utf-8"),
            ])
            .assert()
            .success()
            .stderr(predicate::str::is_empty());

        let projected_root = workspace.path().join(".agents/skills/alpha");
        assert!(
            projected_root.join("SKILL.md").is_file(),
            "the import materializes into the neutral root"
        );
        Self {
            workspace,
            projected_root,
        }
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
        let path = self.projected_root.join("SKILL.md");
        let mut contents = fs::read_to_string(&path).expect("projected copy exists");
        contents.push_str(&format!("\n{note}\n"));
        fs::write(&path, contents).expect("projected copy edited");
    }

    fn projected_copy(&self) -> String {
        fs::read_to_string(self.projected_root.join("SKILL.md")).expect("projection exists")
    }

    fn overlay_copy(&self) -> String {
        fs::read_to_string(
            self.workspace
                .path()
                .join(".agents/overlays/alpha/SKILL.md"),
        )
        .expect("overlay file exists")
    }
}

#[test]
fn local_source_apply_preserves_edits_as_an_overlay() {
    let fixture = LocalSourceFixture::install(ImportSource::LocalPath);
    fixture.edit_projected_copy("keep my edit");

    let body = fixture.run_json(&["update", "alpha", "--apply", "--strategy", "overlay"]);

    assert_eq!(body["ok"], true, "overlay repair succeeds: {body}");
    assert!(
        fixture.overlay_copy().contains("keep my edit"),
        "the edit survives in the managed overlay"
    );
    assert_eq!(
        body["data"]["applied"][0]["skill"], "alpha",
        "the skill is reported as applied: {body}"
    );

    let lockfile = fixture.workspace.read_lockfile_yaml();
    let entry = &lockfile["imports"]["alpha"];
    assert_ne!(
        entry["hashes"]["overlay"].as_str().unwrap_or(""),
        "sha256:none",
        "the overlay is part of the effective version: {lockfile:?}"
    );

    // The generated copy is rebuilt from the source plus the overlay, so the
    // drift that blocked a plain sync is gone.
    assert!(
        fixture.projected_copy().contains("keep my edit"),
        "the projection keeps the preserved edit"
    );
    let sync = fixture.run(&["sync"]);
    sync.success();

    let connection =
        Connection::open(fixture.workspace.home_path().join(".skillctl/state.db")).unwrap();
    let kinds: Vec<String> = connection
        .prepare("SELECT kind FROM history_events WHERE skill_id = ?1 ORDER BY id ASC")
        .unwrap()
        .query_map(params!["alpha"], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert!(
        kinds.contains(&"overlay-created".to_string())
            && kinds.contains(&"update-applied".to_string()),
        "the repair is auditable in history: {kinds:?}"
    );
}

#[test]
fn archive_source_apply_preserves_edits_as_an_overlay() {
    let fixture = LocalSourceFixture::install(ImportSource::Archive);
    fixture.edit_projected_copy("keep my archive edit");

    let body = fixture.run_json(&["update", "alpha", "--apply", "--strategy", "overlay"]);

    assert_eq!(body["ok"], true, "overlay repair succeeds: {body}");
    assert!(fixture.overlay_copy().contains("keep my archive edit"));
    assert!(
        fixture.projected_copy().contains("keep my archive edit"),
        "the projection keeps the preserved edit"
    );
}

#[test]
fn local_source_apply_discards_edits_after_a_durable_backup() {
    let fixture = LocalSourceFixture::install(ImportSource::LocalPath);
    fixture.edit_projected_copy("temporary edit");

    let body = fixture.run_json(&["update", "alpha", "--apply", "--strategy", "discard"]);

    assert_eq!(body["ok"], true, "discard repair succeeds: {body}");
    let discarded = &body["data"]["discarded"][0];
    let backup = discarded["backup"].as_str().expect("backup path exists");
    assert!(
        fs::read_to_string(PathBuf::from(backup).join("SKILL.md"))
            .expect("backed-up content exists")
            .contains("temporary edit"),
        "the discarded edit is recoverable from {backup}"
    );
    let projected = fixture.projected_copy();
    assert!(
        !projected.contains("temporary edit"),
        "the projection is refreshed: {projected}"
    );
    assert!(
        projected.contains("Summarize release notes."),
        "the projection holds the pinned source content: {projected}"
    );

    let connection =
        Connection::open(fixture.workspace.home_path().join(".skillctl/state.db")).unwrap();
    let summaries: Vec<String> = connection
        .prepare(
            "SELECT summary FROM history_events WHERE skill_id = ?1 AND kind = 'cleanup' \
             ORDER BY id ASC",
        )
        .unwrap()
        .query_map(params!["alpha"], |row| row.get(0))
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
fn local_source_apply_can_fork_into_local_ownership_and_keep_the_edits() {
    let fixture = LocalSourceFixture::install(ImportSource::LocalPath);
    fixture.edit_projected_copy("keep my forked edit");

    let body = fixture.run_json(&["update", "alpha", "--apply", "--strategy", "fork"]);

    assert_eq!(body["ok"], true, "fork repair succeeds: {body}");
    assert_eq!(body["data"]["applied"][0]["strategy"], "fork");

    let lockfile = fixture.workspace.read_lockfile_yaml();
    assert!(
        lockfile["imports"]["alpha"].is_null(),
        "forking releases the managed import: {lockfile:?}"
    );
    assert!(
        fixture.projected_copy().contains("keep my forked edit"),
        "forking keeps the unrecorded edits: {}",
        fixture.projected_copy()
    );
}

#[test]
fn a_local_source_clean_apply_names_the_strategies_that_work() {
    let fixture = LocalSourceFixture::install(ImportSource::LocalPath);
    fixture.edit_projected_copy("keep my edit");

    let body = fixture.run_json(&["update", "alpha", "--apply"]);

    assert_eq!(body["ok"], false, "a clean apply refuses to guess: {body}");
    let error = body["errors"][0].as_str().expect("error exists");
    assert!(
        error.contains("--strategy overlay") && error.contains("--strategy discard"),
        "the error names both repair strategies: {error}"
    );
    assert!(
        !fixture
            .workspace
            .path()
            .join(".agents/overlays/alpha")
            .exists(),
        "a refused apply creates no overlay"
    );
}

#[test]
fn a_local_source_plan_document_applies_the_repair_it_describes() {
    let fixture = LocalSourceFixture::install(ImportSource::LocalPath);
    fixture.edit_projected_copy("keep my edit");
    let plan_path = fixture.workspace.path().join("update-plan.json");

    let body = fixture.run_json(&[
        "update",
        "alpha",
        "--strategy",
        "overlay",
        "--plan",
        "update-plan.json",
    ]);
    assert_eq!(body["ok"], true, "planning changes nothing: {body}");
    let document: Value =
        serde_json::from_str(&fs::read_to_string(&plan_path).expect("plan document exists"))
            .expect("plan document is valid json");
    assert!(
        document["plans"][0]["proposed"][0]["summary"]
            .as_str()
            .expect("summary exists")
            .contains("resolve unrecorded edits"),
        "a local-source plan describes a repair: {document}"
    );
    assert!(
        !fixture
            .workspace
            .path()
            .join(".agents/overlays/alpha")
            .exists(),
        "planning alone creates no overlay"
    );

    let body = fixture.run_json(&["update", "--apply", "--plan-file", "update-plan.json"]);

    assert_eq!(body["ok"], true, "the plan applies: {body}");
    assert_eq!(body["data"]["applied"][0]["skill"], "alpha", "{body}");
    assert!(
        fixture.overlay_copy().contains("keep my edit"),
        "the planned repair keeps the edits"
    );
}

#[test]
fn a_requested_apply_that_changes_nothing_warns_instead_of_staying_silent() {
    let fixture = LocalSourceFixture::install(ImportSource::LocalPath);

    let body = fixture.run_json(&["update", "alpha", "--apply", "--strategy", "overlay"]);

    assert_eq!(body["ok"], true, "the check itself succeeds: {body}");
    assert_eq!(body["data"]["applied"], json!([]));
    let reason = body["data"]["apply_skipped"][0]["reason"]
        .as_str()
        .expect("reason exists");
    assert!(
        reason.contains("local"),
        "the skip reason explains the source kind: {reason}"
    );
    assert!(
        !body["warnings"]
            .as_array()
            .expect("warnings exist")
            .is_empty(),
        "a no-op apply is visible in the warnings: {body}"
    );

    let assert = fixture.run(&["update", "alpha", "--apply", "--strategy", "overlay"]);
    assert
        .code(2)
        .stdout(predicate::str::contains("warning:"))
        .stdout(predicate::str::contains("alpha"));
}

#[test]
fn local_source_update_reports_the_edits_it_detected() {
    let fixture = LocalSourceFixture::install(ImportSource::LocalPath);
    fixture.edit_projected_copy("keep my edit");

    let body = fixture.run_json(&["update", "alpha"]);

    let plan = &body["data"]["plans"][0];
    assert_eq!(plan["outcome"], "local-source");
    assert_eq!(plan["local_modification_detected"], true);
    assert_eq!(plan["recommended_action"], "create-overlay");
    assert_eq!(plan["modifications"][0]["kind"], "projected-copy");
    assert_eq!(plan["modifications"][0]["managed"], false);
}

#[test]
fn doctor_names_a_working_repair_for_an_edited_generated_copy() {
    let fixture = LocalSourceFixture::install(ImportSource::LocalPath);
    fixture.edit_projected_copy("keep my edit");

    let body = fixture.run_json(&["doctor"]);

    let issues = body["data"]["issues"]
        .as_array()
        .expect("issues exist")
        .iter()
        .filter(|issue| issue["code"] == "projection-drift")
        .collect::<Vec<_>>();
    assert!(!issues.is_empty(), "the drift is reported: {body}");
    for issue in issues {
        let fix = issue["fix"].as_str().expect("fix exists");
        assert!(
            fix.contains("update alpha --apply --strategy overlay")
                && fix.contains("update alpha --apply --strategy discard"),
            "the fix names commands that work for a local source: {fix}"
        );
        assert!(
            !fix.contains("skillctl sync"),
            "a plain sync preserves the copy, so it is not a fix: {fix}"
        );
    }
}

#[test]
fn doctor_still_recommends_sync_for_a_clean_but_stale_copy() {
    let fixture = LocalSourceFixture::install(ImportSource::LocalPath);
    fixture.run(&["override", "alpha"]).success();
    let overlay = fixture
        .workspace
        .path()
        .join(".agents/overlays/alpha/SKILL.md");
    let mut contents = fs::read_to_string(&overlay).expect("overlay manifest exists");
    contents.push_str("\noverlaid note\n");
    fs::write(&overlay, contents).expect("overlay edited");

    let body = fixture.run_json(&["doctor"]);

    let issues = body["data"]["issues"]
        .as_array()
        .expect("issues exist")
        .iter()
        .filter(|issue| issue["code"] == "projection-drift")
        .collect::<Vec<_>>();
    assert!(!issues.is_empty(), "the stale copy is reported: {body}");
    for issue in issues {
        let fix = issue["fix"].as_str().expect("fix exists");
        assert!(
            fix.contains("skillctl sync"),
            "an unedited stale copy is repaired by sync: {fix}"
        );
    }
}

#[test]
fn explain_names_a_working_repair_for_an_edited_generated_copy() {
    let fixture = LocalSourceFixture::install(ImportSource::LocalPath);
    fixture.edit_projected_copy("keep my edit");

    let body = fixture.run_json(&["explain", "alpha"]);

    let drifted = body["data"]["targets"]
        .as_array()
        .expect("targets exist")
        .iter()
        .filter(|target| target["observed_matches_winner"] == json!(false))
        .collect::<Vec<_>>();
    assert!(!drifted.is_empty(), "the drift is visible: {body}");
    for target in drifted {
        let fix = target["fix"].as_str().expect("fix exists");
        assert!(
            fix.contains("update alpha --apply --strategy overlay")
                && fix.contains("update alpha --apply --strategy discard"),
            "the fix names commands that work for a local source: {fix}"
        );
        assert!(
            !fix.contains("skillctl sync to refresh"),
            "a plain sync preserves the copy, so it is not a fix: {fix}"
        );
    }
}
