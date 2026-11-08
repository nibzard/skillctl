use super::*;

/// Install one workspace import and return the installed skill name.
fn install_workspace_import(workspace: &TestWorkspace, home_path: &std::path::Path) {
    workspace.write_manifest(concat!("version: 1\n", "\n", "targets:\n", "  - codex\n",));
    workspace.write_skill_source("shared-skills", "release-notes");

    initialize_runtime_state(workspace);

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", home_path)
        .args([
            "--json",
            "--no-input",
            "--name",
            "release-notes",
            "--scope",
            "workspace",
            "install",
            "shared-skills",
        ])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
}

/// Run one command without `--json` and return its stdout as text.
fn human_stdout(workspace: &TestWorkspace, home_path: &std::path::Path, args: &[&str]) -> String {
    let output = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", home_path)
        .args(args)
        .assert()
        .success()
        .stderr(predicate::str::is_empty())
        .get_output()
        .stdout
        .clone();
    String::from_utf8(output).expect("human stdout is valid utf-8")
}

/// Run one command with `--json` and return its parsed payload.
fn json_payload(workspace: &TestWorkspace, home_path: &std::path::Path, args: &[&str]) -> Value {
    let output = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", home_path)
        .args(args)
        .assert()
        .success()
        .stderr(predicate::str::is_empty())
        .get_output()
        .stdout
        .clone();
    serde_json::from_slice(&output).expect("stdout is json")
}

#[test]
fn human_list_names_each_managed_skill_with_its_scope_and_revision() {
    let workspace = TestWorkspace::new();
    let home_path = workspace.home_path();
    install_workspace_import(&workspace, &home_path);

    let payload = json_payload(&workspace, &home_path, &["--json", "list"]);
    let stdout = human_stdout(&workspace, &home_path, &["list"]);

    assert!(
        stdout.contains("Listed "),
        "list should keep its summary line: {stdout}"
    );
    for skill in payload["data"]["skills"].as_array().expect("skills exist") {
        let name = skill["skill"].as_str().expect("skill name is text");
        let scope = skill["scope"].as_str().expect("scope is text");
        let revision = skill["resolved_revision"]
            .as_str()
            .expect("revision is text");
        assert!(
            stdout.contains(&format!("  {name} [{scope}]")),
            "list should name {name} in scope {scope}: {stdout}"
        );
        assert!(
            stdout.contains(revision),
            "list should show the revision of {name}: {stdout}"
        );
        let targets: Vec<String> = skill["projections"]
            .as_array()
            .expect("projections exist")
            .iter()
            .map(|projection| {
                projection["target"]
                    .as_str()
                    .expect("target is text")
                    .to_string()
            })
            .collect();
        if !targets.is_empty() {
            assert!(
                stdout.contains(&targets.join(", ")),
                "list should show the projected targets of {name}: {stdout}"
            );
        }
    }
}

#[test]
fn human_path_lists_each_concrete_path_from_the_json_payload() {
    let workspace = TestWorkspace::new();
    let home_path = workspace.home_path();
    install_workspace_import(&workspace, &home_path);

    let payload = json_payload(&workspace, &home_path, &["--json", "path", "release-notes"]);
    let stdout = human_stdout(&workspace, &home_path, &["path", "release-notes"]);

    assert!(
        stdout.contains("Resolved filesystem paths for release-notes."),
        "path should keep its summary line: {stdout}"
    );
    for key in ["stored_source_root", "active_source_root"] {
        let path = payload["data"][key].as_str().expect("root is text");
        assert!(
            stdout.contains(path),
            "path should list the {key} at {path}: {stdout}"
        );
    }
    let revision = payload["data"]["resolved_revision"]
        .as_str()
        .expect("revision is text");
    assert!(
        stdout.contains(&format!("  revision: {revision}")),
        "path should list the resolved revision: {stdout}"
    );
    for root in payload["data"]["planned_roots"]
        .as_array()
        .expect("planned roots exist")
    {
        let target = root["target"].as_str().expect("target is text");
        let path = root["path"].as_str().expect("path is text");
        assert!(
            stdout.contains(&format!("  planned root ({target}): {path}")),
            "path should list the planned root for {target}: {stdout}"
        );
    }
    for projection in payload["data"]["projections"]
        .as_array()
        .expect("projections exist")
    {
        let target = projection["target"].as_str().expect("target is text");
        let path = projection["path"].as_str().expect("path is text");
        assert!(
            stdout.contains(&format!("  projection ({target}): {path}")),
            "path should list the projection for {target}: {stdout}"
        );
    }
}

#[test]
fn human_history_lists_each_event_with_kind_revision_and_time() {
    let workspace = TestWorkspace::new();
    let home_path = workspace.home_path();
    install_workspace_import(&workspace, &home_path);

    let payload = json_payload(
        &workspace,
        &home_path,
        &["--json", "history", "release-notes"],
    );
    let stdout = human_stdout(&workspace, &home_path, &["history", "release-notes"]);

    assert!(
        stdout.contains("Showing "),
        "history should keep its summary line: {stdout}"
    );
    let entries = payload["data"]["entries"]
        .as_array()
        .expect("entries exist")
        .to_vec();
    assert!(!entries.is_empty(), "the install must record history");

    for entry in &entries {
        let kind = entry["kind"].as_str().expect("kind is text");
        let occurred_at = entry["occurred_at"].as_str().expect("occurred_at is text");
        let skill_id = entry["skill_id"].as_str().expect("skill_id is text");
        assert!(
            stdout.contains(&format!("  {occurred_at}  {kind}  {skill_id}")),
            "history should list the {kind} event at {occurred_at}: {stdout}"
        );
        if let Some(revision) = entry["details"]["resolved_revision"].as_str() {
            assert!(
                stdout.contains(revision),
                "history should show the revision of the {kind} event: {stdout}"
            );
        }
    }
}

#[test]
fn quiet_still_hides_the_detail_lines_and_json_stays_unchanged() {
    let workspace = TestWorkspace::new();
    let home_path = workspace.home_path();
    install_workspace_import(&workspace, &home_path);

    for args in [
        vec!["--quiet", "list"],
        vec!["--quiet", "path", "release-notes"],
        vec!["--quiet", "history", "release-notes"],
    ] {
        assert_eq!(
            human_stdout(&workspace, &home_path, &args),
            String::new(),
            "--quiet must suppress summary and detail lines for {:?}",
            args
        );
    }

    let payload = json_payload(&workspace, &home_path, &["--json", "list"]);
    let mut keys = payload
        .as_object()
        .expect("payload is an object")
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    keys.sort();
    assert_eq!(
        keys,
        vec![
            "command".to_string(),
            "data".to_string(),
            "errors".to_string(),
            "ok".to_string(),
            "warnings".to_string(),
        ],
        "the human detail lines must not leak into the JSON contract"
    );
}
