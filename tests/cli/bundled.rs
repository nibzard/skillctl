use super::*;

/// Run skillctl in a workspace and return the JSON response body.
///
/// The assertion demands non-empty stdout so a silent run fails loudly.
fn run_json(workspace: &TestWorkspace, args: &[&str]) -> (assert_cmd::assert::Assert, Value) {
    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(args)
        .assert()
        .stdout(predicate::str::is_empty().not());
    let body: Value =
        serde_json::from_slice(&assert.get_output().stdout).expect("stdout is valid json");
    (assert, body)
}

/// A workspace whose committed import uses the bundled skill's own name.
///
/// This mirrors this repository's `.agents/skillctl.yaml`, where a workspace
/// import named `skillctl` coexists with the bundled user-scope skill that
/// every command bootstraps.
struct BundledNameCollision {
    workspace: TestWorkspace,
}

impl BundledNameCollision {
    fn new() -> Self {
        let workspace = TestWorkspace::new();
        workspace.write_manifest(concat!(
            "version: 1\n",
            "\n",
            "targets:\n",
            "  - codex\n",
            "  - gemini-cli\n",
            "  - opencode\n",
        ));
        workspace.write_skill_source("demo-source", "skillctl");

        run_json(&workspace, &["--json", "telemetry", "status"]);
        run_json(
            &workspace,
            &[
                "--json",
                "--no-input",
                "--name",
                "skillctl",
                "--scope",
                "workspace",
                "install",
                "demo-source",
            ],
        );

        Self { workspace }
    }

    fn home_path(&self) -> PathBuf {
        self.workspace.home_path()
    }

    fn manifest(&self) -> String {
        fs::read_to_string(self.workspace.path().join(".agents/skillctl.yaml"))
            .expect("manifest exists")
    }

    /// Copy the committed control files into a checkout that holds no local
    /// install record, which is the state a fresh clone is in.
    fn clone_portable_state(&self) -> TestWorkspace {
        let checkout = TestWorkspace::new();
        for relative in [".agents/skillctl.yaml", ".agents/skillctl.lock"] {
            let contents =
                fs::read_to_string(self.workspace.path().join(relative)).unwrap_or_else(|error| {
                    panic!("{relative} is readable: {error}");
                });
            checkout.write_file(relative, &contents);
        }
        checkout
    }
}

/// First error message of a JSON response.
fn first_error(body: &Value) -> String {
    body["errors"][0].as_str().unwrap_or_default().to_string()
}

#[test]
fn unscoped_lifecycle_commands_refuse_a_name_installed_in_both_scopes() {
    let fixture = BundledNameCollision::new();
    let workspace = &fixture.workspace;
    let home_path = fixture.home_path();

    // Disable and override already refuse the ambiguous name, so remove and
    // enable must report the same resolution error instead of guessing the
    // bundled user-scope skill.
    for command in ["disable", "enable", "override", "remove"] {
        let (assert, body) = run_json(workspace, &["--json", command, "skillctl"]);
        assert.failure().code(3).stderr(predicate::str::is_empty());
        assert_eq!(body["ok"], false, "{command} should fail: {body:#?}");
        assert!(
            first_error(&body).contains("exists in multiple scopes; re-run with --scope"),
            "expected the shared scope-resolution error from {command}: {body:#?}",
        );
    }

    assert!(
        home_path.join(".agents/skills/skillctl/SKILL.md").is_file(),
        "the bundled user-scope skill must survive the refused commands",
    );
    assert!(
        home_path.join(".claude/skills/skillctl/SKILL.md").is_file(),
        "the bundled user-scope skill must survive the refused commands",
    );
    assert!(
        fixture.manifest().contains("id: skillctl"),
        "the workspace import must stay declared: {}",
        fixture.manifest(),
    );

    let connection =
        Connection::open(home_path.join(".skillctl/state.db")).expect("state database opens");
    let bundled_records: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM install_records WHERE scope = ?1 AND skill_id = ?2",
            params!["user", "skillctl"],
            |row| row.get(0),
        )
        .expect("bundled install count query succeeds");
    assert_eq!(
        bundled_records, 1,
        "the refused remove must keep the bundled install record",
    );
}

#[test]
fn remove_honors_the_scope_selector_for_the_bundled_name_collision() {
    let fixture = BundledNameCollision::new();
    let workspace = &fixture.workspace;
    let home_path = fixture.home_path();

    let (assert, body) = run_json(
        workspace,
        &["--json", "--scope", "user", "remove", "skillctl"],
    );
    assert.success().stderr(predicate::str::is_empty());
    assert_eq!(body["ok"], true, "user-scope remove failed: {body:#?}");
    assert_eq!(body["data"]["scope"], "user");
    assert_eq!(body["data"]["builtin"], true);
    assert!(
        !home_path.join(".agents/skills/skillctl").exists(),
        "the bundled user-scope skill should be pruned",
    );
    assert!(
        fixture.manifest().contains("id: skillctl"),
        "the workspace import must stay declared: {}",
        fixture.manifest(),
    );

    let (assert, body) = run_json(
        workspace,
        &["--json", "--scope", "workspace", "remove", "skillctl"],
    );
    let exit_code = assert.get_output().status.code().expect("exit code exists");
    assert!(
        matches!(exit_code, 0 | 2),
        "workspace-scope remove should succeed, possibly with warnings: {body:#?}",
    );
    assert_eq!(body["ok"], true, "workspace-scope remove failed: {body:#?}",);
    assert_eq!(body["data"]["scope"], "workspace");
    assert!(
        !fixture.manifest().contains("id: skillctl"),
        "the workspace import should be gone: {}",
        fixture.manifest(),
    );
}

#[test]
fn unscoped_remove_refuses_a_name_only_the_manifest_claims() {
    let fixture = BundledNameCollision::new();
    let checkout = fixture.clone_portable_state();

    // Any command bootstraps the bundled skill, so this checkout holds the
    // bundled user-scope install and a declared but unrestored import.
    run_json(&checkout, &["--json", "telemetry", "status"]);
    let home_path = checkout.home_path();

    let (assert, body) = run_json(&checkout, &["--json", "remove", "skillctl"]);
    assert.failure().code(3).stderr(predicate::str::is_empty());
    assert_eq!(body["ok"], false, "remove should fail: {body:#?}");
    assert!(
        first_error(&body).contains("exists in multiple scopes; re-run with --scope"),
        "expected the shared scope-resolution error: {body:#?}",
    );
    assert!(
        home_path.join(".agents/skills/skillctl/SKILL.md").is_file(),
        "the bundled user-scope skill must survive the refused remove",
    );

    // The scope selector still reaches the declared import.
    let (assert, body) = run_json(
        &checkout,
        &["--json", "--scope", "workspace", "remove", "skillctl"],
    );
    let exit_code = assert.get_output().status.code().expect("exit code exists");
    assert!(
        matches!(exit_code, 0 | 2),
        "workspace-scope remove should succeed, possibly with warnings: {body:#?}",
    );
    assert_eq!(body["ok"], true, "workspace-scope remove failed: {body:#?}",);
    assert!(
        home_path.join(".agents/skills/skillctl/SKILL.md").is_file(),
        "removing the workspace import must leave the bundled skill alone",
    );
}

#[test]
fn unscoped_remove_refuses_the_bundled_name_when_only_a_stale_pin_claims_it() {
    let fixture = BundledNameCollision::new();
    let checkout = fixture.clone_portable_state();

    // Drop the manifest import by hand, so the lockfile pin the checkout still
    // carries is the stale-lockfile-entry state remove clears.
    let manifest = fs::read_to_string(checkout.path().join(".agents/skillctl.yaml"))
        .expect("checkout manifest exists");
    let start = manifest
        .find("imports:")
        .expect("manifest declares imports");
    let end = manifest.find("targets:").expect("manifest lists targets");
    checkout.write_file(
        ".agents/skillctl.yaml",
        &format!("{}{}", &manifest[..start], &manifest[end..]),
    );

    run_json(&checkout, &["--json", "telemetry", "status"]);
    let home_path = checkout.home_path();

    // The bundled skill shares the stale entry's name, so remove must not
    // guess it and silently leave the pin behind.
    let (assert, body) = run_json(&checkout, &["--json", "remove", "skillctl"]);
    assert.failure().code(3).stderr(predicate::str::is_empty());
    assert_eq!(body["ok"], false, "remove should fail: {body:#?}");
    let message = first_error(&body);
    assert!(
        message.contains("bundled user-scope skill")
            && message.contains("lockfile entry the workspace manifest does not declare"),
        "the refusal should name both targets: {message}",
    );
    assert!(
        message.contains("--scope workspace"),
        "the refusal should name the selector that clears the entry: {message}",
    );
    assert!(
        home_path.join(".agents/skills/skillctl/SKILL.md").is_file(),
        "the bundled user-scope skill must survive the refused remove",
    );
    assert!(
        fs::read_to_string(checkout.path().join(".agents/skillctl.lock"))
            .expect("checkout lockfile exists")
            .contains("imports:"),
        "the stale pin must survive the refused remove",
    );

    // The selector the refusal names clears the entry and leaves the bundled
    // skill installed.
    let (assert, body) = run_json(
        &checkout,
        &["--json", "--scope", "workspace", "remove", "skillctl"],
    );
    let exit_code = assert.get_output().status.code().expect("exit code exists");
    assert!(
        matches!(exit_code, 0 | 2),
        "workspace-scope remove should succeed, possibly with warnings: {body:#?}",
    );
    assert_eq!(body["ok"], true, "workspace-scope remove failed: {body:#?}",);
    assert_eq!(body["data"]["resolved_from"], "lockfile-only");
    assert!(
        !fs::read_to_string(checkout.path().join(".agents/skillctl.lock"))
            .expect("checkout lockfile exists")
            .contains("imports:"),
        "the stale pin should be cleared",
    );
    assert!(
        home_path.join(".agents/skills/skillctl/SKILL.md").is_file(),
        "clearing the pin must leave the bundled skill alone",
    );
}

#[test]
fn disable_refuses_the_bundled_name_in_a_lockfile_only_checkout() {
    let fixture = BundledNameCollision::new();
    let checkout = fixture.clone_portable_state();

    // Any command bootstraps the bundled skill, so this checkout holds the
    // bundled user-scope install next to a declared but unrestored import that
    // shares its name and owns the only 'skillctl' lockfile entry.
    run_json(&checkout, &["--json", "telemetry", "status"]);
    let home_path = checkout.home_path();

    // The unscoped name stays ambiguous, exactly as remove treats it.
    let (assert, body) = run_json(&checkout, &["--json", "disable", "skillctl"]);
    assert.failure().code(3).stderr(predicate::str::is_empty());
    assert_eq!(body["ok"], false, "unscoped disable should fail: {body:#?}");
    assert!(
        first_error(&body).contains("exists in multiple scopes; re-run with --scope"),
        "expected the shared scope-resolution error: {body:#?}",
    );

    // The bundled skill is locally owned, so disable refuses it by name instead
    // of pairing the bundled install with the workspace import's lockfile
    // entry and reporting a stale plan.
    let (assert, body) = run_json(
        &checkout,
        &["--json", "--scope", "user", "disable", "skillctl"],
    );
    assert.failure().code(3).stderr(predicate::str::is_empty());
    assert_eq!(
        body["ok"], false,
        "user-scope disable should fail: {body:#?}"
    );
    let message = first_error(&body);
    assert!(
        message.contains("bundled user-scope skill"),
        "the refusal should name the bundled skill: {message}",
    );
    assert!(
        !message.contains("stale"),
        "the refusal must not claim a stale plan: {message}",
    );

    assert!(
        home_path.join(".agents/skills/skillctl/SKILL.md").is_file(),
        "the bundled user-scope skill must survive the refused disable",
    );
    let committed_manifest = fs::read_to_string(checkout.path().join(".agents/skillctl.yaml"))
        .expect("checkout manifest exists");
    assert!(
        committed_manifest.contains("enabled: true"),
        "the workspace import must stay enabled: {committed_manifest}",
    );
    let committed_lockfile = fs::read_to_string(checkout.path().join(".agents/skillctl.lock"))
        .expect("checkout lockfile exists");
    assert!(
        committed_lockfile.contains("imports:"),
        "the workspace import must keep its lockfile pin: {committed_lockfile}",
    );

    // The scope selector still reaches the declared workspace import.
    let (assert, body) = run_json(
        &checkout,
        &["--json", "--scope", "workspace", "disable", "skillctl"],
    );
    let exit_code = assert.get_output().status.code().expect("exit code exists");
    assert!(
        matches!(exit_code, 0 | 2),
        "workspace-scope disable should succeed, possibly with warnings: {body:#?}",
    );
    assert_eq!(
        body["ok"], true,
        "workspace-scope disable failed: {body:#?}",
    );
    assert_eq!(body["data"]["scope"], "workspace");
    let manifest = fs::read_to_string(checkout.path().join(".agents/skillctl.yaml"))
        .expect("checkout manifest exists");
    assert!(
        manifest.contains("enabled: false"),
        "the workspace import should now be disabled: {manifest}",
    );
    assert_eq!(
        fs::read_to_string(checkout.path().join(".agents/skillctl.lock"))
            .expect("checkout lockfile exists"),
        committed_lockfile,
        "disabling the import must keep its lockfile pin",
    );
    assert!(
        home_path.join(".agents/skills/skillctl/SKILL.md").is_file(),
        "disabling the workspace import must leave the bundled skill alone",
    );
}

#[test]
fn override_and_fork_refuse_the_bundled_name_in_a_lockfile_only_checkout() {
    let fixture = BundledNameCollision::new();
    let checkout = fixture.clone_portable_state();

    // Any command bootstraps the bundled skill, so this checkout holds the
    // bundled user-scope install next to a declared but unrestored import that
    // shares its name and owns the only 'skillctl' lockfile entry.
    run_json(&checkout, &["--json", "telemetry", "status"]);
    let home_path = checkout.home_path();

    // The unscoped name stays ambiguous and the explicit user selector names
    // the bundled skill. Both refuse by name instead of pairing the bundled
    // install record with the workspace import's lockfile entry and reporting
    // a stale plan no command can repair.
    for args in [
        vec!["override", "skillctl"],
        vec!["--scope", "user", "override", "skillctl"],
        vec!["fork", "skillctl"],
        vec!["--scope", "user", "fork", "skillctl"],
    ] {
        let mut command = vec!["--json"];
        command.extend(args.iter().copied());
        let (assert, body) = run_json(&checkout, &command);
        assert.failure().code(3).stderr(predicate::str::is_empty());
        assert_eq!(body["ok"], false, "{args:?} should fail: {body:#?}");
        let message = first_error(&body);
        assert!(
            message.contains("bundled user-scope skill")
                && message.contains("is not a managed import"),
            "the refusal should name the bundled skill: {message}",
        );
        assert!(
            !message.contains("stale"),
            "the refusal must not claim a stale plan: {message}",
        );
    }

    // The unscoped refusal keeps the shared scope-resolution sentence and
    // names the selector that reaches the declared import.
    let (_, body) = run_json(&checkout, &["--json", "override", "skillctl"]);
    let message = first_error(&body);
    assert!(
        message.contains("exists in multiple scopes; re-run with --scope")
            && message.contains("'skillctl --scope workspace override skillctl'"),
        "the unscoped refusal should name the workspace alternative: {message}",
    );

    // The user-scope refusal names the exits for the bundled skill itself.
    let (_, body) = run_json(
        &checkout,
        &["--json", "--scope", "user", "fork", "skillctl"],
    );
    let message = first_error(&body);
    assert!(
        message.contains("'skillctl --scope user remove skillctl'")
            && message.contains("'skillctl install <source> --name skillctl --scope workspace'"),
        "the bundled refusal should name the supported alternatives: {message}",
    );

    assert!(
        home_path.join(".agents/skills/skillctl/SKILL.md").is_file(),
        "the bundled user-scope skill must survive the refused commands",
    );
    let committed_manifest = fs::read_to_string(checkout.path().join(".agents/skillctl.yaml"))
        .expect("checkout manifest exists");
    assert!(
        committed_manifest.contains("id: skillctl"),
        "the workspace import must stay declared: {committed_manifest}",
    );
    let committed_lockfile = fs::read_to_string(checkout.path().join(".agents/skillctl.lock"))
        .expect("checkout lockfile exists");
    assert!(
        committed_lockfile.contains("imports:"),
        "the workspace import must keep its lockfile pin: {committed_lockfile}",
    );

    // The workspace selector resolves the declared import, so the refusal names
    // the state that blocks the command: this checkout holds no stored source
    // and no install record for it, which is a different failure than the
    // bundled-name dead end.
    let (assert, body) = run_json(
        &checkout,
        &["--json", "--scope", "workspace", "override", "skillctl"],
    );
    assert.failure().code(3).stderr(predicate::str::is_empty());
    assert_eq!(
        body["ok"], false,
        "workspace-scope override should fail: {body:#?}",
    );
    let message = first_error(&body);
    assert!(
        message.contains("no stored source")
            && message.contains("restore --frozen")
            && !message.contains("stale"),
        "the workspace selector should name the unrestored import: {message}",
    );

    // The bundled record no longer pairs with the import's entry, so a
    // user-scope sync reports the honest unrestored-import state instead of a
    // stale plan.
    let (assert, body) = run_json(&checkout, &["--json", "--scope", "user", "sync"]);
    assert.failure().code(3).stderr(predicate::str::is_empty());
    assert_eq!(body["ok"], false, "user-scope sync should fail: {body:#?}");
    let message = first_error(&body);
    assert!(
        message.contains("restore --frozen") && !message.contains("stale"),
        "sync should name the restore repair instead of a stale plan: {message}",
    );
}

#[test]
fn override_and_fork_reach_the_declared_import_through_the_workspace_selector() {
    let fixture = BundledNameCollision::new();
    let workspace = &fixture.workspace;
    let home_path = fixture.home_path();

    // The bundled skill shares the import's name, so the user selector refuses
    // it by name instead of pairing the bundled record with the import's
    // lockfile entry.
    for args in [
        vec!["--scope", "user", "override", "skillctl"],
        vec!["--scope", "user", "fork", "skillctl"],
    ] {
        let mut command = vec!["--json"];
        command.extend(args.iter().copied());
        let (assert, body) = run_json(workspace, &command);
        assert.failure().code(3).stderr(predicate::str::is_empty());
        assert_eq!(body["ok"], false, "{args:?} should fail: {body:#?}");
        let message = first_error(&body);
        assert!(
            message.contains("bundled user-scope skill"),
            "the refusal should name the bundled skill: {message}",
        );
    }

    // A user-scope sync no longer pairs the bundled record with the import's
    // entry, so it runs instead of reporting a stale plan.
    let (assert, body) = run_json(workspace, &["--json", "--scope", "user", "sync"]);
    let exit_code = assert.get_output().status.code().expect("exit code exists");
    assert!(
        matches!(exit_code, 0 | 2),
        "user-scope sync should succeed, possibly with warnings: {body:#?}",
    );
    assert_eq!(body["ok"], true, "user-scope sync failed: {body:#?}");

    // The workspace selector reaches the declared import, not the bundled
    // skill: the overlay lands in the workspace overlay root.
    let (assert, body) = run_json(
        workspace,
        &["--json", "--scope", "workspace", "override", "skillctl"],
    );
    let exit_code = assert.get_output().status.code().expect("exit code exists");
    assert!(
        matches!(exit_code, 0 | 2),
        "workspace-scope override should succeed, possibly with warnings: {body:#?}",
    );
    assert_eq!(
        body["ok"], true,
        "workspace-scope override failed: {body:#?}",
    );
    assert_eq!(body["data"]["scope"], "workspace");
    assert_eq!(body["data"]["overlay_root"], ".agents/overlays/skillctl");
    assert!(
        workspace
            .path()
            .join(".agents/overlays/skillctl/SKILL.md")
            .is_file(),
        "the overlay must be created for the declared import",
    );
    assert!(
        home_path.join(".agents/skills/skillctl/SKILL.md").is_file(),
        "the bundled user-scope skill must stay installed",
    );

    // Forking through the workspace selector takes the declared import into
    // local ownership and leaves the bundled skill alone.
    let (assert, body) = run_json(
        workspace,
        &["--json", "--scope", "workspace", "fork", "skillctl"],
    );
    let exit_code = assert.get_output().status.code().expect("exit code exists");
    assert!(
        matches!(exit_code, 0 | 2),
        "workspace-scope fork should succeed, possibly with warnings: {body:#?}",
    );
    assert_eq!(body["ok"], true, "workspace-scope fork failed: {body:#?}");
    assert!(
        workspace
            .path()
            .join(".agents/skills/skillctl/SKILL.md")
            .is_file(),
        "the forked copy must land in canonical workspace skills",
    );
    assert!(
        !fixture.manifest().contains("id: skillctl"),
        "the fork must remove the managed import: {}",
        fixture.manifest(),
    );
    assert!(
        home_path.join(".agents/skills/skillctl/SKILL.md").is_file(),
        "forking the import must leave the bundled skill alone",
    );
}

#[test]
fn override_and_fork_refuse_the_bundled_skill_without_a_name_collision() {
    let workspace = TestWorkspace::new();
    workspace.write_manifest(concat!("version: 1\n", "\n", "targets:\n", "  - codex\n",));
    workspace.write_file(".agents/skillctl.lock", MINIMAL_LOCKFILE.as_str());
    let home_path = workspace.home_path();

    run_json(&workspace, &["--json", "telemetry", "status"]);

    for args in [
        vec!["override", "skillctl"],
        vec!["--scope", "user", "override", "skillctl"],
        vec!["fork", "skillctl"],
        vec!["--scope", "user", "fork", "skillctl"],
    ] {
        let mut command = vec!["--json"];
        command.extend(args.iter().copied());
        let (assert, body) = run_json(&workspace, &command);
        assert.failure().code(3).stderr(predicate::str::is_empty());
        assert_eq!(body["ok"], false, "{args:?} should fail: {body:#?}");
        let message = first_error(&body);
        assert!(
            message.contains("bundled user-scope skill")
                && message.contains("is not a managed import"),
            "the refusal should name the bundled skill: {message}",
        );
        assert!(
            !message.contains("stale"),
            "the refusal must not claim a stale plan: {message}",
        );
    }

    assert!(
        home_path.join(".agents/skills/skillctl/SKILL.md").is_file(),
        "the bundled user-scope skill must survive the refused commands",
    );
}

#[test]
fn override_fork_and_enable_refuse_the_bundled_name_when_only_a_stale_pin_claims_it() {
    let fixture = BundledNameCollision::new();
    let checkout = fixture.clone_portable_state();

    // Drop the manifest import by hand, so the lockfile pin the checkout still
    // carries is the stale-lockfile-entry state remove clears.
    let manifest = fs::read_to_string(checkout.path().join(".agents/skillctl.yaml"))
        .expect("checkout manifest exists");
    let start = manifest
        .find("imports:")
        .expect("manifest declares imports");
    let end = manifest.find("targets:").expect("manifest lists targets");
    checkout.write_file(
        ".agents/skillctl.yaml",
        &format!("{}{}", &manifest[..start], &manifest[end..]),
    );

    run_json(&checkout, &["--json", "telemetry", "status"]);
    let home_path = checkout.home_path();

    // Override and fork used to pair the bundled install record with the stale
    // pin's entry and report a stale plan, which no command can repair.
    for args in [
        vec!["override", "skillctl"],
        vec!["--scope", "user", "fork", "skillctl"],
    ] {
        let mut command = vec!["--json"];
        command.extend(args.iter().copied());
        let (assert, body) = run_json(&checkout, &command);
        assert.failure().code(3).stderr(predicate::str::is_empty());
        assert_eq!(body["ok"], false, "{args:?} should fail: {body:#?}");
        let message = first_error(&body);
        assert!(
            message.contains("bundled user-scope skill") && !message.contains("stale"),
            "the refusal should name the bundled skill: {message}",
        );
    }

    // Enabling the bundled skill would report success for a name the stale pin
    // still claims, so enable refuses until a selector resolves the name.
    let (assert, body) = run_json(&checkout, &["--json", "enable", "skillctl"]);
    assert.failure().code(3).stderr(predicate::str::is_empty());
    assert_eq!(body["ok"], false, "enable should fail: {body:#?}");
    let message = first_error(&body);
    assert!(
        message.contains("bundled user-scope skill")
            && message.contains("lockfile entry the workspace manifest does not declare")
            && message.contains("--scope user"),
        "the refusal should name both targets and the selector: {message}",
    );
    assert!(
        fs::read_to_string(checkout.path().join(".agents/skillctl.lock"))
            .expect("checkout lockfile exists")
            .contains("imports:"),
        "the stale pin must survive the refused enable",
    );

    // The selector the refusal names still enables the bundled skill.
    let (assert, body) = run_json(
        &checkout,
        &["--json", "--scope", "user", "enable", "skillctl"],
    );
    let exit_code = assert.get_output().status.code().expect("exit code exists");
    assert!(
        matches!(exit_code, 0 | 2),
        "user-scope enable should succeed, possibly with warnings: {body:#?}",
    );
    assert_eq!(body["ok"], true, "user-scope enable failed: {body:#?}");
    assert!(
        home_path.join(".agents/skills/skillctl/SKILL.md").is_file(),
        "the bundled user-scope skill must stay installed",
    );
}

#[test]
fn disable_refuses_the_bundled_skill_without_a_name_collision() {
    let workspace = TestWorkspace::new();
    let home_path = workspace.home_path();

    run_json(&workspace, &["--json", "telemetry", "status"]);

    for args in [
        vec!["disable", "skillctl"],
        vec!["--scope", "user", "disable", "skillctl"],
    ] {
        let mut command = vec!["--json"];
        command.extend(args.iter().copied());
        let (assert, body) = run_json(&workspace, &command);
        assert.failure().code(3).stderr(predicate::str::is_empty());
        assert_eq!(body["ok"], false, "{args:?} should fail: {body:#?}");
        let message = first_error(&body);
        assert!(
            message.contains("bundled user-scope skill"),
            "the refusal should name the bundled skill: {message}",
        );
    }

    assert!(
        home_path.join(".agents/skills/skillctl/SKILL.md").is_file(),
        "the bundled user-scope skill must survive the refused disable",
    );
}

#[test]
fn bundled_skill_is_installed_in_user_scope_on_first_command() {
    let workspace = TestWorkspace::new();
    let home_path = workspace.home_path();

    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", &home_path)
        .args(["--json", "telemetry", "status"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    let body: Value = serde_json::from_slice(&assert.get_output().stdout).expect("stdout is json");

    assert_eq!(body["command"], "telemetry-status");
    assert!(
        home_path.join(".agents/skills/skillctl/SKILL.md").is_file(),
        "bundled skill should project into the neutral user root",
    );
    assert!(
        home_path.join(".claude/skills/skillctl/SKILL.md").is_file(),
        "bundled skill should project into the claude-compatible user root",
    );
    assert!(
        !home_path
            .join(".config/agents/skills/skillctl/SKILL.md")
            .exists(),
        "amp should share the neutral user root instead of a separate config root",
    );
    assert!(
        !home_path.join(".copilot/skills/skillctl").exists(),
        "copilot should share the neutral user root instead of a copilot-only root",
    );

    let connection =
        Connection::open(home_path.join(".skillctl/state.db")).expect("state database opens");
    let install_row: (String, String, String, String) = connection
        .query_row(
            "SELECT scope, skill_id, source_url, resolved_revision FROM install_records \
             WHERE scope = ?1 AND skill_id = ?2",
            params!["user", "skillctl"],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("bundled install record exists");

    assert_eq!(install_row.0, "user");
    assert_eq!(install_row.1, "skillctl");
    assert_eq!(install_row.2, "builtin://skillctl");
    assert_eq!(install_row.3, env!("CARGO_PKG_VERSION"));
}

#[test]
fn bundled_skill_appears_in_user_scope_explain_doctor_and_tui() {
    let workspace = TestWorkspace::new();
    let home_path = workspace.home_path();

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", &home_path)
        .args(["--json", "telemetry", "status"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    let explain_assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", &home_path)
        .args(["--json", "--scope", "user", "explain", "skillctl"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    let explain_body: Value =
        serde_json::from_slice(&explain_assert.get_output().stdout).expect("stdout is json");

    assert_eq!(explain_body["command"], "explain");
    assert_eq!(explain_body["data"]["skill"], "skillctl");
    assert_eq!(explain_body["data"]["scope"], "user");
    assert_eq!(explain_body["data"]["status"], "selected");
    assert_eq!(explain_body["data"]["winner"]["scope"], "user");
    assert_eq!(explain_body["data"]["winner"]["source_class"], "bundled");
    assert_eq!(
        explain_body["data"]["drift"]["active_projection_matches_winner"],
        true
    );
    assert_eq!(
        explain_body["data"]["drift"]["active_differs_from_pinned_source"],
        false
    );
    let explain_targets = explain_body["data"]["targets"]
        .as_array()
        .expect("explain targets exist");
    assert_eq!(explain_targets.len(), 6);
    for target in [
        "codex",
        "claude-code",
        "github-copilot",
        "gemini-cli",
        "amp",
        "opencode",
    ] {
        assert!(
            explain_targets
                .iter()
                .any(|entry| entry["target"] == target && entry["visible"] == true),
            "expected bundled skill to be visible for {target}: {explain_body:#?}",
        );
    }

    let doctor_assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", &home_path)
        .args(["--json", "--scope", "user", "doctor"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    let doctor_body: Value =
        serde_json::from_slice(&doctor_assert.get_output().stdout).expect("stdout is json");

    assert_eq!(doctor_body["command"], "doctor");
    assert_eq!(
        doctor_body["data"]["summary"]["checked_skill_count"]
            .as_u64()
            .expect("checked skill count is numeric"),
        1,
    );
    assert!(
        doctor_body["data"]["issues"]
            .as_array()
            .expect("doctor issues are an array")
            .is_empty(),
        "unexpected doctor issues: {doctor_body:#?}",
    );

    let tui_assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", &home_path)
        .args(["--json", "--scope", "user", "--name", "skillctl", "tui"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    let tui_body: Value =
        serde_json::from_slice(&tui_assert.get_output().stdout).expect("stdout is json");

    assert_eq!(tui_body["command"], "tui");
    assert_eq!(tui_body["data"]["skills"][0]["skill"], "skillctl");
    assert_eq!(tui_body["data"]["skills"][0]["scope"], "user");
    assert_eq!(
        tui_body["data"]["skills"][0]["visibility"]["status"],
        "selected"
    );
    assert_eq!(
        tui_body["data"]["skills"][0]["visibility"]["winner"]["source_class"],
        "bundled"
    );
    assert_eq!(
        tui_body["data"]["skills"][0]["visibility"]["targets"]
            .as_array()
            .expect("tui targets exist")
            .len(),
        6,
    );
    assert_eq!(
        tui_body["data"]["skills"][0]["visibility"]["drift"]["active_projection_matches_winner"],
        true
    );
}

#[test]
fn unscoped_explain_and_inspect_point_at_the_bundled_user_scope_report() {
    let workspace = TestWorkspace::new();
    let home_path = workspace.home_path();

    // Any command bootstraps the bundled skill, so the name refers to an
    // install the workspace scope this unscoped request reads cannot resolve.
    run_json(&workspace, &["--json", "telemetry", "status"]);

    let explain_assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", &home_path)
        .arg("explain")
        .arg("skillctl")
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    let explain_stdout = String::from_utf8_lossy(&explain_assert.get_output().stdout).into_owned();
    assert!(
        explain_stdout.contains("not currently visible in workspace scope"),
        "the summary must name the scope the report read: {explain_stdout}",
    );
    assert!(
        explain_stdout.contains("bundled skillctl skill is managed at user scope"),
        "the summary must name the scope that owns the name: {explain_stdout}",
    );
    assert!(
        explain_stdout.contains("skillctl --scope user explain skillctl"),
        "the summary must point at the user-scope surface: {explain_stdout}",
    );

    let inspect_assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", &home_path)
        .arg("inspect")
        .arg("skillctl")
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    let inspect_stdout = String::from_utf8_lossy(&inspect_assert.get_output().stdout).into_owned();
    assert!(
        inspect_stdout.contains("workspace scope preflight inspection"),
        "the preflight must name the scope the report read: {inspect_stdout}",
    );
    assert!(
        inspect_stdout.contains("skillctl --scope user explain skillctl"),
        "the preflight must point at the user-scope surface: {inspect_stdout}",
    );

    let (_, explain_body) = run_json(&workspace, &["--json", "explain", "skillctl"]);
    assert_eq!(explain_body["data"]["scope"], "workspace");
    assert_eq!(explain_body["data"]["status"], "missing");
    assert_eq!(explain_body["data"]["owner_scope"], "user");
    assert!(
        explain_body["data"]["winner"].is_null(),
        "no workspace candidate resolves, so no winner may be reported: {explain_body:#?}",
    );
    let targets = explain_body["data"]["targets"]
        .as_array()
        .expect("explain targets exist");
    assert!(
        !targets.is_empty(),
        "the default target set must report the absence per target: {explain_body:#?}",
    );
    for target in targets {
        assert_eq!(
            target["visible"], false,
            "target must stay invisible: {target:#?}"
        );
        assert!(
            target["fix"]
                .as_str()
                .expect("target fix exists")
                .contains("skillctl --scope user explain skillctl"),
            "the per-target fix must name the user-scope surface: {target:#?}",
        );
    }

    let (_, inspect_body) = run_json(&workspace, &["--json", "inspect", "skillctl"]);
    assert_eq!(inspect_body["data"]["scope"], "workspace");
    assert_eq!(inspect_body["data"]["status"], "missing");
    assert_eq!(inspect_body["data"]["owner_scope"], "user");

    // The user-scope surface keeps answering with the bundled report, so the
    // pointer leads to the projections that scope plans.
    let (_, user_body) = run_json(
        &workspace,
        &["--json", "--scope", "user", "explain", "skillctl"],
    );
    assert_eq!(user_body["data"]["scope"], "user");
    assert_eq!(user_body["data"]["status"], "selected");
    assert_eq!(user_body["data"]["winner"]["source_class"], "bundled");
    assert!(
        user_body["data"].get("owner_scope").is_none(),
        "the scope that owns the name needs no pointer: {user_body:#?}",
    );
}

#[test]
fn unscoped_explain_keeps_the_workspace_import_report_for_the_bundled_name() {
    let fixture = BundledNameCollision::new();

    // The workspace import resolves the name, so the unscoped report answers
    // with that winner instead of pointing at the bundled user-scope install.
    let (_, body) = run_json(&fixture.workspace, &["--json", "explain", "skillctl"]);
    assert_eq!(body["data"]["scope"], "workspace");
    assert_eq!(body["data"]["status"], "selected");
    assert_eq!(body["data"]["winner"]["scope"], "workspace");
    assert_eq!(body["data"]["winner"]["source_class"], "imported");
    assert!(
        body["data"].get("owner_scope").is_none(),
        "a name the graph resolves carries no owner pointer: {body:#?}",
    );
}

#[test]
fn unscoped_explain_keeps_a_stale_pin_report_for_the_bundled_name() {
    let fixture = BundledNameCollision::new();
    let checkout = fixture.clone_portable_state();

    // Drop the manifest import by hand, so the lockfile pin the checkout still
    // carries is the stale-lockfile-entry state the report must keep.
    let manifest = fs::read_to_string(checkout.path().join(".agents/skillctl.yaml"))
        .expect("checkout manifest exists");
    let start = manifest
        .find("imports:")
        .expect("manifest declares imports");
    let end = manifest.find("targets:").expect("manifest lists targets");
    checkout.write_file(
        ".agents/skillctl.yaml",
        &format!("{}{}", &manifest[..start], &manifest[end..]),
    );

    run_json(&checkout, &["--json", "telemetry", "status"]);

    let (_, body) = run_json(&checkout, &["--json", "explain", "skillctl"]);
    assert_eq!(body["data"]["status"], "missing");
    assert!(
        body["data"].get("owner_scope").is_none(),
        "a name the workspace still pins must not read as bundled-only: {body:#?}",
    );
    assert!(
        body["data"]["issues"]
            .as_array()
            .expect("issues are an array")
            .iter()
            .any(|issue| issue["code"] == "stale-lockfile-entry"),
        "the stale pin must stay the reported finding: {body:#?}",
    );
}

#[test]
fn tui_does_not_bootstrap_bundled_skill_on_a_fresh_home() {
    let workspace = TestWorkspace::new();
    let home_path = workspace.home_path();

    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", &home_path)
        .args(["--json", "tui"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    let body: Value = serde_json::from_slice(&assert.get_output().stdout).expect("stdout is json");

    assert_eq!(body["command"], "tui");
    assert_eq!(body["ok"], true);
    assert!(
        body["data"]["skills"]
            .as_array()
            .expect("skills array exists")
            .is_empty(),
        "unexpected TUI payload: {body:#?}",
    );
    assert!(
        !home_path.join(".agents/skills/skillctl/SKILL.md").exists(),
        "tui should not bootstrap the bundled skill into the neutral user root",
    );
    assert!(
        !home_path.join(".claude/skills/skillctl/SKILL.md").exists(),
        "tui should not bootstrap the bundled skill into the claude-compatible user root",
    );
    assert!(
        !home_path
            .join(".config/agents/skills/skillctl/SKILL.md")
            .exists(),
        "tui should not bootstrap the bundled skill into the amp-compatible user root",
    );
}

#[test]
fn bundled_skill_conflicts_are_reported_in_explain_doctor_and_tui() {
    let workspace = TestWorkspace::new();
    let home_path = workspace.home_path();
    let custom_skill_root = home_path.join(".claude/skills/skillctl");
    fs::create_dir_all(&custom_skill_root).expect("custom skill root exists");
    fs::write(
        custom_skill_root.join("SKILL.md"),
        concat!(
            "---\n",
            "name: skillctl\n",
            "description: User-managed skillctl variant.\n",
            "---\n",
            "\n",
            "# custom skillctl\n",
        ),
    )
    .expect("custom skill manifest exists");

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", &home_path)
        .args(["--json", "telemetry", "status"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    let explain_assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", &home_path)
        .args(["--json", "--scope", "user", "explain", "skillctl"])
        .assert()
        .code(2)
        .stderr(predicate::str::is_empty());
    let explain_body: Value =
        serde_json::from_slice(&explain_assert.get_output().stdout).expect("stdout is json");

    assert_eq!(explain_body["ok"], true);
    assert_eq!(explain_body["data"]["status"], "selected");
    assert_eq!(
        explain_body["data"]["drift"]["active_projection_matches_winner"],
        false
    );
    assert_eq!(
        explain_body["data"]["drift"]["active_differs_from_pinned_source"],
        true
    );

    let doctor_assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", &home_path)
        .args(["--json", "--scope", "user", "doctor"])
        .assert()
        .code(2)
        .stderr(predicate::str::is_empty());
    let doctor_body: Value =
        serde_json::from_slice(&doctor_assert.get_output().stdout).expect("stdout is json");

    assert!(
        doctor_body["data"]["issues"]
            .as_array()
            .expect("doctor issues are an array")
            .iter()
            .any(|issue| issue["code"] == "bundled-skill-conflict" && issue["skill"] == "skillctl"),
        "expected bundled conflict issue: {doctor_body:#?}",
    );
    assert!(
        doctor_body["data"]["issues"]
            .as_array()
            .expect("doctor issues are an array")
            .iter()
            .any(|issue| issue["code"] == "projection-drift" && issue["skill"] == "skillctl"),
        "expected bundled projection drift issue: {doctor_body:#?}",
    );

    let tui_assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", &home_path)
        .args(["--json", "--scope", "user", "--name", "skillctl", "tui"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    let tui_body: Value =
        serde_json::from_slice(&tui_assert.get_output().stdout).expect("stdout is json");

    assert_eq!(
        tui_body["data"]["skills"][0]["visibility"]["drift"]["active_projection_matches_winner"],
        false
    );
    assert_eq!(
        tui_body["data"]["skills"][0]["visibility"]["drift"]["active_differs_from_pinned_source"],
        true
    );
    assert!(
        tui_body["data"]["skills"][0]["visibility"]["issues"]
            .as_array()
            .expect("tui visibility issues are an array")
            .iter()
            .any(|issue| issue["code"] == "bundled-skill-conflict"),
        "expected bundled conflict issue in TUI visibility: {tui_body:#?}",
    );
}

#[test]
fn bundled_bootstrap_rolls_back_when_the_transaction_fails_after_state_write() {
    let workspace = TestWorkspace::new();

    let snapshot = workspace.snapshot_paths(&[
        "home/.skillctl/state.db",
        "home/.agents/skills",
        "home/.claude/skills",
        "home/.config/agents/skills",
    ]);

    assert_transaction_rolled_back(
        &workspace,
        "bundled-bootstrap:after-state",
        "1770000000",
        &["telemetry", "status"],
        &snapshot,
    );
}

#[test]
fn bundled_skill_removal_is_explicit_and_persists_across_later_runs() {
    let workspace = TestWorkspace::new();
    let home_path = workspace.home_path();

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", &home_path)
        .args(["--json", "telemetry", "status"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    let remove_assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", &home_path)
        .args(["--json", "--scope", "user", "remove", "skillctl"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    let remove_body: Value =
        serde_json::from_slice(&remove_assert.get_output().stdout).expect("stdout is json");

    assert_eq!(remove_body["command"], "remove");
    assert_eq!(remove_body["data"]["skill"], "skillctl");
    assert_eq!(remove_body["data"]["scope"], "user");
    assert!(
        !home_path.join(".agents/skills/skillctl").exists(),
        "explicit removal should prune bundled user projections",
    );
    assert!(
        !home_path.join(".claude/skills/skillctl").exists(),
        "explicit removal should prune bundled user projections",
    );
    assert!(
        !home_path.join(".config/agents/skills/skillctl").exists(),
        "explicit removal should prune bundled user projections",
    );

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", &home_path)
        .args(["--json", "telemetry", "status"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    assert!(
        !home_path.join(".agents/skills/skillctl").exists(),
        "explicit removal should suppress later automatic reinstall",
    );

    let history_assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", &home_path)
        .args(["--json", "--scope", "user", "history", "skillctl"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    let history_body: Value =
        serde_json::from_slice(&history_assert.get_output().stdout).expect("stdout is json");
    let latest_kind = history_body["data"]["entries"][0]["kind"]
        .as_str()
        .expect("history kind exists");
    let latest_reason = history_body["data"]["entries"][0]["details"]["reason"]
        .as_str()
        .expect("cleanup reason exists");

    assert_eq!(latest_kind, "cleanup");
    assert_eq!(latest_reason, "explicit-remove");
}

#[test]
fn bundled_remove_rolls_back_when_the_transaction_fails_after_state_write() {
    let workspace = TestWorkspace::new();
    let home_path = workspace.home_path();

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", &home_path)
        .args(["--json", "telemetry", "status"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    let snapshot = workspace.snapshot_paths(&[
        "home/.skillctl/state.db",
        "home/.agents/skills",
        "home/.claude/skills",
        "home/.config/agents/skills",
    ]);

    assert_transaction_rolled_back(
        &workspace,
        "bundled-remove:after-state",
        "1770001234",
        &["--scope", "user", "remove", "skillctl"],
        &snapshot,
    );
}

#[test]
fn bundled_skill_does_not_overwrite_hand_authored_user_skill_roots() {
    let workspace = TestWorkspace::new();
    let home_path = workspace.home_path();
    let custom_skill_root = home_path.join(".claude/skills/skillctl");
    fs::create_dir_all(&custom_skill_root).expect("custom skill root exists");
    fs::write(
        custom_skill_root.join("SKILL.md"),
        concat!(
            "---\n",
            "name: skillctl\n",
            "description: User-managed skillctl variant.\n",
            "---\n",
            "\n",
            "# custom skillctl\n",
        ),
    )
    .expect("custom skill manifest exists");

    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", &home_path)
        .args(["--json", "doctor"])
        .assert()
        .code(2)
        .stderr(predicate::str::is_empty());
    let body: Value = serde_json::from_slice(&assert.get_output().stdout).expect("stdout is json");

    assert!(
        home_path.join(".agents/skills/skillctl/SKILL.md").is_file(),
        "bundled skill should still install into other compatible roots",
    );
    assert_eq!(
        fs::read_to_string(custom_skill_root.join("SKILL.md")).expect("custom skill remains"),
        concat!(
            "---\n",
            "name: skillctl\n",
            "description: User-managed skillctl variant.\n",
            "---\n",
            "\n",
            "# custom skillctl\n",
        ),
    );
    assert!(
        body["data"]["issues"]
            .as_array()
            .expect("doctor issues is an array")
            .iter()
            .any(|issue| issue["code"] == "bundled-skill-conflict"),
        "doctor should surface the blocked bundled-skill projection",
    );
}

#[test]
fn bundled_enable_rolls_back_when_the_transaction_fails_after_state_write() {
    let workspace = TestWorkspace::new();
    let home_path = workspace.home_path();

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", &home_path)
        .args(["--json", "telemetry", "status"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", &home_path)
        .args(["--json", "--scope", "user", "remove", "skillctl"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    let snapshot = workspace.snapshot_paths(&[
        "home/.skillctl/state.db",
        "home/.agents/skills",
        "home/.claude/skills",
        "home/.config/agents/skills",
    ]);

    assert_transaction_rolled_back(
        &workspace,
        "bundled-enable:after-state",
        "1770002468",
        &["--scope", "user", "enable", "skillctl"],
        &snapshot,
    );
}

#[test]
fn disable_and_enable_toggle_manifest_state_and_projections() {
    let workspace = TestWorkspace::new();
    workspace.write_manifest(concat!(
        "version: 1\n",
        "\n",
        "targets:\n",
        "  - claude-code\n",
    ));
    workspace.write_skill_source("shared-skills", "release-notes");

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .env("SOURCE_DATE_EPOCH", "1770000000")
        .args([
            "--no-input",
            "--name",
            "release-notes",
            "install",
            "shared-skills",
        ])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    let disable_assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .env("SOURCE_DATE_EPOCH", "1770001234")
        .args(["--json", "disable", "release-notes"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    let disable_body: Value =
        serde_json::from_slice(&disable_assert.get_output().stdout).expect("stdout is valid json");

    assert_eq!(disable_body["command"], "disable");
    assert_eq!(disable_body["ok"], true);
    assert_eq!(disable_body["data"]["skill"], "release-notes");
    assert_eq!(disable_body["data"]["scope"], "workspace");
    assert_eq!(disable_body["data"]["enabled"], false);
    assert!(
        fs::read_to_string(workspace.path().join(".agents/skillctl.yaml"))
            .expect("manifest exists")
            .contains("enabled: false"),
    );
    assert!(
        !workspace
            .path()
            .join(".claude/skills/release-notes")
            .exists(),
        "disabled skill should be pruned from generated projections",
    );

    let connection = Connection::open(workspace.home_path().join(".skillctl/state.db"))
        .expect("state database opens");
    let disabled_projection_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM projection_records WHERE skill_id = ?1",
            params!["release-notes"],
            |row| row.get(0),
        )
        .expect("projection count query succeeds");
    assert_eq!(disabled_projection_count, 0);

    let enable_assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .env("SOURCE_DATE_EPOCH", "1770002468")
        .args(["--json", "enable", "release-notes"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    let enable_body: Value =
        serde_json::from_slice(&enable_assert.get_output().stdout).expect("stdout is valid json");

    assert_eq!(enable_body["command"], "enable");
    assert_eq!(enable_body["ok"], true);
    assert_eq!(enable_body["data"]["enabled"], true);
    assert!(
        fs::read_to_string(workspace.path().join(".agents/skillctl.yaml"))
            .expect("manifest exists")
            .contains("enabled: true"),
    );
    assert!(
        workspace
            .path()
            .join(".claude/skills/release-notes/SKILL.md")
            .is_file(),
        "enabled skill should be rematerialized",
    );

    let enabled_projection_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM projection_records WHERE skill_id = ?1",
            params!["release-notes"],
            |row| row.get(0),
        )
        .expect("projection count query succeeds");
    assert_eq!(enabled_projection_count, 1);
}

#[test]
fn disable_rolls_back_when_the_transaction_fails_after_state_write() {
    let workspace = TestWorkspace::new();
    workspace.write_manifest(concat!(
        "version: 1\n",
        "\n",
        "targets:\n",
        "  - claude-code\n",
    ));
    workspace.write_skill_source("shared-skills", "release-notes");

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .env("SOURCE_DATE_EPOCH", "1770000000")
        .args([
            "--no-input",
            "--name",
            "release-notes",
            "install",
            "shared-skills",
        ])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    let snapshot = workspace.snapshot_paths(&[
        ".agents/skillctl.yaml",
        "home/.skillctl/state.db",
        ".claude/skills",
    ]);

    assert_transaction_rolled_back(
        &workspace,
        "disable:after-state",
        "1770001234",
        &["disable", "release-notes"],
        &snapshot,
    );
}

#[test]
fn enable_rolls_back_when_the_transaction_fails_after_state_write() {
    let workspace = TestWorkspace::new();
    workspace.write_manifest(concat!(
        "version: 1\n",
        "\n",
        "targets:\n",
        "  - claude-code\n",
    ));
    workspace.write_skill_source("shared-skills", "release-notes");

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .env("SOURCE_DATE_EPOCH", "1770000000")
        .args([
            "--no-input",
            "--name",
            "release-notes",
            "install",
            "shared-skills",
        ])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .env("SOURCE_DATE_EPOCH", "1770001234")
        .args(["disable", "release-notes"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    let snapshot = workspace.snapshot_paths(&[
        ".agents/skillctl.yaml",
        "home/.skillctl/state.db",
        ".claude/skills",
    ]);

    assert_transaction_rolled_back(
        &workspace,
        "enable:after-state",
        "1770002468",
        &["enable", "release-notes"],
        &snapshot,
    );
}
