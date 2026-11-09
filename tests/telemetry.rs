#[allow(dead_code)]
mod support;

use assert_cmd::Command;
use predicates::prelude::*;
use rusqlite::{Connection, params};
use serde_json::Value;

use support::TestWorkspace;

fn run_json(workspace: &TestWorkspace, source_date_epoch: &str, args: &[&str]) -> Value {
    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .env("SOURCE_DATE_EPOCH", source_date_epoch)
        .args(args)
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    serde_json::from_slice(&assert.get_output().stdout).expect("stdout is valid json")
}

fn telemetry_consent_history_count(workspace: &TestWorkspace) -> i64 {
    let connection = Connection::open(workspace.home_path().join(".skillctl/state.db"))
        .expect("state database opens");
    connection
        .query_row(
            "SELECT COUNT(*) FROM history_events WHERE kind = ?1",
            params!["telemetry-consent-changed"],
            |row| row.get(0),
        )
        .expect("history query succeeds")
}

#[test]
fn redundant_telemetry_toggles_report_no_change_and_write_no_history() {
    let workspace = TestWorkspace::new();

    // First disable records the consent change.
    let first_disable = run_json(
        &workspace,
        "1770000000",
        &["--json", "telemetry", "disable"],
    );
    assert_eq!(first_disable["command"], "telemetry-disable");
    assert_eq!(first_disable["data"]["consent"], "disabled");
    assert_eq!(first_disable["data"]["changed"], true);
    let first_updated_at = first_disable["data"]["updated_at"]
        .as_str()
        .expect("updated timestamp is present")
        .to_owned();
    assert_eq!(telemetry_consent_history_count(&workspace), 1);

    // A repeated disable one minute later must stay a no-op even though the
    // fresh updated_at timestamp differs from the persisted one.
    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .env("SOURCE_DATE_EPOCH", "1770000060")
        .args(["telemetry", "disable"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty())
        .stdout(predicate::str::contains(
            "Telemetry consent is already disabled.",
        ));

    let repeated_disable = run_json(
        &workspace,
        "1770000120",
        &["--json", "telemetry", "disable"],
    );
    assert_eq!(repeated_disable["data"]["consent"], "disabled");
    assert_eq!(repeated_disable["data"]["changed"], false);
    // The redundant disable rewrote nothing, so the row keeps the timestamp
    // of the one real disable.
    assert_eq!(
        repeated_disable["data"]["updated_at"],
        first_updated_at.as_str()
    );
    assert_eq!(telemetry_consent_history_count(&workspace), 1);

    // Enabling again is a real change, and repeating it is another no-op.
    let enable = run_json(&workspace, "1770000180", &["--json", "telemetry", "enable"]);
    assert_eq!(enable["data"]["consent"], "enabled");
    assert_eq!(enable["data"]["changed"], true);
    let enable_updated_at = enable["data"]["updated_at"]
        .as_str()
        .expect("updated timestamp is present")
        .to_owned();
    assert_eq!(telemetry_consent_history_count(&workspace), 2);

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .env("SOURCE_DATE_EPOCH", "1770000240")
        .args(["telemetry", "enable"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty())
        .stdout(predicate::str::contains(
            "Telemetry consent is already enabled.",
        ));

    let repeated_enable = run_json(&workspace, "1770000300", &["--json", "telemetry", "enable"]);
    assert_eq!(repeated_enable["data"]["consent"], "enabled");
    assert_eq!(repeated_enable["data"]["changed"], false);
    // The redundant enable rewrote nothing, so the row keeps the timestamp of
    // the one real enable.
    assert_eq!(
        repeated_enable["data"]["updated_at"],
        enable_updated_at.as_str()
    );
    assert_eq!(telemetry_consent_history_count(&workspace), 2);
}
