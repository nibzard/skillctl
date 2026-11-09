use super::*;

/// Portable state one teammate commits so another checkout restores the same
/// content.
///
/// The generated copy in the reused canonical root travels with it in the
/// repositories that commit `.agents/skills`, so the clone carries that too.
const PORTABLE_STATE: [&str; 3] = [
    ".agents/skillctl.yaml",
    ".agents/skillctl.lock",
    ".agents/skills",
];

/// A checkout that holds the committed manifest and lockfile, a fresh local
/// state store, and one import whose source cannot be reconstructed here.
///
/// That is the state a fresh clone of a repository with a nonportable import
/// is in. `restore --frozen` cannot acquire the source, and before this repair
/// path existed neither `remove` nor `install` could act on the import, so the
/// only way out was editing both files by hand.
struct LockfileOnlyCheckout {
    /// Author workspace that owns the portable replacement source. It stays
    /// alive for as long as the checkout needs to clone from it.
    #[allow(dead_code)]
    author: TestWorkspace,
    checkout: TestWorkspace,
    replacement_url: String,
}

impl LockfileOnlyCheckout {
    fn new() -> Self {
        let author = TestWorkspace::new();
        author.write_manifest(concat!(
            "version: 1\n",
            "\n",
            "targets:\n",
            "  - codex\n",
            "  - gemini-cli\n",
            "  - opencode\n",
        ));
        author.write_skill_source("demo-source", "release-notes");
        install(&author, &author.home_path(), "demo-source");

        // The demo source was a scratch directory that no longer exists by the
        // time anyone clones the repository, so the import cannot be restored.
        fs::remove_dir_all(author.path().join("demo-source"))
            .expect("the demo source directory is removed");

        // A portable Git source the import can be replaced with.
        author.write_skill_source("portable-source", "release-notes");
        author.init_git_repo("portable-source");
        let replacement_url = author.git_repo_url("portable-source");

        let checkout = TestWorkspace::new();
        clone_portable_state(&author, &checkout);

        Self {
            author,
            checkout,
            replacement_url,
        }
    }

    /// A checkout whose lockfile still pins the import while the committed
    /// manifest no longer declares it.
    ///
    /// That is the `stale-lockfile-entry` state `doctor` and `explain`
    /// report. Before `remove` learned to clear the pin directly, the only
    /// repair was reinstalling the import so a second `remove` had a manifest
    /// import to act on.
    fn with_stale_lockfile_entry() -> Self {
        let fixture = Self::new();
        let manifest = fs::read_to_string(fixture.checkout.path().join(".agents/skillctl.yaml"))
            .expect("manifest exists");
        let start = manifest
            .find("imports:")
            .expect("manifest declares imports");
        let end = manifest.find("targets:").expect("manifest lists targets");
        fixture.checkout.write_file(
            ".agents/skillctl.yaml",
            &format!("{}{}", &manifest[..start], &manifest[end..]),
        );
        fixture
    }

    /// Drop the manifest import of an author workspace in place, so its
    /// install record survives while the manifest stops declaring the import.
    fn without_manifest_import(self) -> Self {
        let manifest = fs::read_to_string(self.author.path().join(".agents/skillctl.yaml"))
            .expect("manifest exists");
        let start = manifest
            .find("imports:")
            .expect("manifest declares imports");
        let end = manifest.find("targets:").expect("manifest lists targets");
        self.author.write_file(
            ".agents/skillctl.yaml",
            &format!("{}{}", &manifest[..start], &manifest[end..]),
        );
        self
    }

    /// Whether one issue code of a JSON response body is present.
    fn reports_issue(body: &Value, code: &str) -> bool {
        body["data"]["issues"]
            .as_array()
            .unwrap_or_else(|| panic!("issues exist: {body:#?}"))
            .iter()
            .any(|issue| issue["code"] == code)
    }

    /// Run skillctl in the checkout with the given home directory.
    fn run(&self, home: &Path, args: &[&str]) -> assert_cmd::assert::Assert {
        Command::cargo_bin("skillctl")
            .expect("binary exists")
            .current_dir(self.checkout.path())
            .env("HOME", home)
            .args(args)
            .assert()
    }

    /// Run skillctl in the checkout and return the JSON response body.
    fn run_json(&self, home: &Path, args: &[&str]) -> (assert_cmd::assert::Assert, Value) {
        let mut json_args = vec!["--json"];
        json_args.extend_from_slice(args);
        let assert = self
            .run(home, &json_args)
            .stdout(predicate::str::is_empty().not());
        let body: Value =
            serde_json::from_slice(&assert.get_output().stdout).expect("stdout is valid json");
        (assert, body)
    }

    /// Run skillctl as JSON in any workspace with the given home directory.
    fn run_json_in(
        workspace: &TestWorkspace,
        home: &Path,
        args: &[&str],
    ) -> (assert_cmd::assert::Assert, Value) {
        let assert = Command::cargo_bin("skillctl")
            .expect("binary exists")
            .current_dir(workspace.path())
            .env("HOME", home)
            .args(args)
            .assert()
            .stdout(predicate::str::is_empty().not());
        let body: Value =
            serde_json::from_slice(&assert.get_output().stdout).expect("stdout is valid json");
        (assert, body)
    }

    /// First error message of a JSON response.
    fn first_error(body: &Value) -> String {
        body["errors"][0].as_str().unwrap_or_default().to_string()
    }

    /// Whether any warning of a JSON response contains the given text.
    fn warnings_contain(body: &Value, text: &str) -> bool {
        body["warnings"]
            .as_array()
            .expect("warnings exist")
            .iter()
            .any(|warning| warning.as_str().unwrap_or_default().contains(text))
    }

    fn manifest(&self) -> String {
        fs::read_to_string(self.checkout.path().join(".agents/skillctl.yaml"))
            .expect("manifest exists")
    }

    fn generated_copy_exists(&self) -> bool {
        self.checkout
            .path()
            .join(".agents/skills/release-notes")
            .exists()
    }
}

fn install(workspace: &TestWorkspace, home: &Path, source: &str) -> Value {
    install_named(workspace, home, source, "release-notes")
}

fn install_named(workspace: &TestWorkspace, home: &Path, source: &str, name: &str) -> Value {
    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", home)
        .args(["--json", "--no-input", "--name", name, "install", source])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    let body: Value =
        serde_json::from_slice(&assert.get_output().stdout).expect("stdout is valid json");
    assert_eq!(body["ok"], true, "install failed: {body:#?}");
    body
}

/// Copy the portable state of one workspace into a fresh checkout.
fn clone_portable_state(from: &TestWorkspace, to: &TestWorkspace) {
    for relative in PORTABLE_STATE {
        let source = from.path().join(relative);
        if source.is_dir() {
            to.copy_path(&source, relative);
        } else if source.is_file() {
            let contents = fs::read_to_string(&source)
                .unwrap_or_else(|error| panic!("{relative} is readable: {error}"));
            to.write_file(relative, &contents);
        }
    }
}

#[test]
fn a_lockfile_only_checkout_removes_an_import_it_cannot_restore() {
    let fixture = LockfileOnlyCheckout::new();
    let home = fixture.checkout.home_path();

    // The checkout cannot reconstruct the import, which is the state that
    // makes removal necessary.
    fixture
        .run(&home, &["restore", "--frozen"])
        .failure()
        .code(3);

    let (assert, body) = fixture.run_json(&home, &["remove", "release-notes"]);
    assert.code(2).stderr(predicate::str::is_empty());
    assert_eq!(body["ok"], true, "remove failed: {body:#?}");
    assert_eq!(body["data"]["skill"], "release-notes");
    assert_eq!(
        body["data"]["resolved_from"], "manifest-and-lockfile",
        "the remove should report that it acted on the committed state: {body:#?}",
    );
    assert!(
        LockfileOnlyCheckout::warnings_contain(&body, "no install record for 'release-notes'"),
        "expected a warning that names the missing install record: {body:#?}"
    );

    let manifest = fixture.manifest();
    assert!(
        !manifest.contains("id: release-notes"),
        "the manifest should no longer declare the import: {manifest}"
    );
    let lockfile = fixture.checkout.read_lockfile_yaml();
    assert!(
        lockfile["imports"]["release-notes"].is_null(),
        "the lockfile should no longer pin the import"
    );
    assert!(
        !fixture.generated_copy_exists(),
        "the generated copy in the reused canonical root should be pruned"
    );

    // The checkout is usable again without editing either file by hand.
    fixture.run(&home, &["sync"]).success();
    let (_, verify) = fixture.run_json(&home, &["verify"]);
    assert_eq!(verify["ok"], true, "verify failed: {verify:#?}");

    // A second remove has nothing left to act on in any layer.
    let (assert, body) = fixture.run_json(&home, &["remove", "release-notes"]);
    assert.failure().code(3);
    assert_eq!(body["ok"], false, "remove should fail: {body:#?}");
}

#[test]
fn a_lockfile_only_checkout_replaces_a_nonrestorable_import_with_a_portable_source() {
    let fixture = LockfileOnlyCheckout::new();
    let home = fixture.checkout.home_path();

    // Installing over a declared import is refused and names the repair.
    let (assert, body) = fixture.run_json(
        &home,
        &[
            "install",
            fixture.replacement_url.as_str(),
            "--name",
            "release-notes",
        ],
    );
    assert.failure().code(3);
    assert!(
        LockfileOnlyCheckout::first_error(&body)
            .contains("already managed by import 'release-notes'"),
        "expected a conflict naming the import: {body:#?}"
    );
    assert!(
        LockfileOnlyCheckout::first_error(&body).contains("skillctl remove release-notes"),
        "the refusal should name the repair command: {body:#?}"
    );

    let (_, remove) = fixture.run_json(&home, &["remove", "release-notes"]);
    assert_eq!(remove["ok"], true, "remove failed: {remove:#?}");

    install(&fixture.checkout, &home, &fixture.replacement_url);

    let manifest = fixture.manifest();
    assert!(
        manifest.contains("type: git"),
        "the replacement import should be a Git source: {manifest}"
    );
    assert!(
        manifest.contains("url: ") && !manifest.contains("demo-source"),
        "the replacement import should not keep the removed source: {manifest}"
    );

    fixture.run(&home, &["sync"]).success();
    fixture.run(&home, &["restore", "--frozen"]).success();
    let (_, verify) = fixture.run_json(&home, &["verify"]);
    assert_eq!(verify["ok"], true, "verify failed: {verify:#?}");
}

#[test]
fn remove_still_refuses_a_skill_no_layer_declares() {
    let fixture = LockfileOnlyCheckout::new();
    let home = fixture.checkout.home_path();

    let (assert, body) = fixture.run_json(&home, &["remove", "unknown-skill"]);
    assert.failure().code(3);
    assert_eq!(body["ok"], false, "remove should fail: {body:#?}");

    let message = LockfileOnlyCheckout::first_error(&body);
    assert!(
        message.contains("unknown-skill"),
        "expected the failure to name the skill: {message}"
    );
}

#[test]
fn remove_from_a_lockfile_only_checkout_honors_the_scope_selector() {
    let fixture = LockfileOnlyCheckout::new();
    let home = fixture.checkout.home_path();

    let (assert, body) = fixture.run_json(&home, &["--scope", "user", "remove", "release-notes"]);
    assert.failure().code(3);
    assert_eq!(body["ok"], false, "remove should fail: {body:#?}");
    assert!(
        fixture.manifest().contains("id: release-notes"),
        "the workspace import should stay untouched: {}",
        fixture.manifest()
    );
}

#[test]
fn a_lockfile_only_checkout_removes_every_unrestored_import_in_turn() {
    let author = TestWorkspace::new();
    author.write_manifest(concat!(
        "version: 1\n",
        "\n",
        "targets:\n",
        "  - codex\n",
        "  - gemini-cli\n",
        "  - opencode\n",
    ));
    author.write_skill_source("demo-source", "release-notes");
    author.write_skill_source_at(
        "demo-source",
        ".agents/skills/triage",
        "triage",
        "Triage incoming issues.",
    );
    install_named(&author, &author.home_path(), "demo-source", "release-notes");
    install_named(&author, &author.home_path(), "demo-source", "triage");

    fs::remove_dir_all(author.path().join("demo-source"))
        .expect("the demo source directory is removed");

    let checkout = TestWorkspace::new();
    clone_portable_state(&author, &checkout);
    let home = checkout.home_path();

    // Removing the first import leaves the second one unresolved, so the
    // projection rebuild is skipped and reported instead of refusing the
    // removal.
    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(checkout.path())
        .env("HOME", &home)
        .args(["--json", "remove", "release-notes"])
        .assert()
        .code(2)
        .stderr(predicate::str::is_empty());
    let body: Value =
        serde_json::from_slice(&assert.get_output().stdout).expect("stdout is valid json");
    assert_eq!(body["ok"], true, "remove failed: {body:#?}");
    assert_eq!(
        body["data"]["projection"]["skipped"], true,
        "the skipped projection rebuild should be reported: {body:#?}"
    );
    assert_eq!(
        body["data"]["projection"]["unrestored_imports"],
        json!(["triage"]),
        "the report should name the import that blocked resolution: {body:#?}"
    );
    assert!(
        body["warnings"]
            .as_array()
            .expect("warnings exist")
            .iter()
            .any(|warning| warning
                .as_str()
                .unwrap_or_default()
                .contains("enabled import 'triage' has no stored source")),
        "expected a warning that names the unresolved import: {body:#?}"
    );
    assert!(
        checkout.read_lockfile_yaml()["imports"]["release-notes"].is_null(),
        "the removed import should leave the lockfile"
    );
    assert!(
        !checkout.read_lockfile_yaml()["imports"]["triage"].is_null(),
        "the unresolved import should stay pinned until it is removed"
    );

    // The last import removes cleanly and leaves a workspace sync can rebuild.
    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(checkout.path())
        .env("HOME", &home)
        .args(["--json", "remove", "triage"])
        .assert()
        .code(2)
        .stderr(predicate::str::is_empty());
    let body: Value =
        serde_json::from_slice(&assert.get_output().stdout).expect("stdout is valid json");
    assert_eq!(body["ok"], true, "remove failed: {body:#?}");

    let manifest =
        fs::read_to_string(checkout.path().join(".agents/skillctl.yaml")).expect("manifest exists");
    assert!(
        !manifest.contains("id: triage") && !manifest.contains("id: release-notes"),
        "no import should remain: {manifest}"
    );

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(checkout.path())
        .env("HOME", &home)
        .args(["sync"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
}

#[test]
fn a_lockfile_only_checkout_disables_and_re_enables_an_import_it_cannot_restore() {
    let fixture = LockfileOnlyCheckout::new();
    let home = fixture.checkout.home_path();

    // The checkout cannot reconstruct the import, which is the state that
    // leaves a disable as the only way to stop projecting it.
    fixture
        .run(&home, &["restore", "--frozen"])
        .failure()
        .code(3);

    let (assert, body) = fixture.run_json(&home, &["disable", "release-notes"]);
    assert.code(2).stderr(predicate::str::is_empty());
    assert_eq!(body["ok"], true, "disable failed: {body:#?}");
    assert_eq!(body["data"]["skill"], "release-notes");
    assert_eq!(body["data"]["enabled"], false, "disable failed: {body:#?}");
    assert_eq!(
        body["data"]["resolved_from"], "manifest-and-lockfile",
        "the disable should report that it acted on the committed state: {body:#?}",
    );
    assert!(
        LockfileOnlyCheckout::warnings_contain(&body, "no install record for 'release-notes'"),
        "expected a warning that names the missing install record: {body:#?}"
    );

    // The pin survives, so the checkout stays reproducible once a portable
    // source replaces the lost one.
    let manifest = fixture.manifest();
    assert!(
        manifest.contains("enabled: false"),
        "the manifest should keep the import but disable it: {manifest}"
    );
    assert!(
        !fixture.checkout.read_lockfile_yaml()["imports"]["release-notes"].is_null(),
        "the lockfile should keep pinning the disabled import"
    );
    assert!(
        !fixture.generated_copy_exists(),
        "the generated copy in the reused canonical root should be pruned"
    );
    fixture.run(&home, &["sync"]).success();

    // Re-enabling keeps the pin but cannot rebuild projections until the
    // source is restored or replaced.
    let (assert, body) = fixture.run_json(&home, &["enable", "release-notes"]);
    assert.code(2).stderr(predicate::str::is_empty());
    assert_eq!(body["ok"], true, "enable failed: {body:#?}");
    assert_eq!(body["data"]["enabled"], true, "enable failed: {body:#?}");
    assert_eq!(
        body["data"]["resolved_from"], "manifest-and-lockfile",
        "the enable should report that it acted on the committed state: {body:#?}",
    );
    assert_eq!(
        body["data"]["projection"]["skipped"], true,
        "the skipped projection rebuild should be reported: {body:#?}"
    );
    assert_eq!(
        body["data"]["projection"]["unrestored_imports"],
        json!(["release-notes"]),
        "the report should name the import that blocked resolution: {body:#?}"
    );
    assert!(
        LockfileOnlyCheckout::warnings_contain(
            &body,
            "enabled import 'release-notes' has no stored source",
        ),
        "expected a warning that names the unresolved import: {body:#?}"
    );
    assert!(
        fixture.manifest().contains("enabled: true"),
        "the manifest should enable the import again: {}",
        fixture.manifest()
    );
    assert!(
        !fixture.checkout.read_lockfile_yaml()["imports"]["release-notes"].is_null(),
        "the lockfile should keep pinning the re-enabled import"
    );
}

#[test]
fn disabling_one_unrestored_import_skips_the_rebuild_while_another_stays_unrestored() {
    let author = TestWorkspace::new();
    author.write_manifest(concat!(
        "version: 1\n",
        "\n",
        "targets:\n",
        "  - codex\n",
        "  - gemini-cli\n",
        "  - opencode\n",
    ));
    author.write_skill_source("demo-source", "release-notes");
    author.write_skill_source_at(
        "demo-source",
        ".agents/skills/triage",
        "triage",
        "Triage incoming issues.",
    );
    install_named(&author, &author.home_path(), "demo-source", "release-notes");
    install_named(&author, &author.home_path(), "demo-source", "triage");

    fs::remove_dir_all(author.path().join("demo-source"))
        .expect("the demo source directory is removed");

    let checkout = TestWorkspace::new();
    clone_portable_state(&author, &checkout);
    let home = checkout.home_path();

    // Disabling the first import leaves the second one unresolved, so the
    // projection rebuild is skipped and reported instead of refusing the
    // toggle.
    let (assert, body) = LockfileOnlyCheckout::run_json_in(
        &checkout,
        &home,
        &["--json", "disable", "release-notes"],
    );
    assert.code(2).stderr(predicate::str::is_empty());
    assert_eq!(body["ok"], true, "disable failed: {body:#?}");
    assert_eq!(
        body["data"]["projection"]["skipped"], true,
        "the skipped projection rebuild should be reported: {body:#?}"
    );
    assert_eq!(
        body["data"]["projection"]["unrestored_imports"],
        json!(["triage"]),
        "the report should name the import that blocked resolution: {body:#?}"
    );
    assert!(
        LockfileOnlyCheckout::warnings_contain(
            &body,
            "enabled import 'triage' has no stored source",
        ),
        "expected a warning that names the unresolved import: {body:#?}"
    );

    let manifest =
        fs::read_to_string(checkout.path().join(".agents/skillctl.yaml")).expect("manifest exists");
    assert!(
        manifest.contains("enabled: false"),
        "the disabled import should stay declared but disabled: {manifest}"
    );
    assert!(
        manifest.contains("id: triage"),
        "the unresolved import should stay declared: {manifest}"
    );
    assert!(
        !checkout.read_lockfile_yaml()["imports"]["release-notes"].is_null(),
        "the disabled import should stay pinned until it is removed or restored"
    );
}

#[test]
fn a_disable_from_a_lockfile_only_checkout_rolls_back_when_the_transaction_fails() {
    let fixture = LockfileOnlyCheckout::new();
    let home = fixture.checkout.home_path();
    let tracked = [
        ".agents/skillctl.yaml",
        ".agents/skillctl.lock",
        ".agents/skills",
    ];
    let snapshot = fixture.checkout.snapshot_paths(&tracked);

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(fixture.checkout.path())
        .env("HOME", &home)
        .env("SOURCE_DATE_EPOCH", "1770001234")
        .env("SKILLCTL_FAILPOINT", "disable:after-state")
        .args(["disable", "release-notes"])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("injected lifecycle failure"));

    assert_eq!(
        &fixture.checkout.snapshot_paths(&tracked),
        &snapshot,
        "a failed disable must leave the committed portable state untouched",
    );
}

#[test]
fn toggle_still_refuses_a_skill_no_layer_declares() {
    let fixture = LockfileOnlyCheckout::new();
    let home = fixture.checkout.home_path();

    for command in ["disable", "enable"] {
        let (assert, body) = fixture.run_json(&home, &[command, "unknown-skill"]);
        assert.failure().code(3);
        assert_eq!(body["ok"], false, "{command} should fail: {body:#?}");

        let message = LockfileOnlyCheckout::first_error(&body);
        assert!(
            message.contains("unknown-skill"),
            "expected the failure to name the skill: {message}"
        );
    }
}

#[test]
fn toggle_from_a_lockfile_only_checkout_honors_the_scope_selector() {
    let fixture = LockfileOnlyCheckout::new();
    let home = fixture.checkout.home_path();

    let (assert, body) = fixture.run_json(&home, &["--scope", "user", "disable", "release-notes"]);
    assert.failure().code(3);
    assert_eq!(body["ok"], false, "disable should fail: {body:#?}");
    assert!(
        fixture.manifest().contains("enabled: true"),
        "the workspace import should stay enabled: {}",
        fixture.manifest()
    );
}

#[test]
fn toggle_from_a_lockfile_only_checkout_refuses_an_import_the_lockfile_does_not_pin() {
    let fixture = LockfileOnlyCheckout::new();
    let home = fixture.checkout.home_path();

    fixture
        .checkout
        .write_lockfile("version: 2\n\nstate:\n  manifest_version: 1\n  local_state_version: 2\n");

    let (assert, body) = fixture.run_json(&home, &["disable", "release-notes"]);
    assert.failure().code(3);
    assert_eq!(body["ok"], false, "disable should fail: {body:#?}");
    let message = LockfileOnlyCheckout::first_error(&body);
    assert!(
        message.contains("declared by the manifest but missing from the lockfile"),
        "expected the divergence to be reported before the toggle: {message}"
    );
    assert!(
        fixture.manifest().contains("enabled: true"),
        "the import should stay enabled: {}",
        fixture.manifest()
    );
}

#[test]
fn remove_clears_a_lockfile_entry_the_manifest_no_longer_declares() {
    let fixture = LockfileOnlyCheckout::with_stale_lockfile_entry();
    let home = fixture.checkout.home_path();

    let (_, doctor) = fixture.run_json(&home, &["doctor"]);
    assert!(
        LockfileOnlyCheckout::reports_issue(&doctor, "stale-lockfile-entry"),
        "the entry starts out stale: {doctor:#?}"
    );

    let (assert, body) = fixture.run_json(&home, &["remove", "release-notes"]);
    assert.code(2).stderr(predicate::str::is_empty());
    assert_eq!(body["ok"], true, "remove failed: {body:#?}");
    assert_eq!(body["data"]["skill"], "release-notes");
    assert_eq!(
        body["data"]["resolved_from"], "lockfile-only",
        "the remove should report that it acted on the pin alone: {body:#?}",
    );
    assert!(
        LockfileOnlyCheckout::warnings_contain(
            &body,
            "no install record or manifest import for 'release-notes'",
        ),
        "expected a warning that names the stale entry: {body:#?}"
    );

    assert!(
        fixture.checkout.read_lockfile_yaml()["imports"]["release-notes"].is_null(),
        "the lockfile should no longer pin the entry"
    );
    assert!(
        !fixture.generated_copy_exists(),
        "the leftover generated copy should be pruned with the entry"
    );

    fixture.run(&home, &["sync"]).success();
    let (_, doctor) = fixture.run_json(&home, &["doctor"]);
    assert!(
        !LockfileOnlyCheckout::reports_issue(&doctor, "stale-lockfile-entry"),
        "the cleared entry should stop being reported: {doctor:#?}"
    );
    let (_, verify) = fixture.run_json(&home, &["verify"]);
    assert_eq!(verify["ok"], true, "verify failed: {verify:#?}");

    // A second remove has nothing left to act on in any layer.
    let (assert, body) = fixture.run_json(&home, &["remove", "release-notes"]);
    assert.failure().code(3);
    assert_eq!(body["ok"], false, "remove should fail: {body:#?}");
}

#[test]
fn remove_clears_a_stale_pin_behind_a_recorded_install() {
    let fixture = LockfileOnlyCheckout::new().without_manifest_import();
    let home = fixture.author.home_path();

    let stored_root = fixture.author.path().join(
        fixture
            .author
            .workspace_import_root_relative("release-notes"),
    );
    assert!(
        stored_root.is_dir(),
        "the install record and its cached source start out present"
    );

    let (assert, body) = LockfileOnlyCheckout::run_json_in(
        &fixture.author,
        &home,
        &["--json", "remove", "release-notes"],
    );
    assert.code(2).stderr(predicate::str::is_empty());
    assert_eq!(body["ok"], true, "remove failed: {body:#?}");
    assert_eq!(
        body["data"]["resolved_from"], "install-record",
        "the recorded install is still the source of truth: {body:#?}",
    );
    assert!(
        LockfileOnlyCheckout::warnings_contain(
            &body,
            "the workspace manifest no longer declared 'release-notes'",
        ),
        "expected a warning that names the missing declaration: {body:#?}"
    );

    assert!(
        fixture.author.read_lockfile_yaml()["imports"]["release-notes"].is_null(),
        "the lockfile should no longer pin the entry"
    );
    assert!(
        !stored_root.exists(),
        "the cached source of the cleared entry should be removed"
    );

    let (_, list) = LockfileOnlyCheckout::run_json_in(&fixture.author, &home, &["--json", "list"]);
    let skills = list["data"]["skills"]
        .as_array()
        .unwrap_or_else(|| panic!("skills array exists: {list:#?}"));
    assert!(
        !skills
            .iter()
            .any(|skill| skill["skill"] == "release-notes" && skill["scope"] == "workspace"),
        "the removed import should leave no managed skill behind: {list:#?}"
    );

    LockfileOnlyCheckout::run_json_in(&fixture.author, &home, &["--json", "sync"])
        .0
        .success();
    let (_, doctor) =
        LockfileOnlyCheckout::run_json_in(&fixture.author, &home, &["--json", "doctor"]);
    assert!(
        !LockfileOnlyCheckout::reports_issue(&doctor, "stale-lockfile-entry"),
        "the cleared entry should stop being reported: {doctor:#?}"
    );
}

#[test]
fn remove_of_a_lockfile_only_entry_honors_the_scope_selector() {
    let fixture = LockfileOnlyCheckout::with_stale_lockfile_entry();
    let home = fixture.checkout.home_path();

    let (assert, body) = fixture.run_json(&home, &["--scope", "user", "remove", "release-notes"]);
    assert.failure().code(3);
    assert_eq!(body["ok"], false, "remove should fail: {body:#?}");
    assert!(
        !fixture.checkout.read_lockfile_yaml()["imports"]["release-notes"].is_null(),
        "the user selector must not clear a workspace pin"
    );

    // The workspace selector reaches the entry the lockfile pins alone.
    let (assert, body) =
        fixture.run_json(&home, &["--scope", "workspace", "remove", "release-notes"]);
    assert.code(2).stderr(predicate::str::is_empty());
    assert_eq!(body["ok"], true, "remove failed: {body:#?}");
    assert!(
        fixture.checkout.read_lockfile_yaml()["imports"]["release-notes"].is_null(),
        "the lockfile should no longer pin the entry"
    );
}

#[test]
fn a_toggle_still_refuses_a_lockfile_entry_the_manifest_does_not_declare() {
    let fixture = LockfileOnlyCheckout::with_stale_lockfile_entry();
    let home = fixture.checkout.home_path();

    for command in ["disable", "enable"] {
        let (assert, body) = fixture.run_json(&home, &[command, "release-notes"]);
        assert.failure().code(3);
        assert_eq!(body["ok"], false, "{command} should fail: {body:#?}");
        assert!(
            LockfileOnlyCheckout::first_error(&body).contains("no manifest import declares it"),
            "a toggle has no declaration to flip: {body:#?}"
        );
        assert!(
            !fixture.checkout.read_lockfile_yaml()["imports"]["release-notes"].is_null(),
            "the pin must survive the refused {command}"
        );
    }
}

#[test]
fn remove_of_a_lockfile_only_entry_rolls_back_when_the_transaction_fails() {
    let fixture = LockfileOnlyCheckout::with_stale_lockfile_entry();
    let home = fixture.checkout.home_path();
    let tracked = [
        ".agents/skillctl.yaml",
        ".agents/skillctl.lock",
        ".agents/skills",
    ];
    let snapshot = fixture.checkout.snapshot_paths(&tracked);

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(fixture.checkout.path())
        .env("HOME", &home)
        .env("SOURCE_DATE_EPOCH", "1770001234")
        .env("SKILLCTL_FAILPOINT", "remove:after-state")
        .args(["remove", "release-notes"])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("injected lifecycle failure"));

    assert_eq!(
        &fixture.checkout.snapshot_paths(&tracked),
        &snapshot,
        "a failed remove must leave the stale pin and its copy untouched",
    );
}

#[test]
fn a_workspace_scoped_remove_keeps_the_pin_of_a_user_scoped_import() {
    let workspace = TestWorkspace::new();
    workspace.write_manifest(concat!(
        "version: 1\n",
        "\n",
        "targets:\n",
        "  - codex\n",
        "  - gemini-cli\n",
        "  - opencode\n",
    ));
    workspace.write_skill_source("demo-source", "release-notes");
    let home = workspace.home_path();
    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", &home)
        .args([
            "--json",
            "--no-input",
            "--name",
            "release-notes",
            "--scope",
            "user",
            "install",
            "demo-source",
        ])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    // The user-scope import owns the only pin of that name, so a workspace
    // selector must refuse the request instead of clearing a pin still in use.
    let (assert, body) = LockfileOnlyCheckout::run_json_in(
        &workspace,
        &home,
        &["--json", "--scope", "workspace", "remove", "release-notes"],
    );
    assert.failure().code(3);
    assert_eq!(body["ok"], false, "remove should fail: {body:#?}");
    assert!(
        LockfileOnlyCheckout::first_error(&body).contains("no manifest import declares it"),
        "expected the resolution refusal: {body:#?}"
    );
    assert!(
        !workspace.read_lockfile_yaml()["imports"]["release-notes"].is_null(),
        "the pin of the user-scope import must stay in place"
    );

    let (assert, body) = LockfileOnlyCheckout::run_json_in(
        &workspace,
        &home,
        &["--json", "--scope", "user", "remove", "release-notes"],
    );
    let exit_code = assert.get_output().status.code().expect("exit code exists");
    assert!(
        matches!(exit_code, 0 | 2),
        "user-scope remove should succeed, possibly with warnings: {body:#?}",
    );
    assert_eq!(body["ok"], true, "user-scope remove failed: {body:#?}");
    assert!(
        workspace.read_lockfile_yaml()["imports"]["release-notes"].is_null(),
        "the matching selector clears the pin it owns"
    );
}
