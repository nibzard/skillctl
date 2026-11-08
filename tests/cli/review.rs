//! Local review decisions and the reachable `imported-reviewed` state.
//!
//! `TrustState::ImportedReviewed` only exists when a recorded approval covers
//! the effective content. These tests drive `skillctl review` end to end: the
//! gate refuses an apply while the import is unreviewed, a recorded approval
//! moves the import to `imported-reviewed` and lets the apply through, and the
//! approval stops covering the skill as soon as an update moves its content.

use super::*;
use assert_cmd::assert::Assert;
use rusqlite::OptionalExtension;

const CLAUDE_MANIFEST: &str = concat!("version: 1\n", "\n", "targets:\n", "  - claude-code\n",);

const BLOCKED_MESSAGE: &str = "trust gate is blocking update apply for 'delta'";

/// One `delta` skill installed from a Git source, with or without scripts.
struct ReviewFixture {
    workspace: TestWorkspace,
    installed_revision: String,
}

impl ReviewFixture {
    fn install() -> Self {
        Self::install_with_option("scripts")
    }

    /// Install `delta` without a `scripts/` tree, so its unreviewed risk stays
    /// normal.
    fn install_script_free() -> Self {
        Self::install_with_option("plain")
    }

    fn install_with_option(variant: &'static str) -> Self {
        let workspace = TestWorkspace::new();
        workspace.write_manifest(CLAUDE_MANIFEST);
        workspace.write_skill_source("git-source", "delta");
        if variant == "scripts" {
            workspace.write_file(
                "git-source/.agents/skills/delta/scripts/run.sh",
                "#!/bin/sh\necho original\n",
            );
        }
        workspace.init_git_repo("git-source");
        let repo_url = workspace.git_repo_url("git-source");

        // A script-bearing import reports the unreviewed script warning and
        // exits with `2`; a script-free install stays a plain success.
        let expected_code = if variant == "scripts" { 2 } else { 0 };
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
            .code(expected_code)
            .stderr(predicate::str::is_empty());

        let installed_revision = workspace.git_head("git-source");
        Self {
            workspace,
            installed_revision,
        }
    }

    /// Publish a newer upstream revision and return its commit.
    fn advance_upstream(&self, body: &str) -> String {
        self.workspace
            .write_skill_source_at("git-source", ".agents/skills/delta", "delta", body);
        fs::write(
            self.workspace
                .path()
                .join("git-source/.agents/skills/delta/scripts/run.sh"),
            "#!/bin/sh\necho upstream\n",
        )
        .expect("upstream script written");
        self.workspace.commit_all("git-source", "advance upstream");
        self.workspace.git_head("git-source")
    }

    /// One command run without the empty-stderr assertion, for refusals.
    fn run_refusal(&self, args: &[&str]) -> (i32, String) {
        let output = Command::cargo_bin("skillctl")
            .expect("binary exists")
            .current_dir(self.workspace.path())
            .env("HOME", self.workspace.home_path())
            .args(args)
            .output()
            .expect("command launches");
        (
            output.status.code().expect("process exits with a code"),
            format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            ),
        )
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

    /// JSON body and exit code of one command.
    fn run_json(&self, args: &[&str]) -> (i32, Value) {
        let output = Command::cargo_bin("skillctl")
            .expect("binary exists")
            .current_dir(self.workspace.path())
            .env("HOME", self.workspace.home_path())
            .args(["--json"])
            .args(args)
            .output()
            .expect("command launches");
        let body: Value = serde_json::from_slice(&output.stdout).expect("stdout is valid json");
        (
            output.status.code().expect("process exits with a code"),
            body,
        )
    }

    fn projected_skill(&self) -> String {
        fs::read_to_string(self.workspace.path().join(".claude/skills/delta/SKILL.md"))
            .expect("projected skill exists")
    }

    /// The recorded review row, read straight from the local state store.
    fn stored_review(&self) -> Option<(String, String, bool)> {
        let connection = Connection::open(self.workspace.home_path().join(".skillctl/state.db"))
            .expect("state store opens");
        connection
            .query_row(
                "SELECT decision, effective_version_hash, contains_scripts FROM skill_reviews \
                 WHERE skill_id = 'delta'",
                [],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)? == 1,
                    ))
                },
            )
            .optional()
            .expect("review query runs")
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
fn review_reports_an_unreviewed_script_bearing_import() {
    let fixture = ReviewFixture::install();

    let (code, body) = fixture.run_json(&["review", "delta"]);

    assert_eq!(code, 2, "the elevated warning is a warning, not a failure");
    assert_eq!(
        body["data"]["trust"]["effective_state"],
        "imported-unreviewed"
    );
    assert_eq!(body["data"]["trust"]["risk_level"], "elevated");
    assert_eq!(body["data"]["trust"]["review_required"], true);
    assert_eq!(body["data"]["trust"]["contains_scripts"], true);
    assert_eq!(body["data"]["review"], json!(null));
    assert_eq!(body["data"]["review_covers_current_content"], false);
    assert_eq!(
        fixture.stored_review(),
        None,
        "a status run records nothing"
    );
}

#[test]
fn review_help_documents_the_command() {
    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .args(["review", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Show or record the local review decision for a managed imported skill.",
        ))
        .stdout(predicate::str::contains("skillctl review ai-sdk --approve"))
        .stdout(predicate::str::contains("skillctl review ai-sdk --revoke"));

    // The update help names the review as the way past the gate, so the
    // refusal message and the documentation agree.
    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .args(["update", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Record a local review with 'skillctl review <skill> --approve'",
        ));
}

#[test]
fn a_recorded_approval_unblocks_the_gated_apply() {
    let fixture = ReviewFixture::install();
    let upstream_revision = fixture.advance_upstream("Upstream body.");

    let (code, _) = fixture.run_json(&["update", "delta", "--apply"]);
    assert_eq!(code, 4, "the gate refuses while the import is unreviewed");

    fixture
        .run(&[
            "review",
            "delta",
            "--approve",
            "--note",
            "scripts inspected, safe",
        ])
        .success()
        .stdout(predicate::str::contains("Recorded a local review of delta"))
        .stdout(predicate::str::contains(
            "an applied update returns the skill to imported-unreviewed",
        ));

    let (decision, hash, contains_scripts) = fixture
        .stored_review()
        .expect("the decision is stored locally");
    assert_eq!(decision, "approved");
    assert_eq!(hash, effective_version_hash(&fixture));
    assert!(contains_scripts, "the decision records the scripts fact");

    // The recorded decision changes local state only: the workspace contract
    // still pins the same revision until an update applies.
    assert_eq!(
        fixture.workspace.read_lockfile_yaml()["imports"]["delta"]["revision"]["resolved"],
        fixture.installed_revision.as_str(),
    );

    let (code, body) = fixture.run_json(&["review", "delta"]);
    assert_eq!(code, 0, "a reviewed import raises no warning");
    assert_eq!(
        body["data"]["trust"]["effective_state"],
        "imported-reviewed"
    );
    assert_eq!(body["data"]["trust"]["risk_level"], "normal");
    assert_eq!(body["data"]["trust"]["review_required"], false);
    assert_eq!(body["data"]["review"]["decision"], "approved");
    assert_eq!(body["data"]["review"]["note"], "scripts inspected, safe");
    assert_eq!(body["data"]["review_covers_current_content"], true);

    // The check now recommends the apply the gate held back.
    let (code, body) = fixture.run_json(&["update", "delta"]);
    assert_eq!(code, 0, "a reviewed import checks without warnings: {body}");
    let plan = &body["data"]["plans"][0];
    assert_eq!(plan["outcome"], "update-available");
    assert_eq!(plan["trust"]["effective_state"], "imported-reviewed");
    assert!(
        plan["trust"].get("blocked_actions").is_none(),
        "a reviewed import reports no blocked action: {plan}",
    );
    assert_eq!(plan["recommended_action"], "apply");

    let (code, body) = fixture.run_json(&["update", "delta", "--apply"]);
    assert_eq!(code, 0, "the approval is the documented way past the gate");
    assert_eq!(body["data"]["applied"][0]["skill"], "delta");
    assert!(
        fixture.projected_skill().contains("Upstream body."),
        "the apply moves the projection to the upstream revision",
    );
    assert_eq!(
        fixture.workspace.read_lockfile_yaml()["imports"]["delta"]["revision"]["resolved"],
        upstream_revision.as_str(),
    );
    assert!(
        fixture
            .history_kinds()
            .contains(&"trust-review".to_string()),
        "the decision reaches the local history ledger",
    );
}

#[test]
fn an_applied_update_returns_the_import_to_unreviewed() {
    let fixture = ReviewFixture::install();
    fixture.advance_upstream("Upstream body.");

    fixture.run(&["review", "delta", "--approve"]).success();
    fixture.run(&["update", "delta", "--apply"]).success();

    let (code, body) = fixture.run_json(&["review", "delta"]);
    assert_eq!(
        code, 2,
        "the new content is unreviewed again, so the warning returns: {body}",
    );
    assert_eq!(
        body["data"]["trust"]["effective_state"], "imported-unreviewed",
        "an approval covers one effective version only",
    );
    assert_eq!(body["data"]["trust"]["risk_level"], "elevated");
    // The stale decision stays on record, and the report says why it no longer
    // counts.
    assert_eq!(body["data"]["review"]["decision"], "approved");
    assert_eq!(body["data"]["review_covers_current_content"], false);
    fixture
        .run(&["review", "delta"])
        .code(2)
        .stdout(predicate::str::contains(
            "the recorded review covers an earlier version, so record a fresh one",
        ));

    fixture.advance_upstream("Second body.");
    let (code, body) = fixture.run_json(&["update", "delta", "--apply"]);
    assert_eq!(code, 4, "the gate returns with the new content");
    assert!(
        body["errors"][0]
            .as_str()
            .expect("error message exists")
            .contains(BLOCKED_MESSAGE),
    );
}

#[test]
fn revoking_a_recorded_review_restores_the_gate() {
    let fixture = ReviewFixture::install();
    fixture.advance_upstream("Upstream body.");
    fixture.run(&["review", "delta", "--approve"]).success();

    fixture
        .run(&["review", "delta", "--revoke"])
        .success()
        .stdout(predicate::str::contains(
            "Revoked the recorded review of delta.",
        ))
        .stdout(predicate::str::contains(
            "imported-unreviewed again, so the trust gate holds back update applies",
        ));

    let (decision, _, _) = fixture
        .stored_review()
        .expect("the revocation replaces the approval on record");
    assert_eq!(decision, "revoked");

    let (code, body) = fixture.run_json(&["review", "delta"]);
    assert_eq!(code, 2, "a revoked import is unreviewed");
    assert_eq!(
        body["data"]["trust"]["effective_state"],
        "imported-unreviewed"
    );
    assert_eq!(body["data"]["trust"]["risk_level"], "elevated");
    assert_eq!(body["data"]["review"]["decision"], "revoked");
    assert_eq!(body["data"]["review_covers_current_content"], true);

    // The content did not change with the revocation, so the summary states the
    // revoked decision instead of claiming the review covers an earlier
    // version.
    fixture
        .run(&["review", "delta"])
        .code(2)
        .stdout(predicate::str::contains(
            "the recorded review was revoked, so record a fresh one",
        ))
        .stdout(predicate::str::contains("covers an earlier version").not());

    let (code, body) = fixture.run_json(&["update", "delta", "--apply"]);
    assert_eq!(code, 4, "a revoked decision may not gate-bypass an apply");
    assert!(
        body["errors"][0]
            .as_str()
            .expect("error message exists")
            .contains(BLOCKED_MESSAGE),
    );
}

#[test]
fn a_revoked_decision_that_covers_the_content_is_not_reported_as_stale() {
    let fixture = ReviewFixture::install_script_free();

    fixture.run(&["review", "delta", "--approve"]).success();
    let (code, body) = fixture.run_json(&["review", "delta"]);
    assert_eq!(code, 0, "a script-free review raises no warning: {body}");
    assert_eq!(
        body["data"]["trust"]["effective_state"],
        "imported-reviewed"
    );

    // The revocation records the same content, so the decision on record still
    // covers what is installed; only the decision itself changed.
    fixture.run(&["review", "delta", "--revoke"]).success();
    let (code, body) = fixture.run_json(&["review", "delta"]);
    assert_eq!(
        code, 0,
        "a script-free unreviewed import stays a plain status: {body}",
    );
    assert_eq!(
        body["data"]["trust"]["effective_state"],
        "imported-unreviewed"
    );
    assert_eq!(body["data"]["trust"]["risk_level"], "normal");
    assert_eq!(body["data"]["review"]["decision"], "revoked");
    assert_eq!(body["data"]["review_covers_current_content"], true);
    assert_eq!(
        body["data"]["review"]["resolved_revision"], body["data"]["resolved_revision"],
        "the revoked record still names the installed revision",
    );

    // The human summary states the recorded decision, so it cannot contradict
    // the `review_covers_current_content` field of the same report.
    fixture
        .run(&["review", "delta"])
        .success()
        .stdout(predicate::str::contains(
            "delta in workspace scope is imported-unreviewed with normal risk",
        ))
        .stdout(predicate::str::contains(
            "the recorded review was revoked, so record a fresh one",
        ))
        .stdout(predicate::str::contains("covers an earlier version").not());
}

#[test]
fn the_refusal_names_the_review_command() {
    let fixture = ReviewFixture::install();
    fixture.advance_upstream("Upstream body.");

    let output = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(fixture.workspace.path())
        .env("HOME", fixture.workspace.home_path())
        .args(["update", "delta", "--apply"])
        .output()
        .expect("command launches");

    assert_eq!(output.status.code(), Some(4));
    let error = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        error.contains("record a local review with 'skillctl review delta --approve'"),
        "the refusal must name the command that records a review: {error}",
    );
    assert!(
        error.contains("--strategy fork"),
        "the refusal keeps naming the fork escape hatch: {error}",
    );
}

#[test]
fn doctor_stops_warning_about_a_reviewed_import() {
    let fixture = ReviewFixture::install();

    let (code, body) = fixture.run_json(&["doctor"]);
    assert_eq!(code, 2, "the unreviewed scripts are a warning: {body}");
    let risk: Vec<&Value> = body["data"]["issues"]
        .as_array()
        .expect("issues exist")
        .iter()
        .filter(|issue| issue["code"] == "script-risk")
        .collect();
    assert_eq!(risk.len(), 1, "exactly one script-risk issue: {body}");
    let fix = risk[0]["fix"].as_str().expect("fix exists");
    assert!(
        fix.contains("skillctl review delta --approve"),
        "the fix names the command that records the review: {fix}",
    );

    fixture.run(&["review", "delta", "--approve"]).success();
    let (code, body) = fixture.run_json(&["doctor"]);
    assert_eq!(
        code, 0,
        "a reviewed import raises no doctor warning at all: {body}",
    );
    let risk: Vec<&Value> = body["data"]["issues"]
        .as_array()
        .expect("issues exist")
        .iter()
        .filter(|issue| issue["code"] == "script-risk")
        .collect();
    assert!(risk.is_empty(), "the script-risk issue ends at the review");
}

#[test]
fn inspect_reports_a_reviewed_import_as_reviewed_locally() {
    let fixture = ReviewFixture::install();

    let (_, body) = fixture.run_json(&["inspect", "delta"]);
    assert_eq!(
        body["data"]["trust"]["effective_state"],
        "imported-unreviewed"
    );

    fixture.run(&["review", "delta", "--approve"]).success();
    let (_, body) = fixture.run_json(&["inspect", "delta"]);
    assert_eq!(
        body["data"]["trust"]["effective_state"],
        "imported-reviewed"
    );
    assert_eq!(body["data"]["trust"]["review"]["decision"], "approved");
}

#[test]
fn review_refuses_skills_that_carry_no_imported_content() {
    let fixture = ReviewFixture::install();

    // A fork moves the import into local ownership, which is trusted without a
    // review.
    fixture.run(&["fork", "delta"]).success();
    let (code, output) = fixture.run_refusal(&["review", "delta"]);
    assert_eq!(
        code, 3,
        "a validation refusal is a conflict, not an error: {output}"
    );
    assert!(
        output.contains("locally owned after a fork or detach"),
        "the refusal names the local ownership: {output}",
    );

    // The bundled management skill ships with skillctl, so nothing to review.
    let (code, bundled) = fixture.run_refusal(&["--scope", "user", "review", "skillctl"]);
    assert_eq!(code, 3, "the bundled skill is refused: {bundled}");
    assert!(
        bundled.contains("bundled content that skillctl ships"),
        "the refusal names the bundled content: {bundled}",
    );

    // A canonical local skill never had an import to review.
    fixture
        .workspace
        .write_workspace_skill("mine", "Local helper.", &[]);
    let (code, output) = fixture.run_refusal(&["review", "mine"]);
    assert_eq!(code, 3, "an unreviewable name is refused: {output}");
    assert!(
        output.contains("skill 'mine' is not installed"),
        "the refusal names the managed-install requirement: {output}",
    );
}

#[test]
fn a_review_does_not_touch_the_workspace_contract() {
    let fixture = ReviewFixture::install();
    let before = fixture.workspace.snapshot_paths(&[
        ".agents/skillctl.yaml",
        ".agents/skillctl.lock",
        ".claude/skills",
    ]);

    fixture.run(&["review", "delta", "--approve"]).success();
    fixture.run(&["review", "delta", "--revoke"]).success();

    assert_eq!(
        fixture.workspace.snapshot_paths(&[
            ".agents/skillctl.yaml",
            ".agents/skillctl.lock",
            ".claude/skills"
        ]),
        before,
        "a review decision is local state, so no workspace file may change",
    );
}

#[test]
fn a_review_survives_reopening_the_state_store() {
    let fixture = ReviewFixture::install();
    fixture.run(&["review", "delta", "--approve"]).success();

    // Every command opens the store again, so the second read proves the
    // decision persisted instead of living in one connection.
    let (_, body) = fixture.run_json(&["review", "delta"]);
    assert_eq!(
        body["data"]["trust"]["effective_state"],
        "imported-reviewed"
    );
    assert_eq!(body["data"]["review"]["decision"], "approved");
}

/// The effective version hash the state store currently records for `delta`.
fn effective_version_hash(fixture: &ReviewFixture) -> String {
    let connection = Connection::open(fixture.workspace.home_path().join(".skillctl/state.db"))
        .expect("state store opens");
    connection
        .query_row(
            "SELECT effective_version_hash FROM install_records WHERE skill_id = 'delta'",
            [],
            |row| row.get(0),
        )
        .expect("install record exists")
}

#[test]
fn a_new_overlay_stops_the_review_from_covering_the_content() {
    let fixture = ReviewFixture::install();
    fixture.run(&["review", "delta", "--approve"]).success();

    // An overlay changes the effective content, so the recorded approval no
    // longer covers what projects. The import is unreviewed again until
    // someone reads the customized result.
    fixture.run(&["override", "delta"]).success();
    let (code, body) = fixture.run_json(&["review", "delta"]);
    assert_eq!(code, 2, "the overlaid content is unreviewed: {body}");
    assert_eq!(
        body["data"]["trust"]["effective_state"],
        "imported-unreviewed",
    );
    assert_eq!(body["data"]["review_covers_current_content"], false);
    assert_ne!(
        body["data"]["effective_version_hash"], body["data"]["review"]["effective_version_hash"],
        "the report must show the decision covering an earlier version",
    );

    // Re-approving covers the overlaid content, because the decision records
    // the hash that is effective now.
    fixture.run(&["review", "delta", "--approve"]).success();
    let (code, body) = fixture.run_json(&["review", "delta"]);
    assert_eq!(code, 0, "the fresh approval covers the overlay: {body}");
    assert_eq!(
        body["data"]["trust"]["effective_state"],
        "imported-reviewed"
    );
    assert_eq!(body["data"]["review_covers_current_content"], true);
}
