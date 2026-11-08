//! Trust-gate enforcement for update applies, pins, and rollbacks.
//!
//! The update check announces the gate (`trust.blocked_actions`), and the
//! guarded apply engine enforces it, so an apply that the gate blocks must
//! refuse with exit code `4` and leave installed content untouched. `pin` and
//! `rollback` move an import to a revision the same engine installs, so they
//! consult the same gate and refuse the same moves.

use super::*;

const CLAUDE_MANIFEST: &str = concat!("version: 1\n", "\n", "targets:\n", "  - claude-code\n",);

const BLOCKED_MESSAGE: &str = "trust gate is blocking update apply for 'delta'";
const BLOCKED_PIN_MESSAGE: &str = "trust gate is blocking pin for 'delta'";
const BLOCKED_ROLLBACK_MESSAGE: &str = "trust gate is blocking rollback for 'delta'";

fn skill_manifest(skill_name: &str, description: &str, body: &str) -> String {
    format!(
        concat!(
            "---\n",
            "name: {skill_name}\n",
            "description: {description}\n",
            "---\n",
            "\n",
            "# {skill_name}\n",
            "\n",
            "{body}\n"
        ),
        skill_name = skill_name,
        description = description,
        body = body,
    )
}

/// One script-bearing skill installed from a Git source, with a newer upstream
/// revision waiting. The import stays `imported-unreviewed` with elevated risk,
/// which is the state the trust gate exists to hold back.
struct TrustGateFixture {
    workspace: TestWorkspace,
    installed_revision: String,
}

impl TrustGateFixture {
    fn install() -> Self {
        let workspace = TestWorkspace::new();
        workspace.write_manifest(CLAUDE_MANIFEST);
        workspace.write_file(
            "git-source/.agents/skills/delta/SKILL.md",
            &skill_manifest("delta", "Helper.", "Original body."),
        );
        workspace.write_file(
            "git-source/.agents/skills/delta/scripts/run.sh",
            "#!/bin/sh\necho original\n",
        );
        workspace.init_git_repo("git-source");
        let repo_url = workspace.git_repo_url("git-source");

        // The import carries `scripts/`, so the run reports the unreviewed
        // script warning and exits with `2` instead of `0`.
        Command::cargo_bin("skillctl")
            .expect("binary exists")
            .current_dir(workspace.path())
            .env("HOME", workspace.home_path())
            .args([
                "--no-input",
                "--name",
                "delta",
                "install",
                repo_url.as_str(),
            ])
            .assert()
            .code(2)
            .stderr(predicate::str::is_empty());

        let installed_revision = workspace.git_head("git-source");
        Self {
            workspace,
            installed_revision,
        }
    }

    /// Publish a newer upstream revision that the apply would move to.
    fn advance_upstream(&self) {
        let skill_root = self
            .workspace
            .path()
            .join("git-source/.agents/skills/delta");
        fs::write(
            skill_root.join("SKILL.md"),
            skill_manifest("delta", "Helper.", "Upstream body."),
        )
        .expect("upstream skill manifest written");
        fs::write(
            skill_root.join("scripts/run.sh"),
            "#!/bin/sh\necho upstream\n",
        )
        .expect("upstream script written");
        self.workspace.commit_all("git-source", "advance upstream");
    }

    /// The revision a pin or an apply would move the import to.
    fn upstream_revision(&self) -> String {
        self.workspace.git_head("git-source")
    }

    /// Review the installed content, then apply the waiting update.
    ///
    /// A review covers one effective version, so the applied revision leaves
    /// the import `imported-unreviewed` again, which is the state an explicit
    /// revision move has to cross.
    fn review_and_apply_upstream(&self) {
        let (code, body) = self.run_json(&["review", "delta", "--approve"]);
        assert_eq!(code, 0, "the approval records: {body}");

        let (code, body) = self.run_json(&["update", "delta", "--apply"]);
        assert_eq!(code, 0, "the reviewed update applies: {body}");
        assert_eq!(
            self.workspace.read_lockfile_yaml()["imports"]["delta"]["revision"]["resolved"],
            self.upstream_revision().as_str(),
            "the apply moved the import upstream: {body}",
        );
    }

    /// Edit one generated copy without recording the change, which is the
    /// unmanaged edit a repair strategy exists to resolve.
    fn edit_projected_copy(&self) {
        fs::write(
            self.workspace.path().join(".claude/skills/delta/SKILL.md"),
            skill_manifest("delta", "Directly edited runtime copy.", "My local edit."),
        )
        .expect("projected copy edited");
    }

    /// Paths an apply would have to change for the gate to matter.
    fn apply_state(&self) -> BTreeMap<String, PathSnapshot> {
        self.workspace.snapshot_paths(&[
            ".agents/skillctl.yaml",
            ".agents/skillctl.lock",
            ".claude/skills",
            "home/.skillctl/store/imports",
            "home/.skillctl/store/artifacts",
        ])
    }

    /// Human output of one command, printed to stdout.
    fn run_human(&self, args: &[&str]) -> (i32, String) {
        let output = Command::cargo_bin("skillctl")
            .expect("binary exists")
            .current_dir(self.workspace.path())
            .env("HOME", self.workspace.home_path())
            .args(args)
            .output()
            .expect("command launches");
        assert!(
            output.stderr.is_empty(),
            "unexpected stderr: {}",
            String::from_utf8_lossy(&output.stderr),
        );
        (
            output.status.code().expect("process exits with a code"),
            String::from_utf8_lossy(&output.stdout).into_owned(),
        )
    }

    /// Projection-drift issues of one `doctor` run.
    fn doctor_drift_issues(&self) -> Vec<Value> {
        let (code, body) = self.run_json(&["doctor"]);
        assert_eq!(code, 2, "doctor reports the drift as a warning: {body}");
        let issues = body["data"]["issues"]
            .as_array()
            .expect("issues exist")
            .iter()
            .filter(|issue| issue["code"] == "projection-drift")
            .cloned()
            .collect::<Vec<_>>();
        assert!(
            !issues.is_empty(),
            "the edited copy is reported as drift: {body}",
        );
        issues
    }

    /// Whether `doctor` still reports the projected copy as drift.
    fn doctor_reports_drift(&self) -> bool {
        let (code, body) = self.run_json(&["doctor"]);
        // The unreviewed script keeps the run at a warning even once the copy
        // is clean.
        assert_eq!(code, 2, "doctor keeps reporting the script risk: {body}");
        body["data"]["issues"]
            .as_array()
            .expect("issues exist")
            .iter()
            .any(|issue| issue["code"] == "projection-drift")
    }

    /// Preserved-copy entries of one command report.
    fn preserved_entries(&self, command: &str) -> Vec<Value> {
        let (code, body) = self.run_json(&[command]);
        assert_eq!(code, 2, "{command} reports the preserved copy: {body}");
        let preserved = body["data"]["preserved"]
            .as_array()
            .expect("preserved entries exist")
            .clone();
        assert!(!preserved.is_empty(), "{command} reports the copy: {body}");
        preserved
    }

    fn run_json(&self, args: &[&str]) -> (i32, Value) {
        let output = Command::cargo_bin("skillctl")
            .expect("binary exists")
            .current_dir(self.workspace.path())
            .env("HOME", self.workspace.home_path())
            .args(["--json"])
            .args(args)
            .output()
            .expect("command launches");
        let body = serde_json::from_slice(&output.stdout).expect("stdout is valid json");
        (
            output.status.code().expect("process exits with a code"),
            body,
        )
    }

    fn projected_skill(&self) -> String {
        fs::read_to_string(self.workspace.path().join(".claude/skills/delta/SKILL.md"))
            .expect("projected skill exists")
    }

    /// History kinds recorded for `delta`.
    fn history_kinds(&self) -> Vec<String> {
        let connection = Connection::open(self.workspace.home_path().join(".skillctl/state.db"))
            .expect("state store opens");
        connection
            .prepare(
                "SELECT kind FROM history_events WHERE skill_id = ?1 ORDER BY occurred_at ASC, \
                 id ASC",
            )
            .expect("history query prepares")
            .query_map(params!["delta"], |row| row.get(0))
            .expect("history query runs")
            .collect::<Result<_, _>>()
            .expect("history rows read")
    }
}

#[test]
fn trust_gate_blocks_an_apply_with_exit_code_4_and_changes_nothing() {
    let fixture = TrustGateFixture::install();
    fixture.advance_upstream();
    let before = fixture.apply_state();

    let (code, body) = fixture.run_json(&["update", "delta", "--apply"]);

    assert_eq!(code, 4, "a blocked apply must use the trust-gate exit code");
    assert_eq!(body["ok"], false);
    assert_eq!(body["command"], "update");
    let error = body["errors"][0].as_str().expect("error message exists");
    assert!(error.contains(BLOCKED_MESSAGE), "unexpected error: {error}");
    assert!(
        error.contains("--strategy fork"),
        "the refusal must name the documented way past the gate: {error}",
    );

    assert_eq!(fixture.apply_state(), before, "no apply state may change");
    assert!(
        fixture.projected_skill().contains("Original body."),
        "the projected copy must keep the pinned revision",
    );
    assert_eq!(
        fixture.workspace.read_lockfile_yaml()["imports"]["delta"]["revision"]["resolved"],
        fixture.installed_revision.as_str(),
        "the lockfile must keep the pinned revision",
    );
    assert!(
        !fixture
            .history_kinds()
            .contains(&"update-applied".to_string()),
        "a refused apply must not record an applied update",
    );
}

#[test]
fn trust_gate_blocks_an_apply_that_targets_every_skill() {
    let fixture = TrustGateFixture::install();
    fixture.advance_upstream();
    let before = fixture.apply_state();

    // No skill selector means "every managed skill", which is also what the
    // MCP `skills_update_apply` tool sends with empty arguments.
    let (code, body) = fixture.run_json(&["update", "--apply"]);

    assert_eq!(code, 4);
    assert_eq!(body["ok"], false);
    assert!(
        body["errors"][0]
            .as_str()
            .expect("error message exists")
            .contains(BLOCKED_MESSAGE)
    );
    assert_eq!(fixture.apply_state(), before);
}

#[test]
fn trust_gate_blocks_the_whole_apply_run_before_any_skill_changes() {
    let workspace = TestWorkspace::new();
    workspace.write_manifest(CLAUDE_MANIFEST);
    workspace.write_file(
        "scripted-source/.agents/skills/delta/SKILL.md",
        &skill_manifest("delta", "Helper.", "Original body."),
    );
    workspace.write_file(
        "scripted-source/.agents/skills/delta/scripts/run.sh",
        "#!/bin/sh\necho original\n",
    );
    workspace.write_file(
        "plain-source/.agents/skills/release-notes/SKILL.md",
        &skill_manifest(
            "release-notes",
            "Summarize release notes.",
            "Original body.",
        ),
    );
    for source in ["scripted-source", "plain-source"] {
        workspace.init_git_repo(source);
    }

    for (source, skill) in [
        ("scripted-source", "delta"),
        ("plain-source", "release-notes"),
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
            .output()
            .expect("command launches");
    }
    for (source, skill) in [
        ("scripted-source", "delta"),
        ("plain-source", "release-notes"),
    ] {
        let skill_root = workspace
            .path()
            .join(source)
            .join(".agents/skills")
            .join(skill);
        fs::write(
            skill_root.join("SKILL.md"),
            skill_manifest(skill, "Helper.", "Upstream body."),
        )
        .expect("upstream skill manifest written");
        workspace.commit_all(source, "advance upstream");
    }
    let before = workspace.snapshot_paths(&[".agents/skillctl.lock", ".claude/skills"]);

    // One blocked skill refuses the whole run, so an unblocked skill in the
    // same apply cannot move either.
    let output = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--json", "update", "--apply"])
        .output()
        .expect("command launches");

    assert_eq!(output.status.code(), Some(4));
    let body: Value = serde_json::from_slice(&output.stdout).expect("json body parses");
    assert!(
        body["errors"][0]
            .as_str()
            .expect("error message exists")
            .contains(BLOCKED_MESSAGE)
    );
    assert_eq!(
        workspace.snapshot_paths(&[".agents/skillctl.lock", ".claude/skills"]),
        before,
        "the unblocked skill in the same run must not move either",
    );
}

#[test]
fn trust_gate_blocks_every_strategy_that_moves_the_revision() {
    for strategy in ["overlay", "discard"] {
        let fixture = TrustGateFixture::install();
        fixture.advance_upstream();
        let before = fixture.apply_state();

        let (code, body) =
            fixture.run_json(&["update", "delta", "--apply", "--strategy", strategy]);

        assert_eq!(code, 4, "the {strategy} strategy still applies upstream");
        assert_eq!(body["ok"], false);
        assert_eq!(fixture.apply_state(), before, "strategy {strategy}");
    }
}

#[test]
fn trust_gate_announces_the_block_when_edits_hold_back_the_update() {
    let fixture = TrustGateFixture::install();
    fixture.edit_projected_copy();
    fixture.advance_upstream();

    let (code, body) = fixture.run_json(&["update", "delta"]);

    // A check with warnings is not a failure, but the gate must be visible in
    // the plan exactly as it is for a clean import.
    assert_eq!(code, 2);
    assert_eq!(body["ok"], true);
    assert!(
        body["warnings"]
            .as_array()
            .expect("warnings exist")
            .iter()
            .any(|warning| warning.as_str().unwrap_or("").contains(BLOCKED_MESSAGE)),
        "unexpected warnings: {}",
        body["warnings"],
    );

    let plan = &body["data"]["plans"][0];
    assert_eq!(plan["outcome"], "blocked");
    assert_eq!(plan["local_modification_detected"], true);
    assert_eq!(plan["trust"]["risk_level"], "elevated");
    assert_eq!(plan["trust"]["blocked_actions"], json!(["apply-update"]));
    assert_eq!(plan["recommended_action"], "skip");
    assert_eq!(
        plan["available_actions"],
        json!(["detach", "publish-variant", "skip"]),
        "a gated import keeps only the actions that do not move the revision: {plan}",
    );

    // The local ledger is the system of record, so the gate decision must
    // reach the recorded update check too.
    let connection = Connection::open(fixture.workspace.home_path().join(".skillctl/state.db"))
        .expect("state store opens");
    let details_json: String = connection
        .query_row(
            "SELECT details_json FROM history_events \
             WHERE skill_id = ?1 AND kind = 'update-check' ORDER BY id DESC LIMIT 1",
            params!["delta"],
            |row| row.get(0),
        )
        .expect("update-check history exists");
    let details: Value = serde_json::from_str(&details_json).expect("details json is valid");
    assert_eq!(details["trust"]["risk_level"], "elevated");
    assert_eq!(details["trust"]["blocked_actions"], json!(["apply-update"]));
}

#[test]
fn trust_gate_blocks_an_apply_that_repairs_edited_copies() {
    for strategy in ["overlay", "discard"] {
        let fixture = TrustGateFixture::install();
        fixture.edit_projected_copy();
        fixture.advance_upstream();
        let before = fixture.apply_state();

        let (code, body) =
            fixture.run_json(&["update", "delta", "--apply", "--strategy", strategy]);

        assert_eq!(
            code, 4,
            "the {strategy} repair still moves the import to the new revision",
        );
        assert_eq!(body["ok"], false);
        assert!(
            body["errors"][0]
                .as_str()
                .expect("error message exists")
                .contains(BLOCKED_MESSAGE),
            "unexpected errors for {strategy}: {}",
            body["errors"],
        );
        assert_eq!(fixture.apply_state(), before, "strategy {strategy}");
        assert!(
            fixture.projected_skill().contains("My local edit."),
            "the {strategy} strategy must leave the unmanaged edit in place",
        );
        assert_eq!(
            fixture.workspace.read_lockfile_yaml()["imports"]["delta"]["revision"]["resolved"],
            fixture.installed_revision.as_str(),
            "the {strategy} strategy must keep the pinned revision",
        );
        assert!(
            !fixture
                .history_kinds()
                .contains(&"update-applied".to_string()),
            "a refused {strategy} apply must not record an applied update",
        );
    }
}

#[test]
fn trust_gate_keeps_the_fork_escape_hatch_for_edited_copies() {
    let fixture = TrustGateFixture::install();
    fixture.edit_projected_copy();
    fixture.advance_upstream();

    let (code, body) = fixture.run_json(&["update", "delta", "--apply", "--strategy", "fork"]);

    assert_ne!(code, 4, "a fork is the documented way past the gate");
    assert_eq!(body["ok"], true);
    let canonical = fixture
        .workspace
        .path()
        .join(".agents/skills/delta/SKILL.md");
    assert!(
        fs::read_to_string(&canonical)
            .expect("the forked skill exists")
            .contains("My local edit."),
        "a fork must carry the unmanaged edit into local ownership",
    );
    assert!(
        fixture.workspace.read_lockfile_yaml()["imports"]
            .get("delta")
            .is_none(),
        "a fork removes the managed import",
    );
}

/// The gate narrows the drift advice to the fork and the discard, and an
/// overlay the lockfile no longer pins must not hide the copy edit from that
/// advice. Both commands it names still resolve the copy.
#[test]
fn gated_advice_still_names_repairs_while_the_overlay_changed_since_the_pin() {
    let fixture = TrustGateFixture::install();
    fixture.run_json(&["override", "delta"]);
    let overlay = fixture
        .workspace
        .path()
        .join(".agents/overlays/delta/SKILL.md");
    let mut contents = fs::read_to_string(&overlay).expect("overlay manifest exists");
    contents.push_str("\noverlay note\n");
    fs::write(&overlay, contents).expect("overlay edited without a re-pin");
    fixture.edit_projected_copy();
    fixture.advance_upstream();

    let issues = fixture.doctor_drift_issues();
    let fix = issues[0]["fix"]
        .as_str()
        .expect("the drift issue names a repair");
    assert!(
        fix.contains("update delta --apply --strategy fork")
            && fix.contains("skillctl sync --discard"),
        "the gated advice names the fork and the discard: {fix}"
    );

    let (code, body) = fixture.run_json(&["update", "delta", "--apply", "--strategy", "fork"]);
    assert_ne!(
        code, 4,
        "the advertised fork is the way past the gate: {body}"
    );
    assert_eq!(
        body["ok"], true,
        "the advertised fork resolves the copy: {body}"
    );
    let canonical = fs::read_to_string(
        fixture
            .workspace
            .path()
            .join(".agents/skills/delta/SKILL.md"),
    )
    .expect("the forked skill exists");
    assert!(
        canonical.contains("My local edit."),
        "the fork carries the unmanaged edit into local ownership: {canonical}"
    );
}

#[test]
fn trust_gate_blocks_a_plan_document_apply_for_edited_copies() {
    let fixture = TrustGateFixture::install();
    fixture.edit_projected_copy();
    fixture.advance_upstream();

    // A blocked check only contributes a target to a repair strategy, so the
    // plan is written for the strategy that would resolve the edits.
    let (code, body) = fixture.run_json(&[
        "update",
        "delta",
        "--plan",
        "update-plan.json",
        "--strategy",
        "overlay",
    ]);
    assert_eq!(code, 2, "writing a plan is a warning-level check");
    assert!(
        body["warnings"]
            .as_array()
            .expect("warnings exist")
            .iter()
            .any(|warning| warning.as_str().unwrap_or("").contains(BLOCKED_MESSAGE)),
        "the plan document response must announce the gate: {}",
        body["warnings"],
    );

    let before = fixture.apply_state();
    let (code, body) = fixture.run_json(&[
        "update",
        "delta",
        "--apply",
        "--plan-file",
        "update-plan.json",
        "--strategy",
        "overlay",
    ]);

    assert_eq!(code, 4, "a plan document may not bypass the gate");
    assert_eq!(body["ok"], false);
    assert!(
        body["errors"][0]
            .as_str()
            .expect("error message exists")
            .contains(BLOCKED_MESSAGE),
    );
    assert_eq!(fixture.apply_state(), before);
    assert!(
        fixture.projected_skill().contains("My local edit."),
        "the refused plan apply must leave the unmanaged edit in place",
    );
}

#[test]
fn a_blocked_skill_contributes_to_a_plan_document_under_the_clean_strategy() {
    let fixture = TrustGateFixture::install();
    fixture.edit_projected_copy();
    fixture.advance_upstream();
    let before = fixture.apply_state();

    // The default clean strategy records the revision move too, so the document
    // is never empty, and the warning names the fork because the gate refuses
    // the other repair strategies.
    let (code, body) = fixture.run_json(&["update", "delta", "--plan", "update-plan.json"]);
    assert_eq!(code, 2, "a plan with warnings is not a failure: {body}");
    let plan_path = fixture.workspace.path().join("update-plan.json");
    let document: Value =
        serde_json::from_str(&fs::read_to_string(&plan_path).expect("plan document exists"))
            .expect("plan document is valid json");
    assert_eq!(
        document["plans"].as_array().map(Vec::len),
        Some(1),
        "the blocked skill contributes a plan: {document}",
    );
    assert_eq!(document["strategy"], "clean");

    let warnings = body["warnings"].as_array().expect("warnings exist");
    assert!(
        warnings.iter().any(|warning| warning
            .as_str()
            .unwrap_or_default()
            .contains(BLOCKED_MESSAGE)),
        "the gate decision travels with the document: {warnings:?}",
    );
    let strategy_warning = warnings
        .iter()
        .find(|warning| {
            warning
                .as_str()
                .unwrap_or_default()
                .contains("cannot resolve")
        })
        .expect("the strategy warning exists");
    assert_gated_repair(
        strategy_warning.as_str().unwrap_or_default(),
        "the plan warning",
    );

    // The later apply of that document is refused by the gate, so the flow can
    // never report a silent no-op.
    let (code, body) = fixture.run_json(&[
        "update",
        "delta",
        "--apply",
        "--plan-file",
        "update-plan.json",
    ]);
    assert_eq!(code, 4, "a plan document may not bypass the gate");
    assert_eq!(body["ok"], false);
    assert_eq!(fixture.apply_state(), before);
    assert!(
        fixture.projected_skill().contains("My local edit."),
        "the refused plan apply leaves the unmanaged edit in place",
    );
}

#[test]
fn a_clean_apply_for_a_gated_blocked_skill_is_refused_by_the_gate() {
    let fixture = TrustGateFixture::install();
    fixture.edit_projected_copy();
    fixture.advance_upstream();
    let before = fixture.apply_state();

    let (code, body) = fixture.run_json(&["update", "delta", "--apply"]);

    assert_eq!(
        code, 4,
        "the gate covers every strategy that moves the revision, clean included",
    );
    assert_eq!(body["ok"], false);
    assert!(
        body["errors"][0]
            .as_str()
            .expect("error message exists")
            .contains(BLOCKED_MESSAGE),
    );
    assert_eq!(fixture.apply_state(), before);
}

#[test]
fn trust_gate_blocks_an_apply_from_a_plan_document() {
    let fixture = TrustGateFixture::install();
    fixture.advance_upstream();

    let (code, body) = fixture.run_json(&["update", "delta", "--plan", "update-plan.json"]);
    assert_eq!(code, 2, "writing a plan is a warning-level check");
    assert!(
        body["warnings"]
            .as_array()
            .expect("warnings exist")
            .iter()
            .any(|warning| warning.as_str().unwrap_or("").contains(BLOCKED_MESSAGE)),
        "the plan document response must announce the gate: {}",
        body["warnings"],
    );

    let before = fixture.apply_state();
    let (code, body) = fixture.run_json(&[
        "update",
        "delta",
        "--apply",
        "--plan-file",
        "update-plan.json",
    ]);

    assert_eq!(code, 4, "a plan document may not bypass the gate");
    assert_eq!(body["ok"], false);
    assert!(
        body["errors"][0]
            .as_str()
            .expect("error message exists")
            .contains(BLOCKED_MESSAGE),
    );
    assert_eq!(fixture.apply_state(), before);
}

#[test]
fn a_stale_plan_document_apply_reports_the_staleness_before_the_gate() {
    let fixture = TrustGateFixture::install();
    fixture.advance_upstream();

    // One review licenses exactly one move: it covers the effective version the
    // import holds, so the apply that follows it returns the import to
    // `imported-unreviewed` on the new content.
    let (code, body) = fixture.run_json(&["review", "delta", "--approve"]);
    assert_eq!(code, 0, "the approval records: {body}");

    let (code, body) = fixture.run_json(&["update", "delta", "--plan", "update-plan.json"]);
    assert_eq!(code, 0, "a reviewed import plans without warnings: {body}");

    let (code, body) = fixture.run_json(&[
        "update",
        "delta",
        "--apply",
        "--plan-file",
        "update-plan.json",
    ]);
    assert_eq!(code, 0, "the reviewed document applies: {body}");
    assert_eq!(
        fixture.workspace.read_lockfile_yaml()["imports"]["delta"]["revision"]["resolved"],
        fixture.upstream_revision().as_str(),
        "the first apply moved the import upstream: {body}",
    );

    // The precondition the regression needs: the import is gated again, and the
    // document proposes the revision the install already holds.
    let (code, body) = fixture.run_json(&["doctor"]);
    assert_eq!(code, 2, "the arrived scripts are a warning again: {body}");
    assert!(
        body["data"]["issues"]
            .as_array()
            .expect("issues exist")
            .iter()
            .any(|issue| issue["code"] == "script-risk"),
        "the import returned to imported-unreviewed: {body}",
    );

    let before = fixture.apply_state();
    let (code, body) = fixture.run_json(&[
        "update",
        "delta",
        "--apply",
        "--plan-file",
        "update-plan.json",
    ]);

    // No review can make an outdated document applicable, so the staleness must
    // refuse before the gate is consulted.
    assert_eq!(
        code, 3,
        "a stale plan document is a validation failure, not a gate refusal: {body}",
    );
    assert_eq!(body["ok"], false);
    let error = body["errors"][0].as_str().expect("error message exists");
    assert!(
        error.contains("is stale")
            && error.contains("imports.delta.revision.resolved")
            && error.contains("but the plan was built against"),
        "the refusal reports what changed since planning: {error}",
    );
    assert!(
        error.contains("rebuild the plan with 'skillctl update --plan'"),
        "the refusal must name the repair that succeeds: {error}",
    );
    assert!(
        !error.contains(BLOCKED_MESSAGE),
        "the gate must not answer for a document no review can unblock: {error}",
    );
    assert!(
        !error.contains("skillctl review delta --approve"),
        "the refusal must not name a review that cannot unblock it: {error}",
    );
    assert_eq!(fixture.apply_state(), before);
}

#[test]
fn trust_gate_allows_the_fork_escape_hatch() {
    let fixture = TrustGateFixture::install();
    fixture.advance_upstream();

    let (code, body) = fixture.run_json(&["update", "delta", "--apply", "--strategy", "fork"]);

    assert_ne!(code, 4, "a fork is the documented way past the gate");
    assert_eq!(body["ok"], true);
    assert_eq!(body["data"]["applied"][0]["strategy"], "fork");

    let canonical = fixture
        .workspace
        .path()
        .join(".agents/skills/delta/SKILL.md");
    assert!(
        canonical.is_file(),
        "a fork must take local ownership instead of refusing",
    );
    assert!(
        fixture.workspace.read_lockfile_yaml()["imports"]
            .get("delta")
            .is_none(),
        "a fork removes the managed import",
    );
}

#[test]
fn trust_gate_allows_the_skip_strategy() {
    let fixture = TrustGateFixture::install();
    fixture.advance_upstream();
    let before = fixture.apply_state();

    let (code, body) = fixture.run_json(&["update", "delta", "--apply", "--strategy", "skip"]);

    assert_ne!(code, 4, "a skip keeps the pin and applies nothing");
    assert_eq!(body["ok"], true);
    assert_eq!(body["data"]["applied"].as_array().map(Vec::len), Some(0));
    assert_eq!(
        body["data"]["apply_skipped"][0]["reason"],
        "apply strategy was skip",
    );
    assert_eq!(fixture.apply_state(), before);
}

#[test]
fn update_check_still_announces_the_gate_as_a_warning() {
    let fixture = TrustGateFixture::install();
    fixture.advance_upstream();

    let (code, body) = fixture.run_json(&["update", "delta"]);

    assert_eq!(code, 2, "a check with warnings is not a failure");
    assert_eq!(body["ok"], true);
    assert!(
        body["warnings"]
            .as_array()
            .expect("warnings exist")
            .iter()
            .any(|warning| warning.as_str().unwrap_or("").contains(BLOCKED_MESSAGE)),
    );

    let plan = &body["data"]["plans"][0];
    assert_eq!(plan["recommended_action"], "skip");
    assert_eq!(plan["trust"]["blocked_actions"], json!(["apply-update"]));
    assert_eq!(plan["trust"]["risk_level"], "elevated");
}

#[test]
fn an_import_without_scripts_is_not_gated() {
    let workspace = TestWorkspace::new();
    workspace.write_manifest(CLAUDE_MANIFEST);
    workspace.write_file(
        "git-source/.agents/skills/release-notes/SKILL.md",
        &skill_manifest(
            "release-notes",
            "Summarize release notes.",
            "Original body.",
        ),
    );
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

    let skill_root = workspace
        .path()
        .join("git-source/.agents/skills/release-notes");
    fs::write(
        skill_root.join("SKILL.md"),
        skill_manifest(
            "release-notes",
            "Summarize release notes.",
            "Upstream body.",
        ),
    )
    .expect("upstream skill manifest written");
    workspace.commit_all("git-source", "advance upstream");

    let output = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--json", "update", "release-notes", "--apply"])
        .output()
        .expect("command launches");

    assert_eq!(
        output.status.code(),
        Some(0),
        "an import without scripts is not gated: {}",
        String::from_utf8_lossy(&output.stdout),
    );
    let body: Value = serde_json::from_slice(&output.stdout).expect("json body parses");
    assert_eq!(body["data"]["applied"][0]["skill"], "release-notes");
}

/// One script-free skill installed from a Git source, with a newer upstream
/// revision waiting that introduces `scripts/`.
///
/// The installed content is the only content a recorded review can cover, so
/// this is the first arrival of executable content: no check that evaluates
/// the installed import can see it.
struct IncomingScriptsFixture {
    workspace: TestWorkspace,
    installed_revision: String,
    upstream_revision: String,
}

impl IncomingScriptsFixture {
    /// Install `delta` at a script-free revision, then publish a newer upstream
    /// revision that adds a top-level `scripts/` directory.
    fn install_and_advance() -> Self {
        let workspace = TestWorkspace::new();
        workspace.write_manifest(CLAUDE_MANIFEST);
        workspace.write_file(
            "git-source/.agents/skills/delta/SKILL.md",
            &skill_manifest("delta", "Helper.", "Original body."),
        );
        workspace.init_git_repo("git-source");
        let repo_url = workspace.git_repo_url("git-source");

        // A script-free import is a plain success, so the workspace starts
        // with normal risk and no warning.
        Command::cargo_bin("skillctl")
            .expect("binary exists")
            .current_dir(workspace.path())
            .env("HOME", workspace.home_path())
            .args([
                "--no-input",
                "--name",
                "delta",
                "install",
                repo_url.as_str(),
            ])
            .assert()
            .success()
            .stderr(predicate::str::is_empty());

        let installed_revision = workspace.git_head("git-source");

        // `write_file` creates the `scripts/` directory, so the newer revision
        // introduces executable content the installed import never had.
        workspace.write_file(
            "git-source/.agents/skills/delta/SKILL.md",
            &skill_manifest("delta", "Helper.", "Upstream body."),
        );
        workspace.write_file(
            "git-source/.agents/skills/delta/scripts/run.sh",
            "#!/bin/sh\necho upstream\n",
        );
        workspace.commit_all("git-source", "introduce scripts");
        let upstream_revision = workspace.git_head("git-source");

        Self {
            workspace,
            installed_revision,
            upstream_revision,
        }
    }

    fn run_json(&self, args: &[&str]) -> (i32, Value) {
        let output = Command::cargo_bin("skillctl")
            .expect("binary exists")
            .current_dir(self.workspace.path())
            .env("HOME", self.workspace.home_path())
            .args(["--json"])
            .args(args)
            .output()
            .expect("command launches");
        let body = serde_json::from_slice(&output.stdout).expect("stdout is valid json");
        (
            output.status.code().expect("process exits with a code"),
            body,
        )
    }

    /// Edit one generated copy without recording the change, which is the
    /// unmanaged edit a repair strategy exists to resolve.
    fn edit_projected_copy(&self) {
        fs::write(
            self.workspace.path().join(".claude/skills/delta/SKILL.md"),
            skill_manifest("delta", "Directly edited runtime copy.", "My local edit."),
        )
        .expect("projected copy edited");
    }

    /// Projection-drift issues of one `doctor` run.
    fn doctor_drift_issues(&self) -> Vec<Value> {
        let (code, body) = self.run_json(&["doctor"]);
        assert_eq!(code, 2, "doctor reports the drift as a warning: {body}");
        let issues = body["data"]["issues"]
            .as_array()
            .expect("issues exist")
            .iter()
            .filter(|issue| issue["code"] == "projection-drift")
            .cloned()
            .collect::<Vec<_>>();
        assert!(
            !issues.is_empty(),
            "the edited copy is reported as drift: {body}",
        );
        issues
    }

    /// Preserved-copy entries of one command report.
    fn preserved_entries(&self, command: &str) -> Vec<Value> {
        let (code, body) = self.run_json(&[command]);
        assert_eq!(code, 2, "{command} reports the preserved copy: {body}");
        let preserved = body["data"]["preserved"]
            .as_array()
            .expect("preserved entries exist")
            .clone();
        assert!(!preserved.is_empty(), "{command} reports the copy: {body}");
        preserved
    }

    /// The block reason the newest recorded check wrote for `delta`.
    fn recorded_block_reason(&self) -> Option<String> {
        let connection = Connection::open(self.workspace.home_path().join(".skillctl/state.db"))
            .expect("state store opens");
        connection
            .query_row(
                "SELECT apply_blocked_by FROM update_checks \
                 WHERE skill_id = ?1 ORDER BY id DESC LIMIT 1",
                params!["delta"],
                |row| row.get::<_, Option<String>>(0),
            )
            .expect("update check row reads")
    }

    /// Paths an apply would have to change for the gate to matter.
    fn apply_state(&self) -> BTreeMap<String, PathSnapshot> {
        self.workspace.snapshot_paths(&[
            ".agents/skillctl.yaml",
            ".agents/skillctl.lock",
            ".claude/skills",
            "home/.skillctl/store/imports",
            "home/.skillctl/store/artifacts",
        ])
    }

    /// Whether the projected copy holds the upstream script.
    fn projected_script_exists(&self) -> bool {
        self.workspace
            .path()
            .join(".claude/skills/delta/scripts/run.sh")
            .is_file()
    }
}

/// The clause that names the executable content the apply would install.
fn assert_names_incoming_revision(message: &str, origin: &str) {
    assert!(
        message.contains(BLOCKED_MESSAGE),
        "{origin} must name the blocked skill: {message}",
    );
    assert!(
        message.contains("adds a top-level scripts/ directory no recorded review covers"),
        "{origin} must name the content that would arrive: {message}",
    );
}

#[test]
fn an_update_that_introduces_scripts_is_gated_by_the_check() {
    let fixture = IncomingScriptsFixture::install_and_advance();

    let (code, body) = fixture.run_json(&["update", "delta"]);

    // The check reports a warning, not a failure, and the plan says the apply
    // is held back exactly as it does for installed scripts.
    assert_eq!(code, 2, "a check with warnings is not a failure: {body}");
    assert_eq!(body["ok"], true);
    assert!(
        body["warnings"]
            .as_array()
            .expect("warnings exist")
            .iter()
            .any(|warning| warning
                .as_str()
                .unwrap_or_default()
                .contains("adds a top-level scripts/ directory")),
        "the warning names the incoming revision: {}",
        body["warnings"],
    );

    let plan = &body["data"]["plans"][0];
    assert_eq!(plan["outcome"], "update-available");
    assert_eq!(plan["recommended_action"], "skip");
    assert_eq!(plan["available_actions"], json!(["skip"]));
    assert_eq!(plan["trust"]["blocked_actions"], json!(["apply-update"]));
    assert_eq!(
        plan["trust"]["contains_scripts"], false,
        "the installed content stays script-free: {plan}",
    );
    assert_eq!(plan["trust"]["risk_level"], "normal");

    // The local ledger is the system of record, so the decision travels with
    // the recorded check.
    let connection = Connection::open(fixture.workspace.home_path().join(".skillctl/state.db"))
        .expect("state store opens");
    let details_json: String = connection
        .query_row(
            "SELECT details_json FROM history_events \
             WHERE skill_id = ?1 AND kind = 'update-check' ORDER BY id DESC LIMIT 1",
            params!["delta"],
            |row| row.get(0),
        )
        .expect("update-check history exists");
    let details: Value = serde_json::from_str(&details_json).expect("details json is valid");
    assert_eq!(
        details["trust"]["blocked_actions"],
        json!(["apply-update"]),
        "the gate decision reaches the ledger: {details}",
    );
}

#[test]
fn an_update_apply_that_introduces_scripts_exits_four_and_changes_nothing() {
    let fixture = IncomingScriptsFixture::install_and_advance();
    let before = fixture.apply_state();

    let (code, body) = fixture.run_json(&["update", "delta", "--apply"]);

    assert_eq!(
        code, 4,
        "the first arrival of executable content must meet the gate: {body}",
    );
    assert_eq!(body["ok"], false);
    let error = body["errors"][0].as_str().expect("error message exists");
    assert_names_incoming_revision(error, "the apply refusal");
    assert!(
        error.contains(&fixture.upstream_revision),
        "the refusal names the revision that adds scripts: {error}",
    );
    assert!(
        error.contains("'skillctl review delta --approve'"),
        "the refusal names the review that licenses the move: {error}",
    );
    assert!(
        error.contains("--strategy fork"),
        "the refusal keeps naming the fork the gate allows: {error}",
    );

    assert_eq!(fixture.apply_state(), before, "no apply state may change");
    assert!(
        !fixture.projected_script_exists(),
        "unreviewed executable content must not reach a projection",
    );
    assert_eq!(
        fixture.workspace.read_lockfile_yaml()["imports"]["delta"]["revision"]["resolved"],
        fixture.installed_revision.as_str(),
        "the lockfile must keep the pinned revision",
    );
}

#[test]
fn an_apply_that_introduces_scripts_is_refused_for_every_moving_strategy() {
    for strategy in ["overlay", "discard"] {
        let fixture = IncomingScriptsFixture::install_and_advance();
        let before = fixture.apply_state();

        let (code, body) =
            fixture.run_json(&["update", "delta", "--apply", "--strategy", strategy]);

        assert_eq!(code, 4, "the {strategy} strategy still applies upstream");
        assert_eq!(body["ok"], false);
        assert_names_incoming_revision(
            body["errors"][0].as_str().expect("error message exists"),
            "the refusal",
        );
        assert_eq!(fixture.apply_state(), before, "strategy {strategy}");
        assert!(!fixture.projected_script_exists(), "strategy {strategy}");
    }
}

#[test]
fn a_plan_document_may_not_install_unreviewed_scripts() {
    let fixture = IncomingScriptsFixture::install_and_advance();

    let (code, body) = fixture.run_json(&["update", "delta", "--plan", "update-plan.json"]);
    assert_eq!(code, 2, "writing a plan is a warning-level check");
    assert!(
        body["warnings"]
            .as_array()
            .expect("warnings exist")
            .iter()
            .any(|warning| warning
                .as_str()
                .unwrap_or_default()
                .contains("adds a top-level scripts/ directory")),
        "the plan document response announces the gate: {}",
        body["warnings"],
    );

    let before = fixture.apply_state();
    let (code, body) = fixture.run_json(&[
        "update",
        "delta",
        "--apply",
        "--plan-file",
        "update-plan.json",
    ]);

    // A plan document carries no trust decision, so the refusal comes from the
    // engine, which derives the gate from the tree it is about to install.
    assert_eq!(code, 4, "a plan document may not bypass the gate");
    assert_eq!(body["ok"], false);
    assert_names_incoming_revision(
        body["errors"][0].as_str().expect("error message exists"),
        "the plan document refusal",
    );
    assert_eq!(fixture.apply_state(), before);
    assert!(!fixture.projected_script_exists());
}

#[test]
fn a_recorded_review_licenses_a_script_introducing_apply() {
    let fixture = IncomingScriptsFixture::install_and_advance();

    let (code, body) = fixture.run_json(&["review", "delta", "--approve"]);
    assert_eq!(code, 0, "the approval records: {body}");

    let (code, body) = fixture.run_json(&["update", "delta", "--apply"]);

    assert_eq!(
        code, 0,
        "a recorded approval is the documented way past the gate: {body}",
    );
    assert_eq!(body["data"]["applied"][0]["skill"], "delta");
    assert_eq!(
        fixture.workspace.read_lockfile_yaml()["imports"]["delta"]["revision"]["resolved"],
        fixture.upstream_revision.as_str(),
        "the apply moves the pin to the revision that adds scripts",
    );
    assert!(
        fixture.projected_script_exists(),
        "the reviewed revision projects its scripts",
    );

    // The approval covered the script-free content, so the import is
    // unreviewed again and `doctor` reports the scripts that arrived.
    let (code, body) = fixture.run_json(&["doctor"]);
    assert_eq!(code, 2, "the new scripts are a warning: {body}");
    assert!(
        body["data"]["issues"]
            .as_array()
            .expect("issues exist")
            .iter()
            .any(|issue| issue["code"] == "script-risk"),
        "doctor reports the executable content that arrived: {body}",
    );
}

#[test]
fn the_fork_and_skip_strategies_stay_open_when_the_incoming_revision_adds_scripts() {
    for strategy in ["fork", "skip"] {
        let fixture = IncomingScriptsFixture::install_and_advance();

        let (code, body) =
            fixture.run_json(&["update", "delta", "--apply", "--strategy", strategy]);

        assert_ne!(
            code, 4,
            "the {strategy} strategy is the documented way past the gate: {body}",
        );
        assert_eq!(body["ok"], true, "strategy {strategy}: {body}");
        assert!(
            !fixture.projected_script_exists(),
            "the {strategy} strategy must not install the scripts",
        );

        // A skip keeps the managed import on its pin, and a fork moves the
        // skill into local ownership, so the pinned revision survives either
        // way and no projection gains the unreviewed script.
        let projected = fixture
            .workspace
            .path()
            .join(".claude/skills/delta/SKILL.md");
        assert!(
            fs::read_to_string(&projected)
                .expect("the projected skill exists")
                .contains("Original body."),
            "the {strategy} strategy keeps the installed content",
        );
    }
}

/// The advice for a gated import must not name an update strategy the gate
/// refuses, so every fix names the fork and the explicit discard instead.
fn assert_gated_repair(advice: &str, origin: &str) {
    assert!(
        advice.contains("update delta --apply --strategy fork"),
        "{origin} must name the fork the gate allows: {advice}",
    );
    assert!(
        advice.contains("skillctl sync --discard"),
        "{origin} must name the discard that regenerates the copy: {advice}",
    );
    assert!(
        !advice.contains("--strategy overlay") && !advice.contains("--strategy discard"),
        "{origin} must not name strategies the gate refuses: {advice}",
    );
}

/// The advice for a gated import whose repair keeps the pinned revision names
/// the update strategies, because the gate holds back only applies that move
/// the import upstream.
fn assert_ungated_repair(advice: &str, origin: &str) {
    assert!(
        advice.contains("update delta --apply --strategy overlay"),
        "{origin} must name the overlay that keeps the edits: {advice}",
    );
    assert!(
        advice.contains("update delta --apply --strategy discard"),
        "{origin} must name the discard that backs the edits up: {advice}",
    );
}

#[test]
fn doctor_and_explain_advice_for_a_gated_import_names_strategies_the_gate_allows() {
    let fixture = TrustGateFixture::install();
    fixture.edit_projected_copy();
    fixture.advance_upstream();

    for issue in fixture.doctor_drift_issues() {
        let fix = issue["fix"].as_str().expect("fix exists");
        assert_gated_repair(fix, "the doctor projection-drift fix");
    }

    let (code, output) = fixture.run_human(&["doctor"]);
    assert_eq!(code, 2, "a warning is not a failure: {output}");
    assert!(
        output.contains("fix: run 'skillctl update delta --apply --strategy fork'"),
        "the human report repeats the gated advice: {output}",
    );

    let (code, body) = fixture.run_json(&["explain", "delta"]);
    assert_eq!(code, 0, "explain reports the drift without failing: {body}");
    let drifted: Vec<&Value> = body["data"]["targets"]
        .as_array()
        .expect("targets exist")
        .iter()
        .filter(|target| target["observed_preserved_reason"] == json!("dirty-projected-copy"))
        .collect();
    assert_eq!(drifted.len(), 1, "the edited copy is reported: {body}");
    for target in drifted {
        let fix = target["fix"].as_str().expect("fix exists");
        assert_gated_repair(fix, "the explain target fix");
    }

    let (code, summary) = fixture.run_human(&["explain", "delta"]);
    assert_eq!(code, 0, "the human summary reports the drift: {summary}");
    assert_gated_repair(&summary, "the explain summary");
}

#[test]
fn sync_preserved_report_for_a_gated_import_names_strategies_the_gate_allows() {
    let fixture = TrustGateFixture::install();
    fixture.edit_projected_copy();
    fixture.advance_upstream();

    for entry in fixture.preserved_entries("sync") {
        assert_eq!(entry["skill"], "delta");
        assert_eq!(entry["reason"], "dirty-projected-copy");
        let repair = entry["repair"].as_str().expect("repair exists");
        assert_gated_repair(repair, "the sync preserved entry");
    }
    let (_, body) = fixture.run_json(&["sync"]);
    assert!(
        body["warnings"]
            .as_array()
            .expect("warnings exist")
            .iter()
            .any(|warning| warning
                .as_str()
                .unwrap_or("")
                .contains("update delta --apply --strategy fork")),
        "the preserved-copy warning repeats the gated advice: {}",
        body["warnings"],
    );

    // The advertised repair works, so the advice is actionable.
    let (code, body) = fixture.run_json(&["update", "delta", "--apply", "--strategy", "fork"]);
    assert_ne!(code, 4, "the advertised fork is the way past the gate");
    assert_eq!(body["ok"], true, "the fork takes local ownership: {body}");
    assert!(
        fixture.projected_skill().contains("My local edit."),
        "the fork keeps the unmanaged edit",
    );
}

#[test]
fn clean_preserved_report_for_a_gated_import_names_strategies_the_gate_allows() {
    let fixture = TrustGateFixture::install();
    fixture.edit_projected_copy();
    fixture.advance_upstream();

    for entry in fixture.preserved_entries("clean") {
        assert_eq!(entry["skill"], "delta");
        let repair = entry["repair"].as_str().expect("repair exists");
        assert_gated_repair(repair, "the clean preserved entry");
    }
    assert!(
        fixture.projected_skill().contains("My local edit."),
        "clean keeps the unmanaged edit while it reports it",
    );
}

#[test]
fn gated_drift_advice_does_not_wait_for_an_upstream_check() {
    let fixture = TrustGateFixture::install();
    fixture.edit_projected_copy();

    // No recorded check vouches for the pin, so the advice cannot rule out a
    // waiting revision. It must not name the update strategies, because the
    // moment an update appears the gate refuses them.
    for issue in fixture.doctor_drift_issues() {
        let fix = issue["fix"].as_str().expect("fix exists");
        assert_gated_repair(fix, "the doctor projection-drift fix");
    }

    // The discard the advice names resolves the drift without an update.
    let (code, body) = fixture.run_json(&["sync", "--discard"]);
    assert_eq!(code, 0, "the advertised discard succeeds: {body}");
    assert!(
        !fixture.projected_skill().contains("My local edit."),
        "the advertised discard regenerates the copy",
    );
}

#[test]
fn a_recorded_uptodate_check_lets_the_advice_name_the_update_strategies() {
    let fixture = TrustGateFixture::install();
    fixture.edit_projected_copy();

    // The check records that no revision waits behind the pin, which is the
    // local evidence that a repair keeps the pinned revision and the gate lets
    // it through.
    let (code, body) = fixture.run_json(&["update", "delta"]);
    assert_eq!(code, 2, "a check with warnings is not a failure: {body}");
    assert_eq!(
        body["data"]["plans"][0]["outcome"], "up-to-date",
        "the fixture holds no waiting revision: {body}",
    );

    for issue in fixture.doctor_drift_issues() {
        let fix = issue["fix"].as_str().expect("fix exists");
        assert_ungated_repair(fix, "the doctor projection-drift fix");
    }
    for entry in fixture.preserved_entries("sync") {
        let repair = entry["repair"].as_str().expect("repair exists");
        assert_ungated_repair(repair, "the sync preserved entry");
    }

    let (code, body) = fixture.run_json(&["explain", "delta"]);
    assert_eq!(code, 0, "explain reports the drift without failing: {body}");
    let drifted: Vec<&Value> = body["data"]["targets"]
        .as_array()
        .expect("targets exist")
        .iter()
        .filter(|target| target["observed_preserved_reason"] == json!("dirty-projected-copy"))
        .collect();
    assert_eq!(drifted.len(), 1, "the edited copy is reported: {body}");
    for target in drifted {
        let fix = target["fix"].as_str().expect("fix exists");
        assert_ungated_repair(fix, "the explain target fix");
    }
    let (code, summary) = fixture.run_human(&["explain", "delta"]);
    assert_eq!(code, 0, "the human summary reports the drift: {summary}");
    assert_ungated_repair(&summary, "the explain summary");

    // The advertised repair runs instead of meeting the gate, and it resolves
    // the drift it was named for.
    let (code, body) = fixture.run_json(&["update", "delta", "--apply", "--strategy", "overlay"]);
    assert_ne!(
        code, 4,
        "a repair that keeps the pinned revision is not gated: {body}",
    );
    assert_eq!(body["ok"], true, "the advertised overlay applies: {body}");
    let overlay = fixture
        .workspace
        .path()
        .join(".agents/overlays/delta/SKILL.md");
    assert!(
        fs::read_to_string(&overlay)
            .expect("the overlay exists")
            .contains("My local edit."),
        "the advertised overlay keeps the edit",
    );
    assert!(
        !fixture.doctor_reports_drift(),
        "the advertised overlay resolves the drift it was named for",
    );
}

#[test]
fn a_recorded_pending_update_keeps_the_gated_advice() {
    let fixture = TrustGateFixture::install();
    fixture.edit_projected_copy();
    fixture.advance_upstream();

    let (code, body) = fixture.run_json(&["update", "delta"]);
    assert_eq!(code, 2, "a check with warnings is not a failure: {body}");
    assert_eq!(
        body["data"]["plans"][0]["outcome"], "blocked",
        "the waiting revision blocks the import: {body}",
    );

    // The recorded check now vouches for a waiting revision, so the gate
    // refuses the repair strategies and the advice must not name them.
    for issue in fixture.doctor_drift_issues() {
        let fix = issue["fix"].as_str().expect("fix exists");
        assert_gated_repair(fix, "the doctor projection-drift fix");
    }
}

/// The advice for a script-free import follows the block the recorded check
/// saw. The installed content is script-free, so only the check's look at the
/// waiting revision can tell a refused repair from an allowed one, and its row
/// is where that observation survives.
#[test]
fn drift_advice_for_a_script_free_import_follows_the_recorded_incoming_block() {
    let fixture = IncomingScriptsFixture::install_and_advance();
    fixture.edit_projected_copy();

    // The check reads the waiting revision before it reports, so the row it
    // records is the local evidence the repair apply would meet the gate.
    let (code, body) = fixture.run_json(&["update", "delta"]);
    assert_eq!(code, 2, "a check with warnings is not a failure: {body}");
    assert_eq!(
        fixture.recorded_block_reason().as_deref(),
        Some("incoming-scripts"),
        "the recorded check carries the block reason the advice reads",
    );

    for issue in fixture.doctor_drift_issues() {
        let fix = issue["fix"].as_str().expect("fix exists");
        assert_gated_repair(fix, "the doctor projection-drift fix");
    }
    for entry in fixture.preserved_entries("sync") {
        let repair = entry["repair"].as_str().expect("repair exists");
        assert_gated_repair(repair, "the sync preserved entry");
    }
    for entry in fixture.preserved_entries("clean") {
        let repair = entry["repair"].as_str().expect("repair exists");
        assert_gated_repair(repair, "the clean preserved entry");
    }

    let (code, body) = fixture.run_json(&["explain", "delta"]);
    assert_eq!(code, 0, "explain reports the drift without failing: {body}");
    let drifted: Vec<&Value> = body["data"]["targets"]
        .as_array()
        .expect("targets exist")
        .iter()
        .filter(|target| target["observed_preserved_reason"] == json!("dirty-projected-copy"))
        .collect();
    assert_eq!(drifted.len(), 1, "the edited copy is reported: {body}");
    for target in drifted {
        let fix = target["fix"].as_str().expect("fix exists");
        assert_gated_repair(fix, "the explain target fix");
    }

    // The strategies the advice withheld are refused exactly as predicted, so
    // naming them would have been advice the engine rejects.
    let before = fixture.apply_state();
    let (code, body) = fixture.run_json(&["update", "delta", "--apply", "--strategy", "overlay"]);
    assert_eq!(code, 4, "the withheld repair meets the gate: {body}");
    assert_eq!(
        fixture.apply_state(),
        before,
        "the refused apply changed nothing"
    );

    // The fork the advice names is the way past the gate, and it keeps the edit.
    let (code, body) = fixture.run_json(&["update", "delta", "--apply", "--strategy", "fork"]);
    assert_ne!(
        code, 4,
        "the advertised fork is the way past the gate: {body}"
    );
    assert_eq!(body["ok"], true, "the fork takes local ownership: {body}");
    let canonical = fixture
        .workspace
        .path()
        .join(".agents/skills/delta/SKILL.md");
    assert!(
        fs::read_to_string(&canonical)
            .expect("the forked skill exists")
            .contains("My local edit."),
        "the advertised fork keeps the unmanaged edit",
    );
}

/// A script-free import whose waiting revision adds no scripts keeps the update
/// strategies, because the repair apply then moves the import without meeting
/// the gate. The check must record what it saw, not block every waiting
/// revision.
#[test]
fn a_recorded_waiting_revision_without_scripts_keeps_the_update_strategies() {
    let workspace = TestWorkspace::new();
    workspace.write_manifest(CLAUDE_MANIFEST);
    workspace.write_file(
        "git-source/.agents/skills/delta/SKILL.md",
        &skill_manifest("delta", "Helper.", "Original body."),
    );
    workspace.init_git_repo("git-source");
    let run = |args: &[&str]| {
        let output = Command::cargo_bin("skillctl")
            .expect("binary exists")
            .current_dir(workspace.path())
            .env("HOME", workspace.home_path())
            .args(args)
            .output()
            .expect("command launches");
        let body: Value = serde_json::from_slice(&output.stdout).expect("stdout is valid json");
        (
            output.status.code().expect("process exits with a code"),
            body,
        )
    };

    run(&[
        "--json",
        "--no-input",
        "--name",
        "delta",
        "install",
        &workspace.git_repo_url("git-source"),
    ]);
    let installed_revision = workspace.git_head("git-source");

    // The waiting revision stays script-free, so the check records a move the
    // gate allows.
    workspace.write_file(
        "git-source/.agents/skills/delta/SKILL.md",
        &skill_manifest("delta", "Helper.", "Upstream body."),
    );
    workspace.commit_all("git-source", "advance upstream without scripts");
    let upstream_revision = workspace.git_head("git-source");
    fs::write(
        workspace.path().join(".claude/skills/delta/SKILL.md"),
        skill_manifest("delta", "Directly edited runtime copy.", "My local edit."),
    )
    .expect("projected copy edited");

    let (code, body) = run(&["--json", "update", "delta"]);
    assert_eq!(
        code, 0,
        "a check without a gate block reports success: {body}"
    );
    assert_eq!(
        body["data"]["plans"][0]["outcome"], "blocked",
        "the edits hold the update back: {body}",
    );
    let connection = Connection::open(workspace.home_path().join(".skillctl/state.db"))
        .expect("state store opens");
    let reason: Option<String> = connection
        .query_row(
            "SELECT apply_blocked_by FROM update_checks \
             WHERE skill_id = ?1 ORDER BY id DESC LIMIT 1",
            params!["delta"],
            |row| row.get(0),
        )
        .expect("update check row reads");
    assert_eq!(
        reason, None,
        "a script-free waiting revision records no block",
    );

    let (code, body) = run(&["--json", "doctor"]);
    assert_eq!(code, 2, "doctor reports the drift as a warning: {body}");
    for issue in body["data"]["issues"]
        .as_array()
        .expect("issues exist")
        .iter()
        .filter(|issue| issue["code"] == "projection-drift")
    {
        let fix = issue["fix"].as_str().expect("fix exists");
        assert_ungated_repair(fix, "the doctor projection-drift fix");
    }

    // The advertised overlay is allowed and resolves the drift.
    let (code, body) = run(&[
        "--json",
        "update",
        "delta",
        "--apply",
        "--strategy",
        "overlay",
    ]);
    assert_eq!(code, 0, "the advertised overlay applies: {body}");
    let lockfile = workspace.read_lockfile_yaml();
    assert_eq!(
        lockfile["imports"]["delta"]["revision"]["resolved"]
            .as_str()
            .unwrap_or("<missing>"),
        upstream_revision.as_str(),
        "the advertised overlay moved to the waiting revision: {lockfile:?}",
    );
    assert_ne!(
        lockfile["imports"]["delta"]["revision"]["resolved"]
            .as_str()
            .unwrap_or("<missing>"),
        installed_revision.as_str(),
    );
    let overlay = workspace.path().join(".agents/overlays/delta/SKILL.md");
    assert!(
        fs::read_to_string(&overlay)
            .expect("overlay file exists")
            .contains("My local edit."),
        "the advertised overlay keeps the edit",
    );
}

/// A review recorded after the check licenses the move the check held back, so
/// the advice names the update strategies again.
#[test]
fn a_review_lets_the_drift_advice_name_the_update_strategies_again() {
    let fixture = IncomingScriptsFixture::install_and_advance();
    fixture.edit_projected_copy();

    let (code, _) = fixture.run_json(&["update", "delta"]);
    assert_eq!(code, 2, "the check reports the gate as a warning");
    for issue in fixture.doctor_drift_issues() {
        let fix = issue["fix"].as_str().expect("fix exists");
        assert_gated_repair(fix, "the gated doctor fix");
    }

    let (code, body) = fixture.run_json(&["review", "delta", "--approve"]);
    assert_eq!(code, 0, "the approval records: {body}");

    // The recorded check still names the blocked revision, but the approval now
    // licenses that move, so the repair strategies work again.
    assert_eq!(
        fixture.recorded_block_reason().as_deref(),
        Some("incoming-scripts"),
        "the recorded row keeps the observation it made",
    );
    for issue in fixture.doctor_drift_issues() {
        let fix = issue["fix"].as_str().expect("fix exists");
        assert_ungated_repair(fix, "the reviewed doctor fix");
    }

    let (code, body) = fixture.run_json(&["update", "delta", "--apply", "--strategy", "overlay"]);
    assert_eq!(
        code, 0,
        "the advertised overlay applies after the review: {body}",
    );
    assert_eq!(
        fixture.workspace.read_lockfile_yaml()["imports"]["delta"]["revision"]["resolved"],
        fixture.upstream_revision.as_str(),
        "the repair moved to the revision the review licensed: {body}",
    );
    let overlay = fixture
        .workspace
        .path()
        .join(".agents/overlays/delta/SKILL.md");
    assert!(
        fs::read_to_string(&overlay)
            .expect("overlay file exists")
            .contains("My local edit."),
        "the advertised overlay keeps the edit",
    );
}

#[test]
fn a_commit_pin_lets_the_advice_name_the_update_strategies_without_a_check() {
    let fixture = TrustGateFixture::install();
    let (code, body) = fixture.run_json(&["pin", "delta", &fixture.installed_revision]);
    assert_eq!(
        code, 0,
        "a pin of the current revision changes nothing: {body}"
    );
    fixture.edit_projected_copy();

    // The pin names the commit itself, so no upstream ref can report a newer
    // revision and no check is needed to prove it.
    for issue in fixture.doctor_drift_issues() {
        let fix = issue["fix"].as_str().expect("fix exists");
        assert_ungated_repair(fix, "the doctor projection-drift fix");
    }

    let (code, body) = fixture.run_json(&["update", "delta", "--apply", "--strategy", "discard"]);
    assert_ne!(
        code, 4,
        "a repair that keeps a commit pin is not gated: {body}",
    );
    assert_eq!(body["ok"], true, "the advertised discard applies: {body}");
    assert!(
        !fixture.projected_skill().contains("My local edit."),
        "the advertised discard regenerates the copy from the pinned revision",
    );
    assert!(
        !fixture.doctor_reports_drift(),
        "the advertised discard resolves the drift it was named for",
    );
}

// ---------------------------------------------------------------------------
// Gated pin and gated rollback
// ---------------------------------------------------------------------------

/// The refusal a gated pin or rollback prints must name its own repair: the
/// review that licenses the move and the fork command that takes local
/// ownership, never the update strategy an explicit revision move cannot use.
fn assert_names_revision_move_repair(message: &str, origin: &str) {
    assert!(
        message.contains("'skillctl review delta --approve'"),
        "{origin} must name the review that licenses the move: {message}",
    );
    assert!(
        message.contains("'skillctl fork delta'"),
        "{origin} must name the fork command for an explicit revision move: {message}",
    );
    assert!(
        !message.contains("--strategy"),
        "{origin} must not name an update strategy: {message}",
    );
}

#[test]
fn trust_gate_blocks_a_pin_that_moves_an_unreviewed_script_bearing_import() {
    let fixture = TrustGateFixture::install();
    fixture.advance_upstream();
    let before = fixture.apply_state();

    let (code, body) = fixture.run_json(&["pin", "delta", &fixture.upstream_revision()]);

    assert_eq!(
        code, 4,
        "a pin that moves a gated import must use the trust-gate exit code: {body}",
    );
    assert_eq!(body["ok"], false);
    assert_eq!(body["command"], "pin");
    let error = body["errors"][0].as_str().expect("error message exists");
    assert!(
        error.contains(BLOCKED_PIN_MESSAGE),
        "unexpected error: {error}",
    );
    assert_names_revision_move_repair(error, "the pin refusal");

    assert_eq!(fixture.apply_state(), before, "no pin state may change");
    assert!(
        fixture.projected_skill().contains("Original body."),
        "the projected copy must keep the pinned revision",
    );
    assert_eq!(
        fixture.workspace.read_lockfile_yaml()["imports"]["delta"]["revision"]["resolved"],
        fixture.installed_revision.as_str(),
        "the lockfile must keep the pinned revision",
    );
    assert!(
        !fixture.history_kinds().contains(&"pin".to_string()),
        "a refused pin must not record a pin event",
    );

    // The human report prints the same refusal on stderr.
    let output = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(fixture.workspace.path())
        .env("HOME", fixture.workspace.home_path())
        .args(["pin", "delta", &fixture.upstream_revision()])
        .output()
        .expect("command launches");
    assert_eq!(output.status.code(), Some(4));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(BLOCKED_PIN_MESSAGE),
        "the human refusal names the gate: {stderr}",
    );
}

#[test]
fn trust_gate_blocks_a_rollback_that_moves_an_unreviewed_script_bearing_import() {
    let fixture = TrustGateFixture::install();
    fixture.advance_upstream();
    fixture.review_and_apply_upstream();
    let before = fixture.apply_state();

    let (code, body) = fixture.run_json(&["rollback", "delta", &fixture.installed_revision]);

    assert_eq!(
        code, 4,
        "a rollback that moves a gated import must use the trust-gate exit code: {body}",
    );
    assert_eq!(body["ok"], false);
    assert_eq!(body["command"], "rollback");
    let error = body["errors"][0].as_str().expect("error message exists");
    assert!(
        error.contains(BLOCKED_ROLLBACK_MESSAGE),
        "unexpected error: {error}",
    );
    assert_names_revision_move_repair(error, "the rollback refusal");

    assert_eq!(
        fixture.apply_state(),
        before,
        "no rollback state may change"
    );
    assert!(
        fixture.projected_skill().contains("Upstream body."),
        "the projected copy must keep the applied revision",
    );
    assert_eq!(
        fixture.workspace.read_lockfile_yaml()["imports"]["delta"]["revision"]["resolved"],
        fixture.upstream_revision().as_str(),
        "the lockfile must keep the applied revision",
    );
    assert!(
        !fixture.history_kinds().contains(&"rollback".to_string()),
        "a refused rollback must not record a rollback event",
    );
}

#[test]
fn a_pin_to_a_revision_that_introduces_scripts_names_the_incoming_revision() {
    let fixture = IncomingScriptsFixture::install_and_advance();
    let before = fixture.apply_state();

    let (code, body) = fixture.run_json(&["pin", "delta", &fixture.upstream_revision]);

    assert_eq!(
        code, 4,
        "the first arrival of executable content meets the gate through pin: {body}",
    );
    assert_eq!(body["ok"], false);
    let error = body["errors"][0].as_str().expect("error message exists");
    assert!(
        error.contains(BLOCKED_PIN_MESSAGE),
        "unexpected error: {error}",
    );
    assert!(
        error.contains(&format!(
            "the incoming revision {} adds a top-level scripts/ directory",
            fixture.upstream_revision
        )),
        "the pin refusal names the revision that adds scripts: {error}",
    );
    assert_names_revision_move_repair(error, "the pin refusal");

    assert_eq!(fixture.apply_state(), before, "no pin state may change");
    assert!(
        !fixture.projected_script_exists(),
        "unreviewed executable content must not reach a projection",
    );
}

#[test]
fn a_rollback_to_a_revision_that_introduces_scripts_names_the_incoming_revision() {
    let fixture = IncomingScriptsFixture::install_and_advance();
    // Pinning without a review is gated, so the import only reaches the
    // script-bearing revision through the update path the gate controls. The
    // rollback target is the script-free revision the import left behind,
    // while the installed scripts are what the gate reports.
    let (code, body) = fixture.run_json(&["review", "delta", "--approve"]);
    assert_eq!(code, 0, "the approval records: {body}");
    let (code, body) = fixture.run_json(&["update", "delta", "--apply"]);
    assert_eq!(code, 0, "the reviewed update applies: {body}");
    let before = fixture.apply_state();

    let (code, body) = fixture.run_json(&["rollback", "delta", &fixture.installed_revision]);

    assert_eq!(
        code, 4,
        "a rollback that leaves a gated import must meet the gate: {body}",
    );
    assert_eq!(body["ok"], false);
    let error = body["errors"][0].as_str().expect("error message exists");
    assert!(
        error.contains(BLOCKED_ROLLBACK_MESSAGE),
        "unexpected error: {error}",
    );
    // The installed content holds the scripts, so the refusal names the
    // installed content and no incoming clause.
    assert!(
        !error.contains("incoming revision"),
        "the refusal reports the content the workspace holds: {error}",
    );
    assert_names_revision_move_repair(error, "the rollback refusal");
    assert_eq!(
        fixture.apply_state(),
        before,
        "no rollback state may change"
    );
}

#[test]
fn a_recorded_review_licenses_a_gated_pin() {
    let fixture = IncomingScriptsFixture::install_and_advance();

    let (code, body) = fixture.run_json(&["review", "delta", "--approve"]);
    assert_eq!(code, 0, "the approval records: {body}");

    let (code, body) = fixture.run_json(&["pin", "delta", &fixture.upstream_revision]);

    assert_eq!(
        code, 0,
        "a recorded approval is the documented way past the gate: {body}",
    );
    assert_eq!(
        fixture.workspace.read_lockfile_yaml()["imports"]["delta"]["revision"]["resolved"],
        fixture.upstream_revision.as_str(),
        "the pin moves the import to the revision that adds scripts",
    );
    assert!(
        fixture.projected_script_exists(),
        "the reviewed revision projects its scripts",
    );

    // The approval covered the script-free content, so the import is
    // unreviewed again and `doctor` reports the scripts that arrived.
    let (code, body) = fixture.run_json(&["doctor"]);
    assert_eq!(code, 2, "the new scripts are a warning: {body}");
    assert!(
        body["data"]["issues"]
            .as_array()
            .expect("issues exist")
            .iter()
            .any(|issue| issue["code"] == "script-risk"),
        "doctor reports the executable content that arrived: {body}",
    );
}

#[test]
fn a_recorded_review_is_the_recovery_path_for_a_gated_rollback() {
    let fixture = TrustGateFixture::install();
    fixture.advance_upstream();
    fixture.review_and_apply_upstream();

    // The refusal must not be a dead end: the review it names covers the
    // applied content, and the same rollback then succeeds.
    let (code, body) = fixture.run_json(&["rollback", "delta", &fixture.installed_revision]);
    assert_eq!(code, 4, "the unreviewed rollback is gated: {body}");
    let error = body["errors"][0].as_str().expect("error message exists");
    assert_names_revision_move_repair(error, "the rollback refusal");

    let (code, body) = fixture.run_json(&["review", "delta", "--approve"]);
    assert_eq!(code, 0, "the approval records: {body}");

    let (code, body) = fixture.run_json(&["rollback", "delta", &fixture.installed_revision]);
    assert_eq!(
        code, 0,
        "the reviewed rollback is the documented recovery path: {body}",
    );
    assert_eq!(
        fixture.workspace.read_lockfile_yaml()["imports"]["delta"]["revision"]["resolved"],
        fixture.installed_revision.as_str(),
        "the rollback restored the previous revision",
    );
    assert!(
        fixture.projected_skill().contains("Original body."),
        "the rollback restored the previous content",
    );
    assert!(
        fixture.history_kinds().contains(&"rollback".to_string()),
        "the rollback records its event",
    );
}

#[test]
fn the_fork_command_stays_open_for_a_gated_revision_move() {
    let fixture = TrustGateFixture::install();
    fixture.advance_upstream();

    let (code, body) = fixture.run_json(&["fork", "delta"]);

    assert_ne!(
        code, 4,
        "the fork the pin and rollback refusals name is the way past the gate: {body}",
    );
    assert_eq!(body["ok"], true, "the fork takes local ownership: {body}");
    let canonical = fixture
        .workspace
        .path()
        .join(".agents/skills/delta/SKILL.md");
    assert!(
        fs::read_to_string(&canonical)
            .expect("the forked skill exists")
            .contains("Original body."),
        "the fork keeps the pinned content in local ownership",
    );
    assert!(
        fixture.workspace.read_lockfile_yaml()["imports"]
            .get("delta")
            .is_none(),
        "the fork removes the managed import",
    );
}

#[test]
fn a_pin_or_rollback_of_the_current_revision_stays_ungated() {
    let fixture = TrustGateFixture::install();

    // A move to the revision the import already holds changes nothing, so the
    // gate, which holds back revision moves, lets both commands through.
    let (code, body) = fixture.run_json(&["pin", "delta", &fixture.installed_revision]);
    assert_eq!(
        code, 0,
        "a pin of the current revision changes nothing: {body}"
    );

    let (code, body) = fixture.run_json(&["rollback", "delta", &fixture.installed_revision]);
    assert_eq!(
        code, 0,
        "a rollback to the current revision changes nothing: {body}",
    );
    assert!(
        fixture.projected_skill().contains("Original body."),
        "the projected copy keeps the pinned content",
    );
}
