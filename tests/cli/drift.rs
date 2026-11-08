use super::*;

const CLAUDE_MANIFEST: &str = concat!("version: 1\n", "\n", "targets:\n", "  - claude-code\n",);

/// Drop the recorded content digest of one generated copy, as in a copy
/// written before digests existed.
fn remove_recorded_digest(projection_dir: &Path) {
    let metadata_path = projection_dir.join(".skillctl-projection.json");
    let mut metadata: Value = serde_json::from_str(
        &fs::read_to_string(&metadata_path).expect("projection metadata exists"),
    )
    .expect("projection metadata is valid json");
    metadata
        .as_object_mut()
        .expect("projection metadata is an object")
        .remove("content_digest");
    fs::write(
        &metadata_path,
        serde_json::to_string_pretty(&metadata).expect("projection metadata serializes"),
    )
    .expect("projection metadata is written");
}

struct DriftFixture {
    workspace: TestWorkspace,
}

impl DriftFixture {
    fn install() -> Self {
        let workspace = TestWorkspace::new();
        workspace.write_manifest(CLAUDE_MANIFEST);
        workspace.write_skill_source("git-source", "release-notes");
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

        Self { workspace }
    }

    /// Install an unrelated second skill. The install may exit with 2 when a
    /// preserved edit in a shared root produces a warning.
    /// Bootstrap only, without an imported skill.
    fn install_only() -> Self {
        let workspace = TestWorkspace::new();
        Self { workspace }
    }

    fn install_second_skill(&self) {
        self.workspace
            .write_skill_source("git-source-2", "bug-triage");
        self.workspace.init_git_repo("git-source-2");
        let output = Command::cargo_bin("skillctl")
            .expect("binary exists")
            .current_dir(self.workspace.path())
            .env("HOME", self.workspace.home_path())
            .args([
                "--no-input",
                "--name",
                "bug-triage",
                "install",
                self.workspace.git_repo_url("git-source-2").as_str(),
            ])
            .output()
            .expect("install runs");
        assert!(
            output.stderr.is_empty(),
            "unexpected stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("Installed 1 skill"),
            "the unrelated install completed: {stdout}"
        );
    }

    fn edit_generated_copy(&self, note: &str) {
        self.edit_generated_skill("release-notes", note);
    }

    /// Pin the managed overlay, as the first `skillctl override` run does.
    fn pin_overlay(&self) {
        self.run_json(&["--json", "override", "release-notes"]);
    }

    /// Edit the pinned overlay without re-pinning it, as the documented
    /// intermediate state between the two `skillctl override` runs.
    fn edit_overlay(&self, note: &str) {
        let path = self
            .workspace
            .path()
            .join(".agents/overlays/release-notes/SKILL.md");
        let mut contents = fs::read_to_string(&path).expect("overlay manifest exists");
        contents.push('\n');
        contents.push_str(note);
        contents.push('\n');
        fs::write(&path, contents).expect("overlay edited");
    }

    /// Drop the recorded digest of the generated copy, as in a copy written
    /// before digests existed.
    fn drop_recorded_digest(&self) {
        remove_recorded_digest(&self.workspace.path().join(".claude/skills/release-notes"));
    }

    /// Edit the generated copy of one skill directly.
    fn edit_generated_skill(&self, skill: &str, note: &str) {
        let path = self
            .workspace
            .path()
            .join(format!(".claude/skills/{skill}/SKILL.md"));
        let mut contents = fs::read_to_string(&path).expect("generated copy exists");
        contents.push_str(note);
        contents.push('\n');
        fs::write(&path, contents).expect("generated copy edited");
    }

    /// Run one command and return its exit code with its whole JSON body as text.
    fn run_json_text(&self, args: &[&str]) -> (i32, String) {
        let (code, body) = self.run_json(args);
        let text = serde_json::to_string(&body).expect("json body serializes");
        (code, text)
    }

    /// Assert one report names a repair that works for an unmanaged copy.
    ///
    /// `surface` names the command in assertion messages. No install record
    /// holds the skill, so every `update --apply` strategy exits with 3.
    fn assert_names_working_repair(
        &self,
        surface: &str,
        args: &[&str],
        skill: &str,
        expected_code: i32,
    ) {
        let (code, text) = self.run_json_text(args);
        assert_eq!(
            code, expected_code,
            "{surface} reports the preserved copy: {text}"
        );
        assert!(
            text.contains("skillctl sync --discard"),
            "{surface} must name the discard that works: {text}",
        );
        assert!(
            !text.contains(&format!("update {skill} --apply")),
            "{surface} must not name an update that exits 3: {text}",
        );
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

    fn generated_copy(&self) -> String {
        fs::read_to_string(
            self.workspace
                .path()
                .join(".claude/skills/release-notes/SKILL.md"),
        )
        .expect("generated copy exists")
    }

    /// Human output of one command, printed to stdout.
    fn run_human(&self, args: &[&str]) -> (i32, String) {
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
        (
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stdout).into_owned(),
        )
    }

    /// Move the installed Git source one commit forward.
    fn advance_upstream(&self, description: &str) {
        self.workspace.write_skill_source_at(
            "git-source",
            ".agents/skills/release-notes",
            "release-notes",
            description,
        );
        self.workspace.commit_all("git-source", "advance upstream");
    }
}

#[test]
fn generated_copies_record_a_content_digest() {
    let fixture = DriftFixture::install();

    let metadata: Value = serde_json::from_str(
        &fs::read_to_string(
            fixture
                .workspace
                .path()
                .join(".claude/skills/release-notes/.skillctl-projection.json"),
        )
        .expect("projection metadata exists"),
    )
    .expect("projection metadata is valid json");
    let digest = metadata["content_digest"].as_str().expect("digest exists");
    assert!(
        digest.starts_with("sha256:"),
        "digest is canonical: {digest}"
    );

    // A clean sync does not report anything preserved.
    let (code, body) = fixture.run_json(&["--json", "sync"]);
    assert_eq!(code, 0, "clean sync succeeds: {body}");
    let preserved = body["data"]["preserved"].as_array();
    assert!(
        preserved.is_none_or(Vec::is_empty),
        "nothing should be preserved: {preserved:?}"
    );
}

#[test]
fn sync_preserves_a_directly_edited_generated_copy() {
    let fixture = DriftFixture::install();
    fixture.edit_generated_copy("keep my edit");

    let (code, body) = fixture.run_json(&["--json", "sync"]);

    assert_eq!(code, 2, "a preserved copy is a warning, not a failure");
    let preserved = &body["data"]["preserved"][0];
    assert_eq!(preserved["skill"], "release-notes");
    assert_eq!(preserved["reason"], "dirty-projected-copy");
    assert!(
        preserved["path"]
            .as_str()
            .expect("path exists")
            .contains(".claude/skills/release-notes"),
        "unexpected path: {preserved}"
    );
    assert!(
        body["warnings"]
            .as_array()
            .expect("warnings exist")
            .iter()
            .any(|warning| warning.as_str().unwrap().contains("preserved")),
        "the warning names the preserved copy"
    );
    assert!(
        fixture.generated_copy().contains("keep my edit"),
        "the edit survives the sync"
    );
}

#[test]
fn explain_names_a_working_repair_for_an_edited_generated_copy() {
    let fixture = DriftFixture::install();
    fixture.edit_generated_copy("keep my edit");

    let (code, body) = fixture.run_json(&["--json", "explain", "release-notes"]);

    assert_eq!(code, 0, "explain reports the drift without failing: {body}");
    let drifted: Vec<&Value> = body["data"]["targets"]
        .as_array()
        .expect("targets exist")
        .iter()
        .filter(|target| target["observed_matches_winner"] == json!(false))
        .collect();
    assert_eq!(drifted.len(), 1, "the edited copy is reported: {body}");
    for target in &drifted {
        assert_eq!(
            target["observed_preserved_reason"], "dirty-projected-copy",
            "the reason matches the identifier sync reports: {target}",
        );
        let fix = target["fix"].as_str().expect("fix exists");
        assert!(
            fix.contains("update release-notes --apply --strategy overlay")
                && fix.contains("update release-notes --apply --strategy discard"),
            "the fix names the strategies doctor names: {fix}",
        );
    }

    let (code, summary) = fixture.run_human(&["explain", "release-notes"]);
    assert_eq!(code, 0, "the human summary reports the drift: {summary}");
    assert!(
        summary.contains("0 of 1 target projection verified"),
        "the drift is visible in the summary: {summary}",
    );
    assert!(
        summary.contains("preserves the copy instead of refreshing it"),
        "the summary says why a plain sync is not the repair: {summary}",
    );
    assert!(
        summary.contains("update release-notes --apply --strategy overlay")
            && summary.contains("update release-notes --apply --strategy discard")
            && summary.contains("skillctl sync --discard' also refreshes the copy"),
        "the summary names commands that resolve the drift: {summary}",
    );
    assert!(
        !summary.contains("Run 'skillctl sync' to materialize or refresh"),
        "a plain sync preserves the copy, so the summary must not recommend it: {summary}",
    );

    // The recommended update strategy resolves the drift once an update can
    // be applied, so the advice is not only truthful but actionable.
    fixture.advance_upstream("Updated upstream release notes helper.");
    let (code, body) = fixture.run_json(&[
        "--json",
        "update",
        "release-notes",
        "--apply",
        "--strategy",
        "overlay",
    ]);
    assert_eq!(code, 0, "the advertised overlay apply succeeds: {body}");

    let (code, summary) = fixture.run_human(&["explain", "release-notes"]);
    assert_eq!(code, 0, "explain still succeeds: {summary}");
    assert!(
        summary.contains("1 of 1 target projection verified"),
        "the advertised repair resolved the drift: {summary}",
    );
    assert!(
        fixture.generated_copy().contains("keep my edit"),
        "the overlay strategy keeps the edit"
    );
}

// A requested repair apply must resolve the drift while the import is
// up-to-date, because that is the state every drift surface reports the
// strategies for.
#[test]
fn update_apply_repairs_an_edited_generated_copy_while_up_to_date() {
    let fixture = DriftFixture::install();
    fixture.edit_generated_copy("keep my edit");

    let (code, body) = fixture.run_json(&[
        "--json",
        "update",
        "release-notes",
        "--apply",
        "--strategy",
        "overlay",
    ]);

    assert_eq!(code, 0, "the advertised overlay apply succeeds: {body}");
    assert_eq!(
        body["data"]["applied"][0]["skill"], "release-notes",
        "the repair is reported as applied: {body}"
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
    assert_eq!(
        lockfile["imports"]["release-notes"]["revision"]["resolved"]
            .as_str()
            .unwrap_or("<missing>"),
        fixture.workspace.git_head("git-source"),
        "the repair keeps the pinned revision: {lockfile:?}"
    );

    assert!(
        fixture.generated_copy().contains("keep my edit"),
        "the overlay strategy keeps the edit in the projection"
    );
    let (code, summary) = fixture.run_human(&["explain", "release-notes"]);
    assert_eq!(code, 0, "explain succeeds: {summary}");
    assert!(
        summary.contains("1 of 1 target projection verified"),
        "the advertised repair resolved the drift: {summary}",
    );
}

#[test]
fn update_apply_discards_an_edited_generated_copy_while_up_to_date() {
    let fixture = DriftFixture::install();
    fixture.edit_generated_copy("temporary edit");

    let (code, body) = fixture.run_json(&[
        "--json",
        "update",
        "release-notes",
        "--apply",
        "--strategy",
        "discard",
    ]);

    assert_eq!(code, 0, "the advertised discard apply succeeds: {body}");
    let discarded = &body["data"]["discarded"][0];
    let backup = discarded["backup"].as_str().expect("backup path exists");
    assert!(
        fs::read_to_string(PathBuf::from(backup).join("SKILL.md"))
            .expect("backed-up content exists")
            .contains("temporary edit"),
        "the discarded edit is recoverable from {backup}"
    );

    let copy = fixture.generated_copy();
    assert!(
        !copy.contains("temporary edit"),
        "the copy is refreshed: {copy}"
    );
    assert!(
        copy.contains("Summarize release notes."),
        "the copy holds the pinned source content again: {copy}"
    );

    let (code, summary) = fixture.run_human(&["explain", "release-notes"]);
    assert_eq!(code, 0, "explain succeeds: {summary}");
    assert!(
        summary.contains("1 of 1 target projection verified"),
        "the advertised repair resolved the drift: {summary}",
    );
}

/// The overlay moved past its recorded pin, so the copy audit used to be
/// skipped entirely and every drift report named update strategies that
/// changed nothing. The audit reads the content skillctl wrote for each copy
/// instead, so the advertised repair resolves the copy in one command.
#[test]
fn an_advertised_repair_resolves_an_edit_while_the_overlay_changed_since_the_pin() {
    let fixture = DriftFixture::install();
    fixture.pin_overlay();
    fixture.edit_overlay("overlay note");
    fixture.edit_generated_copy("keep my edit");

    let (code, body) = fixture.run_json(&["--json", "update", "release-notes"]);
    assert_eq!(code, 2, "the overlay drift warns: {body}");
    let plan = &body["data"]["plans"][0];
    assert_eq!(plan["outcome"], "up-to-date", "no revision waits: {plan}");
    assert_eq!(
        plan["overlay_changed_since_recorded_state"], true,
        "the check records the overlay drift: {plan}"
    );
    assert_eq!(
        plan["local_modification_detected"], true,
        "the check still classifies the edited copy: {plan}"
    );
    assert_eq!(
        plan["modifications"][1]["kind"], "projected-copy",
        "the copy edit is reported beside the managed overlay: {plan}"
    );

    // The human output surfaces the state the check records in its notes.
    let (code, summary) = fixture.run_human(&["update", "release-notes"]);
    assert_eq!(code, 2, "the overlay drift warns: {summary}");
    assert!(
        summary.contains("managed overlay that changed since the recorded state"),
        "the human output names the overlay drift: {summary}"
    );
    assert!(
        summary.contains("'skillctl override release-notes'"),
        "the human output names the re-pin: {summary}"
    );

    // Doctor advises the strategies, so both must act in this state.
    let (code, text) = fixture.run_json_text(&["--json", "doctor"]);
    assert_eq!(code, 2, "doctor reports the drifted copy: {text}");
    assert!(
        text.contains("update release-notes --apply --strategy overlay")
            && text.contains("update release-notes --apply --strategy discard"),
        "doctor names both strategies: {text}"
    );

    let (code, body) = fixture.run_json(&[
        "--json",
        "update",
        "release-notes",
        "--apply",
        "--strategy",
        "overlay",
    ]);
    assert_eq!(
        code, 0,
        "the advertised overlay apply resolves the copy: {body}"
    );

    let overlay = fs::read_to_string(
        fixture
            .workspace
            .path()
            .join(".agents/overlays/release-notes/SKILL.md"),
    )
    .expect("overlay file exists");
    assert!(
        overlay.contains("keep my edit"),
        "the edit survives in the managed overlay: {overlay}"
    );
    let copy = fixture.generated_copy();
    assert!(
        copy.contains("keep my edit"),
        "the overlay strategy keeps the edit in the projection: {copy}"
    );

    let lockfile = fixture.workspace.read_lockfile_yaml();
    let pinned_overlay = lockfile["imports"]["release-notes"]["hashes"]["overlay"]
        .as_str()
        .unwrap_or("<missing>");
    assert_ne!(
        pinned_overlay, "sha256:none",
        "the repair re-pins the overlay it changed: {lockfile:?}"
    );

    let (code, summary) = fixture.run_human(&["explain", "release-notes"]);
    assert_eq!(code, 0, "explain succeeds: {summary}");
    assert!(
        summary.contains("1 of 1 target projection verified"),
        "the advertised repair resolved the drift: {summary}",
    );

    // The repair re-pinned the overlay, so the same state checks clean.
    let (code, body) = fixture.run_json(&["--json", "update", "release-notes"]);
    assert_eq!(code, 0, "the resolved state checks clean: {body}");
    let plan = &body["data"]["plans"][0];
    assert_eq!(
        plan["local_modification_detected"], false,
        "no surface reports the copy any more: {plan}"
    );
    assert!(
        plan.get("overlay_changed_since_recorded_state").is_none(),
        "the overlay is pinned again: {plan}"
    );
}

/// A reinstall keeps the overlay directory a remove retained, but it records
/// no pin for it, so the overlay counts as changed since the recorded state.
/// The copy edit left beside it must still reach the advertised repair, and a
/// copy nobody edited must not be reported as one.
#[test]
fn an_advertised_repair_resolves_an_edit_left_beside_a_retained_overlay() {
    let fixture = DriftFixture::install();
    fixture.pin_overlay();
    fixture.edit_overlay("overlay note");
    fixture.run_json(&["--json", "remove", "release-notes"]);
    fixture.run_json(&[
        "--no-input",
        "--json",
        "--name",
        "release-notes",
        "install",
        &fixture.workspace.git_repo_url("git-source"),
    ]);

    // The retained overlay is unpinned again, but the freshly materialized
    // copy holds no edit, so the check must not report one.
    let (code, body) = fixture.run_json(&["--json", "update", "release-notes"]);
    assert_eq!(code, 2, "the unpinned overlay still warns: {body}");
    let plan = &body["data"]["plans"][0];
    assert_eq!(
        plan["overlay_changed_since_recorded_state"], true,
        "the retained overlay counts as changed: {plan}"
    );
    assert_eq!(
        plan["local_modification_detected"], false,
        "a copy nobody edited is not reported as an edit: {plan}"
    );

    fixture.edit_generated_copy("keep my edit");
    let (code, body) = fixture.run_json(&[
        "--json",
        "update",
        "release-notes",
        "--apply",
        "--strategy",
        "overlay",
    ]);
    assert_eq!(
        code, 0,
        "the advertised overlay apply resolves the copy: {body}"
    );
    assert!(
        fixture.generated_copy().contains("keep my edit"),
        "the edit survives in the projection"
    );

    let (code, summary) = fixture.run_human(&["explain", "release-notes"]);
    assert_eq!(code, 0, "explain succeeds: {summary}");
    assert!(
        summary.contains("1 of 1 target projection verified"),
        "the advertised repair resolved the drift: {summary}",
    );
}

/// The discard strategy is the other half of doctor's advice, so it must act
/// in the same state: it drops the copy edit and projects the overlay change.
#[test]
fn an_advertised_discard_resolves_an_edit_while_the_overlay_changed_since_the_pin() {
    let fixture = DriftFixture::install();
    fixture.pin_overlay();
    fixture.edit_overlay("overlay note");
    fixture.edit_generated_copy("temporary edit");

    let (code, body) = fixture.run_json(&[
        "--json",
        "update",
        "release-notes",
        "--apply",
        "--strategy",
        "discard",
    ]);

    assert_eq!(
        code, 0,
        "the advertised discard apply resolves the copy: {body}"
    );
    let backup = body["data"]["discarded"][0]["backup"]
        .as_str()
        .expect("backup path exists");
    assert!(
        fs::read_to_string(PathBuf::from(backup).join("SKILL.md"))
            .expect("backed-up content exists")
            .contains("temporary edit"),
        "the discarded edit is recoverable from {backup}"
    );

    let copy = fixture.generated_copy();
    assert!(
        !copy.contains("temporary edit"),
        "the copy is refreshed: {copy}"
    );
    assert!(
        copy.contains("overlay note"),
        "the copy projects the overlay change the check judged around: {copy}"
    );

    let (code, body) = fixture.run_json(&["--json", "update", "release-notes"]);
    assert_eq!(code, 0, "the resolved state checks clean: {body}");
    assert_eq!(
        body["data"]["plans"][0]["local_modification_detected"], false,
        "no surface reports the copy any more: {body}"
    );
}

/// A clean apply refuses to guess, and the refusal names the unmanaged copy
/// edit that blocks it, not the managed overlay beside it.
#[test]
fn a_clean_apply_names_the_copy_edit_while_the_overlay_changed_since_the_pin() {
    let fixture = DriftFixture::install();
    fixture.pin_overlay();
    fixture.edit_overlay("overlay note");
    fixture.edit_generated_copy("keep my edit");

    let (code, body) = fixture.run_json(&["--json", "update", "release-notes", "--apply"]);

    assert_eq!(code, 3, "a clean apply refuses to guess: {body}");
    let error = body["errors"][0].as_str().expect("error exists");
    assert!(
        error.contains("--strategy overlay") && error.contains("--strategy discard"),
        "the error names both repair strategies: {error}"
    );
    assert!(
        error.contains(".claude/skills/release-notes"),
        "the error names the unmanaged edit that blocks it: {error}"
    );
    assert!(
        !error.contains(".agents/overlays/release-notes"),
        "the managed overlay is not the edit that blocks the apply: {error}"
    );
    assert!(
        fixture.generated_copy().contains("keep my edit"),
        "the edit survives the refusal"
    );
}

#[test]
fn an_up_to_date_clean_apply_names_the_strategies_that_work() {
    let fixture = DriftFixture::install();
    fixture.edit_generated_copy("keep my edit");

    let (code, body) = fixture.run_json(&["--json", "update", "release-notes", "--apply"]);

    assert_eq!(
        code, 3,
        "the refusal is a validation failure, not a warning: {body}"
    );
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
            .join(".agents/overlays/release-notes")
            .exists(),
        "a refused apply creates no overlay"
    );
    assert!(
        fixture.generated_copy().contains("keep my edit"),
        "the edit survives the refusal"
    );
}

#[test]
fn an_up_to_date_plan_document_applies_the_repair_it_describes() {
    let fixture = DriftFixture::install();
    fixture.edit_generated_copy("keep my edit");
    let plan_path = fixture.workspace.path().join("update-plan.json");

    let (code, body) = fixture.run_json(&[
        "--json",
        "update",
        "release-notes",
        "--plan",
        "update-plan.json",
        "--strategy",
        "overlay",
    ]);
    assert_eq!(code, 0, "planning changes nothing: {body}");
    let document: Value =
        serde_json::from_str(&fs::read_to_string(&plan_path).expect("plan document exists"))
            .expect("plan document is valid json");
    assert_eq!(
        document["plans"][0]["proposed"][0]["revision"],
        Value::Null,
        "an up-to-date repair plan proposes no revision move: {document}"
    );
    assert!(
        document["plans"][0]["proposed"][0]["summary"]
            .as_str()
            .expect("summary exists")
            .contains("resolve unrecorded edits"),
        "the plan describes a repair: {document}"
    );
    assert!(
        !fixture
            .workspace
            .path()
            .join(".agents/overlays/release-notes")
            .exists(),
        "planning alone creates no overlay"
    );

    let (code, body) = fixture.run_json(&[
        "--json",
        "update",
        "--apply",
        "--plan-file",
        "update-plan.json",
    ]);

    assert_eq!(
        code, 2,
        "a preserved edit is reported as a warning, not a failure: {body}"
    );
    assert_eq!(body["ok"], true, "the planned repair applies: {body}");
    assert_eq!(
        body["data"]["applied"][0]["skill"], "release-notes",
        "the document applied its repair: {body}"
    );
    assert!(
        fixture.generated_copy().contains("keep my edit"),
        "the planned repair keeps the edits"
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
}

#[test]
fn an_up_to_date_apply_without_edits_still_warns_about_the_no_op() {
    let fixture = DriftFixture::install();

    let (code, body) = fixture.run_json(&[
        "--json",
        "update",
        "release-notes",
        "--apply",
        "--strategy",
        "overlay",
    ]);

    assert_eq!(
        code, 2,
        "a requested apply that changes nothing exits with a warning: {body}"
    );
    assert_eq!(
        body["data"]["applied"],
        json!([]),
        "nothing was applied: {body}"
    );
    let reason = body["data"]["apply_skipped"][0]["reason"]
        .as_str()
        .expect("reason exists");
    assert!(
        reason.contains("up-to-date"),
        "the skip reason names the check outcome: {reason}"
    );
    assert!(
        !body["warnings"]
            .as_array()
            .expect("warnings exist")
            .is_empty(),
        "a no-op apply is visible in the warnings: {body}"
    );
}

#[test]
fn explain_still_recommends_sync_for_a_clean_but_stale_copy() {
    let fixture = DriftFixture::install();
    fixture.run_json(&["--json", "override", "release-notes"]);
    let overlay = fixture
        .workspace
        .path()
        .join(".agents/overlays/release-notes/SKILL.md");
    let mut contents = fs::read_to_string(&overlay).expect("overlay manifest exists");
    contents.push_str("\noverlaid note\n");
    fs::write(&overlay, contents).expect("overlay edited");

    let (code, body) = fixture.run_json(&["--json", "explain", "release-notes"]);
    assert_eq!(code, 0, "explain reports the stale copy: {body}");
    let drifted: Vec<&Value> = body["data"]["targets"]
        .as_array()
        .expect("targets exist")
        .iter()
        .filter(|target| target["observed_matches_winner"] == json!(false))
        .collect();
    assert_eq!(drifted.len(), 1, "the stale copy is reported: {body}");
    assert!(
        drifted[0].get("observed_preserved_reason").is_none(),
        "a clean copy is refreshed by sync, so it carries no preserved reason: {body}",
    );

    let (code, summary) = fixture.run_human(&["explain", "release-notes"]);
    assert_eq!(code, 0, "the human summary reports the drift: {summary}");
    assert!(
        summary.contains("Run 'skillctl sync' to materialize or refresh"),
        "an unedited stale copy is repaired by sync: {summary}",
    );
    assert!(
        !summary.contains("preserves the copy instead of refreshing it"),
        "the preserved-copy advice must not appear for a clean copy: {summary}",
    );
}

#[test]
fn an_unrelated_install_preserves_a_directly_edited_generated_copy() {
    let fixture = DriftFixture::install();
    fixture.edit_generated_copy("keep my edit");

    fixture.install_second_skill();

    assert!(
        fixture.generated_copy().contains("keep my edit"),
        "an unrelated install must not discard a direct edit"
    );
    let (code, body) = fixture.run_json(&["--json", "explain", "bug-triage"]);
    assert_eq!(code, 0, "the unrelated install still completed: {body}");
}

#[test]
fn sync_discard_backs_up_and_regenerates_an_edited_copy() {
    let fixture = DriftFixture::install();
    fixture.edit_generated_copy("temporary edit");

    let (code, body) = fixture.run_json(&["--json", "sync", "--discard"]);

    assert_eq!(code, 0, "an explicit discard succeeds: {body}");
    let discarded = &body["data"]["discarded"][0];
    assert_eq!(discarded["skill"], "release-notes");
    let backup = discarded["backup"].as_str().expect("backup exists");
    assert!(
        backup.contains(".skillctl/store/discards"),
        "the backup is durable: {backup}"
    );
    assert!(
        fs::read_to_string(PathBuf::from(backup).join("SKILL.md"))
            .expect("backed-up content exists")
            .contains("temporary edit"),
        "the discarded content is recoverable"
    );
    assert!(
        !fixture.generated_copy().contains("temporary edit"),
        "the copy is regenerated without the edit"
    );
}

#[test]
fn clean_preserves_generated_copies_with_unrecorded_edits() {
    let fixture = DriftFixture::install();
    fixture.edit_generated_copy("keep my edit");

    let (code, body) = fixture.run_json(&["--json", "clean"]);

    assert_eq!(code, 2, "clean succeeds with a warning");
    let preserved = &body["data"]["preserved"][0];
    assert_eq!(preserved["skill"], "release-notes");
    assert_eq!(preserved["reason"], "dirty-projected-copy");
    assert!(
        fixture.generated_copy().contains("keep my edit"),
        "clean does not delete unrecorded edits"
    );
}

/// Every surface that reports an edited copy left by a removed import must
/// name a repair that works. `update` refuses the name outright, so the update
/// strategies the installed case names would exit with 3.
#[test]
fn advice_for_an_edited_copy_of_a_removed_import_names_a_working_repair() {
    let fixture = DriftFixture::install();
    fixture.edit_generated_copy("keep my edit");
    let copy = fixture
        .workspace
        .path()
        .join(".claude/skills/release-notes/SKILL.md");

    // The removal embeds a sync report, whose advice must also survive the
    // record the removal deletes on its way out.
    let (code, body) = fixture.run_json(&["--json", "remove", "release-notes"]);
    assert_eq!(code, 2, "remove reports the retained artifact: {body}");
    let preserved = serde_json::to_string(&body["data"]["projection"]["preserved"])
        .expect("the embedded report serializes");
    assert!(
        preserved.contains("skillctl sync --discard") && !preserved.contains("--strategy"),
        "the removal's own report must not name an update that exits 3: {preserved}",
    );
    assert!(
        body["warnings"]
            .as_array()
            .is_some_and(|warnings| warnings.iter().any(|warning| warning
                .as_str()
                .is_some_and(|warning| warning.contains("skillctl sync --discard")))),
        "remove warns about the copy it leaves behind: {body}",
    );
    assert!(
        fs::read_to_string(&copy)
            .expect("the edited copy survives the removal")
            .contains("keep my edit"),
        "remove preserves unrecorded edits"
    );

    // The strategies the advice used to name refuse the removed name.
    let (code, body) = fixture.run_json(&[
        "--json",
        "update",
        "release-notes",
        "--apply",
        "--strategy",
        "overlay",
    ]);
    assert_eq!(code, 3, "update refuses a skill no record holds: {body}");

    fixture.assert_names_working_repair("sync", &["--json", "sync"], "release-notes", 2);
    fixture.assert_names_working_repair("clean", &["--json", "clean"], "release-notes", 2);
    fixture.assert_names_working_repair("doctor", &["--json", "doctor"], "release-notes", 2);
    fixture.assert_names_working_repair(
        "explain",
        &["--json", "explain", "release-notes"],
        "release-notes",
        2,
    );

    // The named repair resolves the leftover copy.
    let (code, body) = fixture.run_json(&["--json", "sync", "--discard"]);
    assert_eq!(code, 0, "the advertised discard repairs the copy: {body}");
    assert!(
        !copy.exists(),
        "the edited copy the removal left behind is gone: {body}"
    );
}

/// A canonical local skill has no install record either, so the same rule holds
/// for its generated copy: no surface may name an update that exits with 3.
#[test]
fn advice_for_an_edited_copy_of_a_canonical_local_skill_names_a_working_repair() {
    let workspace = TestWorkspace::new();
    workspace.write_manifest(CLAUDE_MANIFEST);
    workspace.write_lockfile(MINIMAL_LOCKFILE.as_str());
    workspace.write_workspace_skill("sy", "A local canonical skill.", &[]);
    let fixture = DriftFixture { workspace };

    fixture.run_json(&["--json", "sync"]);
    fixture.edit_generated_skill("sy", "keep my edit");

    let (code, body) =
        fixture.run_json(&["--json", "update", "sy", "--apply", "--strategy", "overlay"]);
    assert_eq!(code, 3, "update refuses a canonical skill: {body}");

    fixture.assert_names_working_repair("sync", &["--json", "sync"], "sy", 2);
    fixture.assert_names_working_repair("clean", &["--json", "clean"], "sy", 2);
    fixture.assert_names_working_repair("doctor", &["--json", "doctor"], "sy", 2);
    fixture.assert_names_working_repair("explain", &["--json", "explain", "sy"], "sy", 0);

    let (code, body) = fixture.run_json(&["--json", "sync", "--discard"]);
    assert_eq!(code, 0, "the advertised discard repairs the copy: {body}");
    let regenerated =
        fs::read_to_string(fixture.workspace.path().join(".claude/skills/sy/SKILL.md"))
            .expect("the copy is regenerated from the canonical skill");
    assert!(
        !regenerated.contains("keep my edit"),
        "the edit is backed up and dropped: {regenerated}"
    );
}

#[test]
fn removing_another_skill_preserves_edited_generated_copies() {
    let fixture = DriftFixture::install();
    fixture.install_second_skill();
    fixture.edit_generated_copy("keep my edit");

    let (_, body) = fixture.run_json(&["--json", "remove", "bug-triage"]);

    assert_eq!(body["ok"], true, "the removal completes: {body}");
    assert!(
        fixture.generated_copy().contains("keep my edit"),
        "removing an unrelated skill must not discard a direct edit"
    );
    assert!(
        !fixture
            .workspace
            .path()
            .join(".claude/skills/bug-triage")
            .exists(),
        "the removed skill's own clean projection is pruned"
    );
}

#[test]
fn the_bundled_skill_projection_survives_direct_edits() {
    let fixture = DriftFixture::install_only();
    // Bootstrap the bundled skill before editing its generated copy.
    fixture.run_json(&["--json", "telemetry", "status"]);

    let bundled = fixture
        .workspace
        .home_path()
        .join(".agents/skills/skillctl/SKILL.md");
    let mut contents = fs::read_to_string(&bundled).expect("bundled projection exists");
    contents.push_str("\nmy local note\n");
    fs::write(&bundled, contents).expect("bundled projection edited");

    let (_, body) = fixture.run_json(&["--json", "doctor"]);

    let issues = body["data"]["issues"].as_array();
    assert!(
        issues.is_some_and(|issues| issues
            .iter()
            .any(|issue| issue["code"] == "bundled-skill-conflict")),
        "the edited bundled copy is reported, not rewritten: {issues:?}"
    );
    assert!(
        fs::read_to_string(&bundled)
            .expect("bundled projection exists")
            .contains("my local note"),
        "the edit survives every skillctl command"
    );
}

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

/// Workspace whose generated copies hold symlinks instead of file copies.
struct SymlinkProjectionFixture {
    workspace: TestWorkspace,
}

impl SymlinkProjectionFixture {
    /// The symlink-mode warning makes install exit with 2 instead of 0.
    fn install() -> Self {
        let workspace = TestWorkspace::new();
        workspace.write_manifest(SYMLINK_CLAUDE_MANIFEST);
        workspace.write_skill_source("git-source", "release-notes");
        // A second file makes the projection hold more than one link, so a
        // drift check must cover every link target.
        workspace.write_file(
            "git-source/.agents/skills/release-notes/notes.md",
            "# Notes\n",
        );
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

    fn projection_dir(&self) -> PathBuf {
        self.workspace.path().join(".claude/skills/release-notes")
    }

    fn projected(&self, name: &str) -> PathBuf {
        self.projection_dir().join(name)
    }

    fn projected_manifest_is_symlink(&self) -> bool {
        fs::symlink_metadata(self.projected("SKILL.md"))
            .expect("projected manifest exists")
            .file_type()
            .is_symlink()
    }

    /// Replace one projected link with a regular edited file, the way an editor
    /// that refuses to follow the link leaves the projection behind.
    fn replace_projected_link_with_an_edited_file(&self, note: &str) {
        let path = self.projected("SKILL.md");
        fs::remove_file(&path).expect("projected link is removed");
        fs::write(
            &path,
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
        .expect("edited file is written");
    }

    /// Install an unrelated second skill into the same workspace. The install
    /// exits with 2 because symlink mode always carries its warning.
    fn install_second_skill(&self) {
        self.workspace
            .write_skill_source("git-source-2", "bug-triage");
        self.workspace.init_git_repo("git-source-2");
        let output = Command::cargo_bin("skillctl")
            .expect("binary exists")
            .current_dir(self.workspace.path())
            .env("HOME", self.workspace.home_path())
            .args([
                "--no-input",
                "--name",
                "bug-triage",
                "install",
                self.workspace.git_repo_url("git-source-2").as_str(),
            ])
            .output()
            .expect("install runs");
        assert!(
            output.stderr.is_empty(),
            "unexpected stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("Installed 1 skill"),
            "the unrelated install completed: {stdout}"
        );
    }

    /// Drop the recorded digest, as in a copy written before digests existed.
    ///
    /// The drift guard then has to compare the projected links against the
    /// digest of the current plan instead of a stored baseline.
    fn drop_recorded_digest(&self) {
        remove_recorded_digest(&self.projection_dir());
    }

    /// Append to the stored import through the projected link, the way an
    /// editor that follows the link writes the runtime copy.
    ///
    /// The link points into `~/.skillctl/store/imports`, so the write lands in
    /// the pinned bytes themselves.
    fn write_through_projected_link(&self, note: &str) {
        use std::io::Write as _;

        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(self.projected("SKILL.md"))
            .expect("the projected link is opened through");
        writeln!(file, "{note}").expect("the note is written through the link");
    }

    /// The stored import file a projected link points at.
    fn stored_manifest(&self) -> PathBuf {
        fs::read_link(self.projected("SKILL.md")).expect("the projected manifest is a link")
    }

    /// Text of the stored import file the projection links into.
    fn stored_manifest_contents(&self) -> String {
        fs::read_to_string(self.stored_manifest()).expect("stored import is readable")
    }

    fn lockfile_bytes(&self) -> Vec<u8> {
        fs::read(self.workspace.path().join(".agents/skillctl.lock")).expect("lockfile is readable")
    }

    fn recorded_digest(&self) -> Option<String> {
        let metadata: Value = serde_json::from_str(
            &fs::read_to_string(self.projection_dir().join(".skillctl-projection.json"))
                .expect("projection metadata exists"),
        )
        .expect("projection metadata is valid json");
        metadata["content_digest"].as_str().map(str::to_string)
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

    /// Advance the second skill's upstream and return the new revision.
    fn advance_second_upstream(&self, description: &str) -> String {
        self.workspace.write_skill_source_at(
            "git-source-2",
            ".agents/skills/bug-triage",
            "bug-triage",
            description,
        );
        self.workspace
            .commit_all("git-source-2", "advance upstream");
        self.workspace.git_head("git-source-2")
    }

    fn lockfile_digest(&self) -> String {
        let contents =
            fs::read_to_string(self.workspace.path().join(".agents/skillctl.lock")).expect("ok");
        let mut hasher = Sha256::new();
        hasher.update(contents.as_bytes());
        format!("{:x}", hasher.finalize())
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
}

#[test]
fn a_symlink_projection_without_a_recorded_digest_matches_the_plan() {
    let fixture = SymlinkProjectionFixture::install();
    fixture.drop_recorded_digest();

    let (code, body) = fixture.run_json(&["--json", "sync"]);

    assert_eq!(code, 2, "only the symlink-mode warning remains: {body}");
    assert_eq!(body["ok"], true, "the sync succeeds: {body}");
    let preserved = body["data"]["preserved"].as_array();
    assert!(
        preserved.is_none_or(Vec::is_empty),
        "links that match the plan are not drift: {preserved:?}"
    );
    assert!(
        fixture.projected_manifest_is_symlink(),
        "the projection is regenerated from the plan"
    );
    assert!(
        fixture
            .recorded_digest()
            .is_some_and(|digest| digest.starts_with("sha256:")),
        "the regenerated copy records its digest"
    );
}

#[test]
fn sync_preserves_a_symlink_projection_replaced_by_an_edited_file() {
    let fixture = SymlinkProjectionFixture::install();
    fixture.replace_projected_link_with_an_edited_file("keep my edit");

    let (code, body) = fixture.run_json(&["--json", "sync"]);

    assert_eq!(code, 2, "a preserved copy adds a warning: {body}");
    let preserved = &body["data"]["preserved"][0];
    assert_eq!(preserved["skill"], "release-notes");
    assert_eq!(preserved["reason"], "dirty-projected-copy");
    assert!(
        !fixture.projected_manifest_is_symlink(),
        "the edited entry stays a regular file"
    );
    assert!(
        fs::read_to_string(fixture.projected("SKILL.md"))
            .expect("edited entry is readable")
            .contains("keep my edit"),
        "the edit survives the sync"
    );
    assert!(
        fs::symlink_metadata(fixture.projected("notes.md"))
            .expect("untouched link exists")
            .file_type()
            .is_symlink(),
        "the untouched sibling link survives the sync"
    );
}

/// An editor that follows a projected symlink writes the import store itself,
/// so the drift guard must report the divergence instead of failing to resolve,
/// and the repair it names must repair the store.
#[test]
fn sync_preserves_a_copy_whose_store_was_written_through_its_links() {
    let fixture = SymlinkProjectionFixture::install();
    fixture.write_through_projected_link("# local note");

    assert!(
        fixture
            .stored_manifest()
            .starts_with(fixture.workspace.home_path().join(".skillctl")),
        "the projected link points into the import store"
    );
    assert!(
        fixture.stored_manifest_contents().contains("# local note"),
        "the write-through changed the stored import"
    );

    let (code, body) = fixture.run_json(&["--json", "sync"]);

    assert_eq!(code, 2, "a diverged store adds a warning: {body}");
    assert_eq!(
        body["ok"], true,
        "the sync reports the divergence instead of failing: {body}"
    );
    let preserved = &body["data"]["preserved"][0];
    assert_eq!(preserved["skill"], "release-notes");
    assert_eq!(preserved["reason"], "diverged-import-store");
    let repair = preserved["repair"].as_str().expect("repair advice exists");
    assert!(
        repair.contains("skillctl restore --frozen") && repair.contains("reinstall"),
        "the repair names commands that repair the store: {repair}"
    );
    assert!(
        !repair.contains("skillctl update release-notes"),
        "update resolves through the same diverged store, so it cannot repair this: {repair}"
    );
    assert!(
        fixture.projected_manifest_is_symlink(),
        "the projection keeps its links"
    );
    assert!(
        fixture.stored_manifest_contents().contains("# local note"),
        "the sync does not silently drop the edit"
    );
}

/// A diverged store is not an edited copy. `sync --discard` would only relink
/// to the same diverged bytes, so the divergence must stay reported instead of
/// being discarded as if the copy itself held the edit.
#[test]
fn sync_discard_cannot_repair_a_diverged_import_store() {
    let fixture = SymlinkProjectionFixture::install();
    fixture.write_through_projected_link("# local note");

    let (code, body) = fixture.run_json(&["--json", "sync", "--discard"]);

    assert_eq!(code, 2, "the divergence outlives the discard: {body}");
    assert_eq!(body["ok"], true, "the discard still succeeds: {body}");
    assert_eq!(
        body["data"]["preserved"][0]["reason"], "diverged-import-store",
        "the copy stays preserved: {body}"
    );
    assert!(
        body["data"]["discarded"]
            .as_array()
            .is_none_or(Vec::is_empty),
        "nothing is backed up as an edited copy: {body}"
    );
    assert!(
        fixture.stored_manifest_contents().contains("# local note"),
        "an explicit discard must not touch the import store"
    );
}

/// `skillctl update <skill>` and `skillctl verify` refuse a diverged store, so
/// each of their errors must name a repair that actually runs. `update` cannot
/// be that repair: it resolves through the store it is asked to fix.
#[test]
fn update_and_verify_name_working_repairs_for_a_diverged_import_store() {
    let fixture = SymlinkProjectionFixture::install();
    fixture.write_through_projected_link("# local note");

    let (code, body) = fixture.run_json(&["--json", "update", "release-notes"]);

    assert_eq!(code, 3, "the check refuses the diverged store: {body}");
    let error = body["errors"][0].as_str().expect("error exists");
    assert!(
        error.contains("content hash mismatch"),
        "the error names the divergence: {error}"
    );
    assert!(
        error.contains("skillctl restore --frozen") && error.contains("reinstall"),
        "the error names repairs that work: {error}"
    );
    assert!(
        error.contains("'skillctl fork release-notes'"),
        "the error names the fork that keeps the edit: {error}"
    );
    assert!(
        !error.contains("'skillctl update release-notes'"),
        "update reproduces this error, so it must not be listed: {error}"
    );

    let (code, body) = fixture.run_json(&["--json", "verify"]);

    assert_eq!(code, 3, "verify reports the broken pin: {body}");
    let error = body["errors"][0].as_str().expect("error exists");
    assert!(
        error.contains("skillctl restore --frozen") && error.contains("reinstall"),
        "verify names repairs that work: {error}"
    );
    assert!(
        error.contains("'skillctl fork release-notes'"),
        "verify names the fork that keeps the edit: {error}"
    );
}

/// `explain` names the divergence for the skill the reader asked about, the
/// way it names a missing store, instead of reporting plain absence.
#[test]
fn explain_names_the_diverged_import_store_for_the_skill() {
    let fixture = SymlinkProjectionFixture::install();
    fixture.write_through_projected_link("# local note");

    let (code, body) = fixture.run_json(&["--json", "explain", "release-notes"]);

    assert_eq!(code, 3, "the diagnosis is a validation failure: {body}");
    assert_eq!(body["ok"], false, "the report is a failure: {body}");
    let issue = &body["data"]["issues"][0];
    assert_eq!(issue["code"], "import-store-divergence");
    assert_eq!(issue["skill"], "release-notes");
    let fix = issue["fix"].as_str().expect("fix advice exists");
    assert!(
        fix.contains("skillctl restore --frozen") && fix.contains("reinstall"),
        "the fix names repairs that work: {fix}"
    );
    assert!(
        fix.contains("'skillctl fork release-notes'"),
        "the fix names the fork that keeps the edit: {fix}"
    );
}

/// The repair the divergence report names must run: a frozen restore rebuilds
/// the pinned bytes, keeps the lockfile, and leaves the projection healthy.
#[test]
fn restore_frozen_repairs_a_store_written_through_its_projection() {
    let fixture = SymlinkProjectionFixture::install();
    fixture.write_through_projected_link("# local note");
    let lockfile_before = fixture.lockfile_bytes();

    let (code, body) = fixture.run_json(&["--json", "restore", "--frozen"]);

    assert_eq!(code, 0, "the named repair works: {body}");
    assert_eq!(body["ok"], true, "the restore succeeds: {body}");
    assert!(
        !fixture.stored_manifest_contents().contains("# local note"),
        "the pinned content is back in the store"
    );
    assert_eq!(
        fixture.lockfile_bytes(),
        lockfile_before,
        "a frozen restore leaves the lockfile unchanged"
    );

    let (code, body) = fixture.run_json(&["--json", "verify"]);

    assert_eq!(code, 0, "verify is clean after the repair: {body}");
    let (code, body) = fixture.run_json(&["--json", "sync"]);

    assert_eq!(code, 2, "only the symlink-mode warning remains: {body}");
    assert!(
        body["data"]["preserved"]
            .as_array()
            .is_none_or(Vec::is_empty),
        "the projection is no longer divergent: {body}"
    );
}

/// The other named repair works too: a reinstall re-pins the import from its
/// source, which clears the divergence the same way a frozen restore does.
#[test]
fn reinstall_repairs_a_store_written_through_its_projection() {
    let fixture = SymlinkProjectionFixture::install();
    fixture.write_through_projected_link("# local note");
    let source = fixture.workspace.git_repo_url("git-source");

    let (code, body) = fixture.run_json(&[
        "--json",
        "--no-input",
        "--name",
        "release-notes",
        "install",
        source.as_str(),
    ]);

    assert_eq!(code, 2, "only the symlink-mode warning remains: {body}");
    assert_eq!(body["ok"], true, "the reinstall succeeds: {body}");
    assert!(
        !fixture.stored_manifest_contents().contains("# local note"),
        "the reinstall rebuilds the store from the source"
    );

    let (code, body) = fixture.run_json(&["--json", "verify"]);

    assert_eq!(code, 0, "verify is clean after the repair: {body}");
}

/// The W05 contract also holds for a write-through: an unrelated install must
/// not discard it, and it must be reported rather than dropped.
#[test]
fn an_unrelated_install_preserves_a_write_through_a_projected_link() {
    let fixture = SymlinkProjectionFixture::install();
    fixture.write_through_projected_link("# local note");
    fixture.install_second_skill();

    assert!(
        fixture.stored_manifest_contents().contains("# local note"),
        "an unrelated install must not discard a direct edit"
    );
    assert!(
        fixture.projected_manifest_is_symlink(),
        "the edited projection keeps its links"
    );
    let (code, body) = fixture.run_json(&["--json", "sync"]);

    assert_eq!(code, 2, "the divergence is still reported: {body}");
    assert_eq!(
        body["data"]["preserved"][0]["reason"],
        "diverged-import-store"
    );
}

/// The divergence report names `skillctl fork <skill>` as the way to keep the
/// edit, so the fork must copy the diverged bytes into local ownership.
#[test]
fn fork_keeps_a_write_through_edit_before_the_store_is_repaired() {
    let fixture = SymlinkProjectionFixture::install();
    fixture.write_through_projected_link("# local note");

    let (code, body) = fixture.run_json(&["--json", "fork", "release-notes"]);

    assert_eq!(code, 0, "the fork takes local ownership: {body}");
    assert_eq!(body["ok"], true, "the fork succeeds: {body}");
    let canonical = fs::read_to_string(
        fixture
            .workspace
            .path()
            .join(".agents/skills/release-notes/SKILL.md"),
    )
    .expect("the canonical skill exists");
    assert!(
        canonical.contains("# local note"),
        "the fork keeps the edit: {canonical}"
    );
}

/// `skillctl update <skill> --apply --strategy fork` refuses on a diverged
/// store, because the check it rides on reads the broken baseline. That
/// refusal is honest only while it names the repair that keeps the stored
/// bytes, which is the standalone `skillctl fork <skill>`.
#[test]
fn update_fork_strategy_refuses_and_names_the_standalone_fork() {
    let fixture = SymlinkProjectionFixture::install();
    fixture.write_through_projected_link("# local note");

    let (code, body) = fixture.run_json(&[
        "--json",
        "update",
        "release-notes",
        "--apply",
        "--strategy",
        "fork",
    ]);

    assert_eq!(
        code, 3,
        "the fork strategy refuses on a diverged store: {body}"
    );
    let error = body["errors"][0].as_str().expect("error exists");
    assert!(
        error.contains("content hash mismatch"),
        "the error names the divergence: {error}"
    );
    assert!(
        error.contains("no update strategy can run through it"),
        "the error says no update strategy applies, fork included: {error}"
    );
    assert!(
        error.contains("'skillctl fork release-notes'"),
        "the error names the standalone fork that keeps the bytes: {error}"
    );
    assert!(
        error.contains("skillctl restore --frozen") && error.contains("reinstall"),
        "the error names the repairs that rebuild the store: {error}"
    );
    assert!(
        !error.contains("'skillctl update release-notes'"),
        "update reproduces this error, so it must not be listed: {error}"
    );
    assert!(
        fixture.stored_manifest_contents().contains("# local note"),
        "the refusal changes nothing: {body}"
    );

    // The repair the refusal names must run and keep the stored bytes.
    let (code, body) = fixture.run_json(&["--json", "fork", "release-notes"]);

    assert_eq!(code, 0, "the named fork runs: {body}");
    let canonical = fs::read_to_string(
        fixture
            .workspace
            .path()
            .join(".agents/skills/release-notes/SKILL.md"),
    )
    .expect("the canonical skill exists");
    assert!(
        canonical.contains("# local note"),
        "the named fork keeps the stored bytes: {canonical}"
    );
}

/// One diverged store blocks only its own import: a run that names another
/// skill must still apply that skill's update while the divergence stays
/// reported, because the divergence belongs to the import, not the workspace.
#[test]
fn an_unrelated_update_applies_while_one_imports_store_diverges() {
    let fixture = SymlinkProjectionFixture::install();
    fixture.install_second_skill();
    fixture.write_through_projected_link("# local note");
    let revision = fixture.advance_second_upstream("Triage with a new checklist.");

    let (code, body) = fixture.run_json(&["--json", "update", "bug-triage", "--apply"]);

    assert_eq!(code, 0, "the unrelated skill updates: {body}");
    assert_eq!(body["ok"], true, "the run succeeds: {body}");
    let applied = body["data"]["applied"]
        .as_array()
        .expect("the apply is reported");
    assert_eq!(
        applied.len(),
        1,
        "only the named skill is applied: {applied:?}"
    );
    assert_eq!(applied[0]["skill"], "bug-triage");
    assert_eq!(
        applied[0]["resolved_revision"], revision,
        "the unrelated skill moves to the new revision: {applied:?}"
    );
    assert!(
        fixture.stored_manifest_contents().contains("# local note"),
        "the diverged store keeps its edit"
    );
    let preserved = body["data"]["preserved"]
        .as_array()
        .expect("the preserved copies are reported");
    assert!(
        preserved
            .iter()
            .any(|entry| entry["skill"] == "release-notes"
                && entry["reason"] == "diverged-import-store"),
        "the diverged copy stays reported: {preserved:?}"
    );
}

/// A whole-workspace check plans around the divergence: the diverged import
/// reports a blocked plan that names the working repairs, and every other
/// skill keeps its normal outcome.
#[test]
fn a_workspace_wide_update_plans_around_a_diverged_store() {
    let fixture = SymlinkProjectionFixture::install();
    fixture.install_second_skill();
    fixture.write_through_projected_link("# local note");
    fixture.advance_second_upstream("Triage with a new checklist.");

    let (code, body) = fixture.run_json(&["--json", "update"]);

    assert_eq!(code, 0, "the check runs for every skill: {body}");
    let plans = body["data"]["plans"].as_array().expect("plans exist");
    let diverged = plans
        .iter()
        .find(|plan| plan["skill"] == "release-notes")
        .expect("the diverged import keeps its plan");
    assert_eq!(diverged["outcome"], "blocked");
    assert_eq!(diverged["store_diverged"], true);
    assert_eq!(diverged["recommended_action"], "skip");
    assert_eq!(diverged["available_actions"], json!(["skip"]));
    let notes = diverged["notes"].as_array().expect("notes exist");
    let note = notes
        .iter()
        .map(|note| note.as_str().unwrap_or_default())
        .collect::<String>();
    assert!(
        note.contains("no longer hashes to the pinned content")
            && note.contains("skillctl restore --frozen")
            && note.contains("skillctl fork release-notes"),
        "the note names the store repair and the way to keep the edit: {note}"
    );
    let unrelated = plans
        .iter()
        .find(|plan| plan["skill"] == "bug-triage")
        .expect("the unrelated skill keeps its plan");
    assert_eq!(
        unrelated["outcome"], "update-available",
        "the unrelated skill is not blocked: {unrelated}"
    );
}

/// A whole-workspace apply skips the diverged import instead of refusing the
/// run: every strategy resolves through the store it would have to repair, so
/// the skip reason names the repairs that rebuild the store, and the apply's
/// sync still preserves the diverged copy.
#[test]
fn a_workspace_wide_apply_skips_a_diverged_import_and_applies_the_rest() {
    let fixture = SymlinkProjectionFixture::install();
    fixture.install_second_skill();
    fixture.write_through_projected_link("# local note");
    let revision = fixture.advance_second_upstream("Triage with a new checklist.");

    let (code, body) = fixture.run_json(&["--json", "update", "--apply"]);

    assert_eq!(code, 0, "the run applies the skills it can: {body}");
    let applied = body["data"]["applied"].as_array().expect("applied exist");
    assert_eq!(
        applied
            .iter()
            .map(|entry| entry["skill"].as_str().unwrap_or_default())
            .collect::<Vec<_>>(),
        vec!["bug-triage"],
        "only the healthy import moves: {applied:?}"
    );
    assert_eq!(applied[0]["resolved_revision"], revision);
    let skipped = body["data"]["apply_skipped"]
        .as_array()
        .expect("skipped exist");
    let diverged_skip = skipped
        .iter()
        .find(|entry| entry["skill"] == "release-notes")
        .expect("the diverged import is skipped with a reason");
    let reason = diverged_skip["reason"].as_str().expect("reason exists");
    assert!(
        reason.contains("skillctl restore --frozen") && reason.contains("reinstall"),
        "the skip names repairs that work: {reason}"
    );
    assert!(
        !reason.contains("'skillctl update release-notes'"),
        "update reproduces the divergence, so it must not be named: {reason}"
    );
    assert!(
        fixture.stored_manifest_contents().contains("# local note"),
        "the diverged store keeps its edit"
    );
}

#[test]
#[cfg(unix)]
fn sync_preserves_a_projection_whose_link_was_retargeted() {
    let fixture = SymlinkProjectionFixture::install();
    let path = fixture.projected("SKILL.md");
    fs::remove_file(&path).expect("projected link is removed");
    std::os::unix::fs::symlink("notes.md", &path).expect("retargeted link is created");
    let (code, body) = fixture.run_json(&["--json", "sync"]);

    assert_eq!(code, 2, "a preserved copy adds a warning: {body}");
    let preserved = &body["data"]["preserved"][0];
    assert_eq!(preserved["skill"], "release-notes");
    assert_eq!(preserved["reason"], "dirty-projected-copy");
    assert_eq!(
        fs::read_link(&path).expect("retargeted link is readable"),
        PathBuf::from("notes.md"),
        "the drift guard must not rewrite the retargeted link"
    );
}

#[test]
fn sync_discard_backs_up_and_regenerates_a_symlink_projection() {
    let fixture = SymlinkProjectionFixture::install();
    fixture.replace_projected_link_with_an_edited_file("temporary edit");

    let (code, body) = fixture.run_json(&["--json", "sync", "--discard"]);

    assert_eq!(code, 2, "only the symlink-mode warning remains: {body}");
    assert_eq!(body["ok"], true, "an explicit discard succeeds: {body}");
    let discarded = &body["data"]["discarded"][0];
    assert_eq!(discarded["skill"], "release-notes");
    let backup = discarded["backup"].as_str().expect("backup exists");
    assert!(
        backup.contains(".skillctl/store/discards"),
        "the backup is durable: {backup}"
    );
    assert!(
        fs::read_to_string(PathBuf::from(backup).join("SKILL.md"))
            .expect("backed-up content exists")
            .contains("temporary edit"),
        "the discarded edit is recoverable"
    );
    assert!(
        fs::symlink_metadata(PathBuf::from(backup).join("notes.md"))
            .expect("backed-up link exists")
            .file_type()
            .is_symlink(),
        "the backup keeps the projected links as links"
    );
    assert!(
        fixture.projected_manifest_is_symlink(),
        "the copy is regenerated from links"
    );
    assert!(
        !fs::read_to_string(fixture.projected("SKILL.md"))
            .expect("regenerated copy is readable")
            .contains("temporary edit"),
        "the regenerated copy holds the stored revision"
    );
}

#[test]
fn a_clean_apply_refuses_to_replace_edits_in_a_symlink_projection() {
    let fixture = SymlinkProjectionFixture::install();
    fixture.advance_upstream("Updated upstream release notes helper.");
    fixture.replace_projected_link_with_an_edited_file("keep my edit");
    let before = fixture.lockfile_digest();

    let (code, body) = fixture.run_json(&["--json", "update", "release-notes", "--apply"]);

    assert_eq!(
        code, 3,
        "the refusal is a validation failure, not an operational error: {body}"
    );
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
    assert!(
        fs::read_to_string(fixture.projected("SKILL.md"))
            .expect("edited entry is readable")
            .contains("keep my edit"),
        "the edit survives the refusal"
    );
}

#[test]
fn update_apply_preserves_symlink_edits_as_an_overlay() {
    let fixture = SymlinkProjectionFixture::install();
    let new_revision = fixture.advance_upstream("Updated upstream release notes helper.");
    fixture.replace_projected_link_with_an_edited_file("keep my edit");

    let (_, body) = fixture.run_json(&[
        "--json",
        "update",
        "release-notes",
        "--apply",
        "--strategy",
        "overlay",
    ]);

    assert_eq!(body["ok"], true, "the overlay apply succeeds: {body}");
    assert!(
        body["data"]["preserved"]
            .as_array()
            .expect("preserved list exists")
            .iter()
            .any(|entry| entry["reason"] == "moved-to-overlay"),
        "the edit is reported as overlay content: {body}"
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
    assert_eq!(
        lockfile["imports"]["release-notes"]["revision"]["resolved"]
            .as_str()
            .unwrap_or("<missing>"),
        new_revision.as_str(),
        "the new revision was applied"
    );
    assert!(
        fixture.projected_manifest_is_symlink(),
        "the rebuilt projection still uses links"
    );
    assert!(
        fs::read_to_string(fixture.projected("SKILL.md"))
            .expect("projection is readable")
            .contains("keep my edit"),
        "the projection still shows the preserved edit"
    );
}

#[test]
fn update_apply_discards_symlink_edits_after_a_durable_backup() {
    let fixture = SymlinkProjectionFixture::install();
    let new_revision = fixture.advance_upstream("Updated upstream release notes helper.");
    fixture.replace_projected_link_with_an_edited_file("temporary edit");

    let (_, body) = fixture.run_json(&[
        "--json",
        "update",
        "release-notes",
        "--apply",
        "--strategy",
        "discard",
    ]);

    assert_eq!(body["ok"], true, "the discard apply succeeds: {body}");
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

    assert!(
        fixture.projected_manifest_is_symlink(),
        "the rebuilt projection still uses links"
    );
    let projected =
        fs::read_to_string(fixture.projected("SKILL.md")).expect("projection is readable");
    assert!(
        projected.contains("Updated upstream release notes helper."),
        "the projection now holds the applied revision"
    );
    assert!(!projected.contains("temporary edit"));

    let lockfile = fixture.workspace.read_lockfile_yaml();
    assert_eq!(
        lockfile["imports"]["release-notes"]["revision"]["resolved"]
            .as_str()
            .unwrap_or("<missing>"),
        new_revision.as_str(),
        "the applied revision is recorded"
    );
}

#[test]
fn update_checks_flag_a_directly_edited_copy_even_after_a_preserving_sync() {
    let fixture = DriftFixture::install();
    fixture.edit_generated_copy("keep my edit");
    let (code, _) = fixture.run_json(&["--json", "sync"]);
    assert_eq!(code, 2);

    let (_, body) = fixture.run_json(&["--json", "update", "release-notes"]);
    let plan = &body["data"]["plans"][0];
    assert_eq!(
        plan["local_modification_detected"], true,
        "the check agrees with the apply guard: {plan}"
    );
}

/// Manifest that plans the shared neutral root the default targets read.
const DEFAULT_TARGETS_MANIFEST: &str = concat!(
    "version: 1\n",
    "\n",
    "targets:\n",
    "  - codex\n",
    "  - gemini-cli\n",
    "  - opencode\n",
);

/// Workspace whose only planned root is the shared `.agents/skills` root.
///
/// The default targets share that root, so retargeting to Claude Code alone
/// drops it out of the plan while every adapter still documents it.
struct DroppedRootFixture {
    workspace: TestWorkspace,
}

impl DroppedRootFixture {
    fn install() -> Self {
        let workspace = TestWorkspace::new();
        workspace.write_manifest(DEFAULT_TARGETS_MANIFEST);
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

        assert!(
            workspace
                .path()
                .join(".agents/skills/release-notes/SKILL.md")
                .exists(),
            "the default targets share the .agents/skills root"
        );
        Self { workspace }
    }

    /// Edit the generated copy that sits in the root the retarget drops.
    fn edit_dropped_copy(&self, note: &str) {
        let path = self.dropped_root().join("SKILL.md");
        let mut contents = fs::read_to_string(&path).expect("generated copy exists");
        contents.push_str(note);
        contents.push('\n');
        fs::write(&path, contents).expect("generated copy edited");
    }

    /// Drop the recorded digest of the dropped copy, as in a copy written
    /// before digests existed.
    fn drop_recorded_digest(&self) {
        remove_recorded_digest(&self.dropped_root());
    }

    /// The generated copy directory the retarget drops out of the plan.
    fn dropped_root(&self) -> PathBuf {
        self.workspace.path().join(".agents/skills/release-notes")
    }

    /// Move the whole plan onto Claude Code, which no default target reads.
    fn retarget_to_claude_code(&self) {
        let manifest_path = self.workspace.path().join(".agents/skillctl.yaml");
        let manifest = fs::read_to_string(&manifest_path).expect("manifest exists");
        let retargeted = manifest.replace(
            concat!(
                "targets:\n",
                "  - codex\n",
                "  - gemini-cli\n",
                "  - opencode\n"
            ),
            "targets:\n  - claude-code\n",
        );
        assert_ne!(
            &retargeted, &manifest,
            "the manifest must hold the default target set before the retarget"
        );
        fs::write(&manifest_path, retargeted).expect("manifest target set updates");
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
}

/// The repair the check recommends must resolve a copy in a root the plan no
/// longer includes, because the same surfaces advertise it there.
#[test]
fn an_advertised_repair_resolves_an_edit_in_a_root_the_plan_dropped() {
    let fixture = DroppedRootFixture::install();
    fixture.edit_dropped_copy("keep my edit");
    fixture.retarget_to_claude_code();

    let (code, body) = fixture.run_json(&["--json", "update", "release-notes"]);
    assert_eq!(code, 2, "the missing planned projection warns: {body}");
    let plan = &body["data"]["plans"][0];
    assert_eq!(plan["outcome"], "up-to-date");
    assert_eq!(
        plan["local_modification_detected"], true,
        "the check keeps reporting the edit: {plan}"
    );
    assert_eq!(
        plan["recommended_action"], "create-overlay",
        "the check still advertises the edit repair: {plan}"
    );
    assert_eq!(
        plan["modifications"],
        json!([{
            "kind": "projected-copy",
            "managed": false,
            "path": ".agents/skills/release-notes/SKILL.md",
            "details": "projected runtime copy differs from the recorded effective skill",
        }]),
        "the modification names the dropped root: {plan}",
    );

    let (code, body) = fixture.run_json(&[
        "--json",
        "update",
        "release-notes",
        "--apply",
        "--strategy",
        "overlay",
    ]);
    assert_eq!(
        code, 0,
        "the advertised overlay repair resolves the copy: {body}"
    );
    assert_eq!(
        body["data"]["applied"][0]["skill"], "release-notes",
        "the repair is reported as applied: {body}"
    );
    assert_eq!(
        body["data"]["preserved"][0]["path"], ".agents/overlays/release-notes/SKILL.md",
        "the edit moved into the managed overlay: {body}"
    );

    assert!(
        !fixture.dropped_root().exists(),
        "the copy in the dropped root is resolved, not left behind"
    );
    let overlay = fs::read_to_string(
        fixture
            .workspace
            .path()
            .join(".agents/overlays/release-notes/SKILL.md"),
    )
    .expect("overlay file exists");
    let projected = fs::read_to_string(
        fixture
            .workspace
            .path()
            .join(".claude/skills/release-notes/SKILL.md"),
    )
    .expect("the planned root is materialized");
    assert!(
        overlay.contains("keep my edit"),
        "the edit survives: {overlay}"
    );
    assert!(
        projected.contains("keep my edit"),
        "the planned root projects the overlay content: {projected}"
    );

    let (code, body) = fixture.run_json(&["--json", "update", "release-notes"]);
    assert_eq!(code, 0, "the resolved state checks clean: {body}");
    assert_eq!(
        body["data"]["plans"][0]["local_modification_detected"], false,
        "no surface reports the copy any more: {body}"
    );
}

/// A copy written before digests were recorded holds no baseline of its own,
/// so the repair must judge it against the content of the pinned source. The
/// check already reports the copy through the projection record it still
/// carries, so the advertised repair has to resolve it.
#[test]
fn an_advertised_repair_resolves_a_digestless_copy_in_a_root_the_plan_dropped() {
    let fixture = DroppedRootFixture::install();
    fixture.drop_recorded_digest();
    fixture.edit_dropped_copy("keep my edit");
    fixture.retarget_to_claude_code();

    let (code, body) = fixture.run_json(&["--json", "update", "release-notes"]);
    assert_eq!(code, 2, "the missing planned projection warns: {body}");
    let plan = &body["data"]["plans"][0];
    assert_eq!(
        plan["local_modification_detected"], true,
        "the check reports the digestless edit: {plan}"
    );
    assert_eq!(
        plan["modifications"][0]["path"], ".agents/skills/release-notes/SKILL.md",
        "the modification names the dropped root: {plan}"
    );

    let (code, body) = fixture.run_json(&[
        "--json",
        "update",
        "release-notes",
        "--apply",
        "--strategy",
        "overlay",
    ]);
    assert_eq!(
        code, 0,
        "the advertised overlay repair resolves the digestless copy: {body}"
    );
    assert_eq!(
        body["data"]["preserved"][0]["reason"], "moved-to-overlay",
        "the edit is reported as overlay content: {body}"
    );

    assert!(
        !fixture.dropped_root().exists(),
        "the digestless copy in the dropped root is resolved, not left behind"
    );
    let overlay = fs::read_to_string(
        fixture
            .workspace
            .path()
            .join(".agents/overlays/release-notes/SKILL.md"),
    )
    .expect("overlay file exists");
    assert!(
        overlay.contains("keep my edit"),
        "the edit survives: {overlay}"
    );
    let metadata: Value = serde_json::from_str(
        &fs::read_to_string(
            fixture
                .workspace
                .path()
                .join(".claude/skills/release-notes/.skillctl-projection.json"),
        )
        .expect("the planned root is materialized"),
    )
    .expect("projection metadata is valid json");
    assert!(
        metadata["content_digest"].is_string(),
        "the rebuilt copy records a digest again: {metadata}"
    );

    let (code, body) = fixture.run_json(&["--json", "update", "release-notes"]);
    assert_eq!(code, 0, "the resolved state checks clean: {body}");
    assert_eq!(
        body["data"]["plans"][0]["local_modification_detected"], false,
        "no surface reports the copy any more: {body}"
    );
}

/// The unverifiable-copy advice names the overlay update, so that update must
/// resolve a digestless copy in one pass: the repair moves the edit into the
/// overlay before the new revision is materialized, instead of leaving the
/// copy preserved and unverifiable forever.
#[test]
fn an_advertised_repair_resolves_a_digestless_copy_while_upstream_advanced() {
    let fixture = DriftFixture::install();
    fixture.drop_recorded_digest();
    fixture.edit_generated_copy("keep my edit");
    fixture.advance_upstream("Updated upstream release notes helper.");
    let new_revision = fixture.workspace.git_head("git-source");

    let (code, body) = fixture.run_json(&["--json", "update", "release-notes"]);
    assert_eq!(code, 0, "the check reports the edit: {body}");
    let plan = &body["data"]["plans"][0];
    assert_eq!(
        plan["local_modification_detected"], true,
        "the check reports the digestless edit: {plan}"
    );
    assert_eq!(
        plan["modifications"][0]["path"], ".claude/skills/release-notes/SKILL.md",
        "the modification names the copy: {plan}"
    );

    let (code, body) = fixture.run_json(&[
        "--json",
        "update",
        "release-notes",
        "--apply",
        "--strategy",
        "overlay",
    ]);
    assert_eq!(
        code, 0,
        "the advertised overlay repair resolves the digestless copy: {body}"
    );
    assert_eq!(
        body["data"]["applied"][0]["skill"], "release-notes",
        "the update is reported as applied: {body}"
    );
    assert_eq!(
        body["data"]["preserved"][0]["path"], ".agents/overlays/release-notes/SKILL.md",
        "the edit moved into the managed overlay: {body}"
    );
    let lockfile = fixture.workspace.read_lockfile_yaml();
    assert_eq!(
        lockfile["imports"]["release-notes"]["revision"]["resolved"]
            .as_str()
            .unwrap_or("<missing>"),
        new_revision.as_str(),
        "the new revision was applied: {body}"
    );
    assert!(
        fixture.generated_copy().contains("keep my edit"),
        "the rebuilt copy still shows the preserved edit"
    );

    let (code, body) = fixture.run_json(&[
        "--json",
        "update",
        "release-notes",
        "--apply",
        "--strategy",
        "overlay",
    ]);
    assert_eq!(
        code, 2,
        "a converged state no longer offers an apply: {body}"
    );
    assert!(
        body["warnings"]
            .as_array()
            .expect("warnings exist")
            .iter()
            .any(|warning| warning
                .as_str()
                .expect("warning is a string")
                .contains("check outcome was 'up-to-date'")),
        "the skip names the up-to-date outcome, not an invisible copy: {body}"
    );

    let (code, body) = fixture.run_json(&["--json", "update", "release-notes"]);
    assert_eq!(code, 0, "the resolved state checks clean: {body}");
    assert_eq!(
        body["data"]["plans"][0]["local_modification_detected"], false,
        "no surface reports the copy any more: {body}"
    );
}

/// The same repair must resolve a symlink projection that carries no recorded
/// digest, because the drift guard hashes such a projection by its links
/// against the digest of the current plan.
#[test]
fn an_advertised_repair_resolves_a_digestless_symlink_projection_edit() {
    let fixture = SymlinkProjectionFixture::install();
    fixture.drop_recorded_digest();
    fixture.replace_projected_link_with_an_edited_file("keep my edit");

    let (code, body) = fixture.run_json(&["--json", "update", "release-notes"]);
    assert_eq!(code, 0, "the check reports the edit: {body}");
    let plan = &body["data"]["plans"][0];
    assert_eq!(
        plan["local_modification_detected"], true,
        "the check reports the digestless edit: {plan}"
    );

    let (code, body) = fixture.run_json(&[
        "--json",
        "update",
        "release-notes",
        "--apply",
        "--strategy",
        "overlay",
    ]);
    assert_eq!(
        code, 0,
        "the advertised overlay repair resolves the digestless links: {body}"
    );
    assert_eq!(
        body["data"]["preserved"][0]["reason"], "moved-to-overlay",
        "the edit is reported as overlay content: {body}"
    );

    let overlay = fs::read_to_string(
        fixture
            .workspace
            .path()
            .join(".agents/overlays/release-notes/SKILL.md"),
    )
    .expect("overlay file exists");
    assert!(
        overlay.contains("keep my edit"),
        "the edit survives: {overlay}"
    );
    assert!(
        fixture.projected_manifest_is_symlink(),
        "the rebuilt projection still uses links"
    );
    assert!(
        fs::read_to_string(fixture.projected("SKILL.md"))
            .expect("projection is readable")
            .contains("keep my edit"),
        "the projection still shows the preserved edit"
    );

    let (code, body) = fixture.run_json(&["--json", "update", "release-notes"]);
    assert_eq!(code, 0, "the resolved state checks clean: {body}");
    assert_eq!(
        body["data"]["plans"][0]["local_modification_detected"], false,
        "no surface reports the projection any more: {body}"
    );
}

/// A sync that preserved the copy rebuilds the projection records, so the
/// check must still find the copy and the advertised repair must still work.
#[test]
fn an_advertised_repair_resolves_a_preserved_copy_a_sync_already_reported() {
    let fixture = DroppedRootFixture::install();
    fixture.edit_dropped_copy("temporary edit");
    fixture.retarget_to_claude_code();

    let (code, body) = fixture.run_json(&["--json", "sync"]);
    assert_eq!(code, 2, "the preserved copy warns: {body}");
    let preserved = &body["data"]["preserved"][0];
    assert_eq!(preserved["path"], ".agents/skills/release-notes");
    assert!(
        preserved["repair"]
            .as_str()
            .expect("repair advice exists")
            .contains("update release-notes --apply --strategy discard"),
        "sync advertises the update repair: {preserved}"
    );

    let (_, body) = fixture.run_json(&["--json", "update", "release-notes"]);
    let plan = &body["data"]["plans"][0];
    assert_eq!(
        plan["local_modification_detected"], true,
        "the check agrees with the preserved copy after the records moved: {plan}"
    );
    assert_eq!(
        plan["modifications"][0]["path"], ".agents/skills/release-notes",
        "the modification names the dropped root: {plan}"
    );

    let (code, body) = fixture.run_json(&[
        "--json",
        "update",
        "release-notes",
        "--apply",
        "--strategy",
        "discard",
    ]);
    assert_eq!(
        code, 0,
        "the advertised discard repair resolves the copy: {body}"
    );
    let backup = body["data"]["discarded"][0]["backup"]
        .as_str()
        .expect("backup path exists")
        .to_string();
    assert!(
        fs::read_to_string(PathBuf::from(&backup).join("SKILL.md"))
            .expect("backed-up content exists")
            .contains("temporary edit"),
        "the discarded edit is recoverable from {backup}"
    );
    assert!(
        !fixture.dropped_root().exists(),
        "the copy in the dropped root is resolved, not left behind"
    );
}

/// The repair `disable` advertises must resolve the copy its sync preserved,
/// although the disabled import resolves to no candidate any more.
#[test]
fn an_advertised_repair_resolves_the_copy_a_disable_left_behind() {
    let fixture = DriftFixture::install();
    fixture.edit_generated_copy("keep my edit");

    let (code, body) = fixture.run_json(&["--json", "disable", "release-notes"]);
    assert_eq!(code, 2, "disable succeeds with the preserved copy: {body}");
    assert!(
        body["warnings"]
            .as_array()
            .expect("warnings exist")
            .iter()
            .any(|warning| warning
                .as_str()
                .expect("warning is a string")
                .contains("update release-notes --apply --strategy overlay")),
        "disable advertises the update repair: {body}"
    );

    let (code, body) = fixture.run_json(&["--json", "update", "release-notes"]);
    assert_eq!(code, 0, "the disabled import still checks: {body}");
    let plan = &body["data"]["plans"][0];
    assert_eq!(plan["outcome"], "up-to-date");
    assert_eq!(
        plan["local_modification_detected"], true,
        "the check reports the leftover copy of the disabled import: {plan}"
    );
    assert_eq!(
        plan["modifications"][0]["path"], ".claude/skills/release-notes",
        "the modification names the preserved copy: {plan}"
    );
    let notes = plan["notes"]
        .as_array()
        .expect("notes exist")
        .iter()
        .filter_map(|note| note.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        notes.contains("is disabled"),
        "the check explains why nothing projects the import: {notes}"
    );

    let (code, body) = fixture.run_json(&[
        "--json",
        "update",
        "release-notes",
        "--apply",
        "--strategy",
        "overlay",
    ]);
    assert_eq!(code, 0, "the advertised repair resolves the copy: {body}");
    assert_eq!(
        body["data"]["applied"][0]["skill"], "release-notes",
        "the repair is reported as applied: {body}"
    );
    let overlay = fs::read_to_string(
        fixture
            .workspace
            .path()
            .join(".agents/overlays/release-notes/SKILL.md"),
    )
    .expect("overlay file exists");
    assert!(
        overlay.contains("keep my edit"),
        "the edit survives: {overlay}"
    );
    let manifest = fs::read_to_string(fixture.workspace.path().join(".agents/skillctl.yaml"))
        .expect("manifest exists");
    assert!(
        manifest.contains("enabled: false"),
        "the repair keeps the import disabled: {manifest}"
    );
    assert!(
        !fixture
            .workspace
            .path()
            .join(".claude/skills/release-notes")
            .exists(),
        "the leftover copy of the disabled import is gone"
    );

    // Enabling the import again projects the preserved edit, which closes the
    // loop the repair advice opened.
    let (code, body) = fixture.run_json(&["--json", "enable", "release-notes"]);
    assert_eq!(code, 0, "enable succeeds: {body}");
    let (code, body) = fixture.run_json(&["--json", "sync"]);
    assert_eq!(code, 0, "sync projects the overlay content: {body}");
    assert!(
        fixture.generated_copy().contains("keep my edit"),
        "the edit the repair preserved is projected again"
    );
}

/// The same repair must resolve the user-scope copy of a disabled import,
/// because the documented user roots are a different root set.
#[test]
fn an_advertised_repair_resolves_the_user_scope_copy_a_disable_left_behind() {
    let workspace = TestWorkspace::new();
    workspace.write_manifest(CLAUDE_MANIFEST);
    workspace.write_skill_source("git-source", "release-notes");
    workspace.init_git_repo("git-source");
    let run = |args: &[&str]| {
        let output = Command::cargo_bin("skillctl")
            .expect("binary exists")
            .current_dir(workspace.path())
            .env("HOME", workspace.home_path())
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
    };

    let (code, body) = run(&[
        "--json",
        "--no-input",
        "--scope",
        "user",
        "--name",
        "release-notes",
        "install",
        &workspace.git_repo_url("git-source"),
    ]);
    assert_eq!(code, 0, "the user-scope install succeeds: {body}");

    let user_copy = workspace.home_path().join(".claude/skills/release-notes");
    let mut contents =
        fs::read_to_string(user_copy.join("SKILL.md")).expect("generated copy exists");
    contents.push_str("keep my edit\n");
    fs::write(user_copy.join("SKILL.md"), contents).expect("generated copy edited");

    let (code, body) = run(&["--json", "--scope", "user", "disable", "release-notes"]);
    assert_eq!(code, 2, "disable succeeds with the preserved copy: {body}");
    assert!(
        body["warnings"]
            .as_array()
            .expect("warnings exist")
            .iter()
            .any(|warning| warning
                .as_str()
                .expect("warning is a string")
                .contains("update release-notes --apply --strategy overlay")),
        "disable advertises the update repair: {body}"
    );

    let (code, body) = run(&["--json", "--scope", "user", "update", "release-notes"]);
    assert_eq!(code, 0, "the disabled import still checks: {body}");
    assert_eq!(
        body["data"]["plans"][0]["local_modification_detected"], true,
        "the check reports the user-scope leftover copy: {body}"
    );

    let (code, body) = run(&[
        "--json",
        "--scope",
        "user",
        "update",
        "release-notes",
        "--apply",
        "--strategy",
        "overlay",
    ]);
    assert_eq!(code, 0, "the advertised repair resolves the copy: {body}");
    assert_eq!(
        body["data"]["applied"][0]["scope"], "user",
        "the repair is reported as applied: {body}"
    );
    let overlay = fs::read_to_string(
        workspace
            .path()
            .join(".agents/overlays/release-notes/SKILL.md"),
    )
    .expect("overlay file exists");
    assert!(
        overlay.contains("keep my edit"),
        "the edit survives: {overlay}"
    );
    assert!(
        !user_copy.exists(),
        "the user-scope leftover copy of the disabled import is gone"
    );
}

/// The same repair must resolve a symlink projection, whose entries are links
/// the drift guard hashes by target.
#[test]
fn an_advertised_repair_resolves_a_symlink_projection_a_disable_left_behind() {
    let fixture = SymlinkProjectionFixture::install();
    fixture.replace_projected_link_with_an_edited_file("keep my edit");

    let (code, body) = fixture.run_json(&["--json", "disable", "release-notes"]);
    assert_eq!(code, 2, "disable succeeds with the preserved copy: {body}");
    assert!(
        body["warnings"]
            .as_array()
            .expect("warnings exist")
            .iter()
            .any(|warning| warning
                .as_str()
                .expect("warning is a string")
                .contains("update release-notes --apply --strategy overlay")),
        "disable advertises the update repair: {body}"
    );

    let (code, body) = fixture.run_json(&[
        "--json",
        "update",
        "release-notes",
        "--apply",
        "--strategy",
        "overlay",
    ]);
    assert_eq!(code, 0, "the advertised repair resolves the copy: {body}");
    assert_eq!(
        body["data"]["applied"][0]["skill"], "release-notes",
        "the repair is reported as applied: {body}"
    );
    let overlay = fs::read_to_string(
        fixture
            .workspace
            .path()
            .join(".agents/overlays/release-notes/SKILL.md"),
    )
    .expect("overlay file exists");
    assert!(
        overlay.contains("keep my edit"),
        "the edit survives: {overlay}"
    );
    assert!(
        !fixture.projection_dir().exists(),
        "the leftover symlink projection of the disabled import is gone"
    );
}

/// A symlink projection that carries no recorded digest is judged against the
/// digest of its links. Judging it against the content digest instead reports
/// drift the repair engine cannot resolve, because the engine reads the digest
/// a copy records and finds none.
#[test]
fn update_keeps_a_digestless_symlink_projection_clean() {
    let fixture = SymlinkProjectionFixture::install();
    fixture.drop_recorded_digest();

    let (code, body) = fixture.run_json(&["--json", "update", "release-notes"]);

    assert_eq!(
        code, 0,
        "a check that finds nothing reports success: {body}"
    );
    let plan = &body["data"]["plans"][0];
    assert_eq!(plan["outcome"], "up-to-date");
    assert_eq!(
        plan["local_modification_detected"], false,
        "links that match the plan are not drift: {plan}"
    );
    assert_eq!(
        plan["modifications"],
        json!([]),
        "no repair is advertised for a copy the engine would skip: {plan}"
    );
}
