use super::*;

#[test]
fn override_creates_a_minimal_overlay_and_updates_manifest_lockfile_and_state() {
    let workspace = TestWorkspace::new();
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

    let installed_lockfile = workspace.read_lockfile_yaml();
    let original_overlay_hash = installed_lockfile["imports"]["release-notes"]["hashes"]["overlay"]
        .as_str()
        .expect("overlay hash exists")
        .to_string();
    let original_effective_version =
        installed_lockfile["imports"]["release-notes"]["hashes"]["effective_version"]
            .as_str()
            .expect("effective version exists")
            .to_string();
    assert_eq!(original_overlay_hash, "sha256:none");

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .env("SOURCE_DATE_EPOCH", "1770001234")
        .args(["override", "release-notes"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty())
        .stdout(predicate::str::contains(
            "Created overlay for release-notes",
        ))
        .stdout(predicate::str::contains(".agents/overlays/release-notes"));

    let overlay_manifest_path = workspace
        .path()
        .join(".agents/overlays/release-notes/SKILL.md");
    assert!(
        overlay_manifest_path.is_file(),
        "overlay manifest should exist"
    );
    assert_eq!(
        fs::read_to_string(&overlay_manifest_path).expect("overlay manifest exists"),
        fs::read_to_string(
            workspace
                .workspace_import_root("release-notes")
                .join(".agents/skills/release-notes/SKILL.md")
        )
        .expect("stored import manifest exists")
    );

    let manifest = fs::read_to_string(workspace.path().join(".agents/skillctl.yaml"))
        .expect("manifest exists");
    assert!(
        manifest.contains("overrides:\n  release-notes: .agents/overlays/release-notes\n"),
        "manifest was {manifest}",
    );

    let lockfile = workspace.read_lockfile_yaml();
    let overlay_hash = lockfile["imports"]["release-notes"]["hashes"]["overlay"]
        .as_str()
        .expect("overlay hash exists")
        .to_string();
    let effective_version = lockfile["imports"]["release-notes"]["hashes"]["effective_version"]
        .as_str()
        .expect("effective version exists")
        .to_string();
    assert_ne!(overlay_hash, "sha256:none");
    assert_ne!(overlay_hash, original_overlay_hash);
    assert_ne!(effective_version, original_effective_version);
    assert!(
        !lockfile["imports"]["release-notes"]
            .as_mapping()
            .expect("import entry exists")
            .contains_key("timestamps"),
        "local timestamps stay out of the portable lockfile"
    );

    let connection = Connection::open(workspace.home_path().join(".skillctl/state.db"))
        .expect("state database opens");
    let state_row: (String, String, String) = connection
        .query_row(
            "SELECT overlay_hash, effective_version_hash, updated_at \
             FROM install_records WHERE skill_id = ?1",
            params!["release-notes"],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("install record exists");
    assert_eq!(state_row.0, overlay_hash);
    assert_eq!(state_row.1, effective_version);
    assert_eq!(state_row.2, "2026-02-02T03:00:34Z");

    let pin_effective_version: Option<String> = connection
        .query_row(
            "SELECT effective_version_hash FROM pins WHERE skill_id = ?1",
            params!["release-notes"],
            |row| row.get(0),
        )
        .expect("pin record exists");
    assert_eq!(
        pin_effective_version.as_deref(),
        Some(effective_version.as_str())
    );

    let history_kinds: Vec<String> = connection
        .prepare(
            "SELECT kind FROM history_events WHERE skill_id = ?1 ORDER BY occurred_at ASC, id ASC",
        )
        .expect("statement prepares")
        .query_map(params!["release-notes"], |row| row.get(0))
        .expect("history query succeeds")
        .collect::<Result<_, _>>()
        .expect("history rows decode");
    assert_eq!(history_kinds.first().map(String::as_str), Some("install"));
    assert_eq!(
        history_kinds.last().map(String::as_str),
        Some("overlay-created")
    );
    assert!(
        history_kinds.iter().any(|kind| kind == "projection"),
        "history should retain projection events: {history_kinds:?}",
    );
}

#[test]
fn override_is_idempotent_and_does_not_clobber_existing_overlay_edits() {
    let workspace = TestWorkspace::new();
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
        .args(["override", "release-notes"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    let overlay_path = workspace
        .path()
        .join(".agents/overlays/release-notes/SKILL.md");
    fs::write(
        &overlay_path,
        concat!(
            "---\n",
            "name: release-notes\n",
            "description: Local overlay version.\n",
            "---\n",
            "\n",
            "# release-notes\n"
        ),
    )
    .expect("overlay file updated");

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .env("SOURCE_DATE_EPOCH", "1770002468")
        .args(["override", "release-notes"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty())
        .stdout(predicate::str::contains(
            "Overlay for release-notes is ready",
        ));

    assert!(
        fs::read_to_string(&overlay_path)
            .expect("overlay exists")
            .contains("Local overlay version."),
        "existing overlay edits should be preserved",
    );

    let lockfile = workspace.read_lockfile_yaml();
    assert_ne!(
        lockfile["imports"]["release-notes"]["hashes"]["overlay"]
            .as_str()
            .expect("overlay hash exists"),
        "sha256:none"
    );
    assert!(
        !lockfile["imports"]["release-notes"]
            .as_mapping()
            .expect("import entry exists")
            .contains_key("timestamps"),
        "local timestamps stay out of the portable lockfile"
    );
}

/// `override` and `fork` in a workspace that holds a canonical local skill but
/// no manifest.
///
/// Resolution runs against the documented defaults, so a name no install or
/// declaration covers hears the same refusal the sibling import-lifecycle
/// commands give it, instead of a raw manifest read error.
#[test]
fn override_and_fork_refuse_the_unmanaged_name_in_a_manifest_free_workspace() {
    let workspace = TestWorkspace::new();
    workspace.write_workspace_skill("bare", "Bare skill.", &[]);
    let home = workspace.home_path();

    for args in [
        vec!["override", "bare"],
        vec!["fork", "bare"],
        vec!["--scope", "workspace", "override", "bare"],
        vec!["--scope", "user", "fork", "bare"],
    ] {
        let output = Command::cargo_bin("skillctl")
            .expect("binary exists")
            .current_dir(workspace.path())
            .env("HOME", &home)
            .args(&args)
            .output()
            .expect("command runs");
        assert_eq!(
            output.status.code(),
            Some(3),
            "{args:?} must refuse as a validation failure"
        );
        assert!(
            output.stdout.is_empty(),
            "{args:?} reports the refusal on stderr, not as a report"
        );
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        assert!(
            stderr.contains("skill 'bare' is not installed and no manifest import declares it"),
            "{args:?} must name the name nothing manages: {stderr}"
        );
        assert!(
            !stderr.contains("os error"),
            "{args:?} must not surface the raw filesystem error: {stderr}"
        );
    }

    // The refusal is the one the sibling commands answer with, so one
    // name-resolution rule covers every import-lifecycle command.
    let sibling = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", &home)
        .args(["remove", "bare"])
        .output()
        .expect("command runs");
    assert_eq!(
        sibling.status.code(),
        Some(3),
        "remove refuses the same name the same way"
    );
    assert_eq!(
        String::from_utf8_lossy(&sibling.stderr),
        String::from_utf8_lossy(
            &Command::cargo_bin("skillctl")
                .expect("binary exists")
                .current_dir(workspace.path())
                .env("HOME", &home)
                .args(["override", "bare"])
                .output()
                .expect("command runs")
                .stderr
        ),
        "override and remove must give the bare name one refusal"
    );

    // The refusals leave the bare workspace bare.
    assert!(
        !workspace.path().join(".agents/skillctl.yaml").exists(),
        "no manifest may appear"
    );
    assert!(
        !workspace.path().join(".agents/overlays").exists(),
        "no overlay directory may be created"
    );
    assert!(
        !workspace.path().join(".agents/skills/bare").is_symlink(),
        "the canonical local skill stays a directory someone authored"
    );
    assert!(
        workspace
            .path()
            .join(".agents/skills/bare/SKILL.md")
            .is_file(),
        "the canonical local skill stays untouched"
    );

    // The machine contract carries the same refusal.
    let output = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", &home)
        .args(["--json", "fork", "bare"])
        .output()
        .expect("command runs");
    assert_eq!(output.status.code(), Some(3), "the JSON run refuses too");
    let body: Value = serde_json::from_slice(&output.stdout).expect("stdout is valid json");
    assert_eq!(
        body["ok"], false,
        "the refusal must not claim success: {body:#?}"
    );
    assert!(
        body["errors"]
            .as_array()
            .expect("errors array exists")
            .iter()
            .any(|error| error
                .as_str()
                .expect("error is a string")
                .contains("skill 'bare' is not installed")),
        "the JSON error must name the unmanaged skill: {body:#?}"
    );
}

/// `override` and `fork` in a workspace that holds a real install but lost its
/// manifest.
///
/// The install record resolves the name, so the refusal names the state that
/// actually blocks the command: the declaration both commands rewrite is gone.
#[test]
fn override_and_fork_name_init_when_an_install_survives_a_lost_manifest() {
    let workspace = TestWorkspace::new();
    workspace.write_skill_source("shared-skills", "release-notes");

    Command::cargo_bin("skillctl")
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
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    let lockfile_before = fs::read_to_string(workspace.path().join(".agents/skillctl.lock"))
        .expect("the install pinned a lockfile");
    fs::remove_file(workspace.path().join(".agents/skillctl.yaml"))
        .expect("the manifest is removed");

    for command in ["override", "fork"] {
        let output = Command::cargo_bin("skillctl")
            .expect("binary exists")
            .current_dir(workspace.path())
            .env("HOME", workspace.home_path())
            .args([command, "release-notes"])
            .output()
            .expect("command runs");
        assert_eq!(
            output.status.code(),
            Some(3),
            "{command} must refuse as a validation failure"
        );
        assert!(
            output.stdout.is_empty(),
            "{command} reports the refusal on stderr, not as a report"
        );
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        assert!(
            stderr.contains(".agents/skillctl.yaml"),
            "{command} must name the missing manifest: {stderr}"
        );
        assert!(
            stderr.contains(&format!("'{command}'")),
            "{command} must name the command that needs declared state: {stderr}"
        );
        assert!(
            stderr.contains("skillctl init"),
            "{command} must name the repair: {stderr}"
        );
        assert!(
            !stderr.contains("os error"),
            "{command} must not surface the raw filesystem error: {stderr}"
        );
    }

    // A refused command changes nothing: the pin stays, no overlay appears, and
    // no canonical copy is created for the fork that never ran.
    assert_eq!(
        fs::read_to_string(workspace.path().join(".agents/skillctl.lock"))
            .expect("the lockfile survives"),
        lockfile_before,
        "the refusal must leave the pin exactly as it was"
    );
    assert!(
        !workspace
            .path()
            .join(".agents/overlays/release-notes")
            .exists(),
        "no overlay for the import may be created"
    );
    assert!(
        !workspace
            .path()
            .join(".agents/skills/release-notes")
            .is_symlink(),
        "the fork may not replace the projected root"
    );
}

/// `override` and `fork` in a workspace that holds a manifest and an install
/// but lost its lockfile.
///
/// An overlay re-pins effective hashes in the lockfile and a fork removes the
/// pin, so the missing file is the state that blocks both. The refusal names
/// both repairs, because `skillctl init` would write an empty lockfile and
/// drop the pin the command was about to rewrite.
#[test]
fn override_and_fork_name_both_repairs_when_the_manifest_survives_a_lost_lockfile() {
    let workspace = TestWorkspace::new();
    workspace.write_skill_source("shared-skills", "release-notes");

    Command::cargo_bin("skillctl")
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
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    fs::remove_file(workspace.path().join(".agents/skillctl.lock"))
        .expect("the lockfile is removed");

    for command in ["override", "fork"] {
        let output = Command::cargo_bin("skillctl")
            .expect("binary exists")
            .current_dir(workspace.path())
            .env("HOME", workspace.home_path())
            .args([command, "release-notes"])
            .output()
            .expect("command runs");
        assert_eq!(
            output.status.code(),
            Some(3),
            "{command} must refuse as a validation failure"
        );
        assert!(
            output.stdout.is_empty(),
            "{command} reports the refusal on stderr, not as a report"
        );
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        assert!(
            stderr.contains(".agents/skillctl.lock"),
            "{command} must name the missing lockfile: {stderr}"
        );
        assert!(
            stderr.contains(&format!("'{command}'")),
            "{command} must name the command that needs pinned state: {stderr}"
        );
        assert!(
            stderr.contains("retrieve the committed .agents/skillctl.lock"),
            "{command} must name the team-checkout repair: {stderr}"
        );
        assert!(
            stderr.contains("skillctl init"),
            "{command} must name the new-workspace repair: {stderr}"
        );
        assert!(
            !stderr.contains("os error"),
            "{command} must not surface the raw filesystem error: {stderr}"
        );
    }

    // A refused command changes nothing: the lockfile stays absent, no overlay
    // appears, and no canonical copy is created for a fork that never ran.
    assert!(
        !workspace.path().join(".agents/skillctl.lock").exists(),
        "no lockfile may appear"
    );
    assert!(
        !workspace
            .path()
            .join(".agents/overlays/release-notes")
            .exists(),
        "no overlay for the import may be created"
    );
    assert!(
        !workspace
            .path()
            .join(".agents/skills/release-notes")
            .is_symlink(),
        "the fork may not replace the projected root"
    );
}

#[test]
fn override_rolls_back_when_the_transaction_fails_after_state_write() {
    let workspace = TestWorkspace::new();
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
        ".agents/skillctl.lock",
        ".agents/overlays/release-notes",
        "home/.skillctl/state.db",
    ]);

    assert_transaction_rolled_back(
        &workspace,
        "override:after-state",
        "1770001234",
        &["override", "release-notes"],
        &snapshot,
    );
}

/// One full quickstart overlay round: install from a portable Git source,
/// override, edit the overlay, and sync.
///
/// The test needs a source a second checkout can acquire again, so it builds a
/// local Git repository instead of installing from a plain directory.
struct OverlayEditFixture {
    workspace: TestWorkspace,
}

impl OverlayEditFixture {
    fn new() -> Self {
        let workspace = TestWorkspace::new();
        workspace.write_skill_source("git-source", "release-notes");
        workspace.init_git_repo("git-source");

        Command::cargo_bin("skillctl")
            .expect("binary exists")
            .current_dir(workspace.path())
            .env("HOME", workspace.home_path())
            .args([
                "--json",
                "--no-input",
                "--name",
                "release-notes",
                "install",
                &workspace.git_repo_url("git-source"),
            ])
            .assert()
            .success()
            .stderr(predicate::str::is_empty());

        Command::cargo_bin("skillctl")
            .expect("binary exists")
            .current_dir(workspace.path())
            .env("HOME", workspace.home_path())
            .args(["--json", "--no-input", "override", "release-notes"])
            .assert()
            .success()
            .stderr(predicate::str::is_empty());

        workspace.write_file(
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

        Self { workspace }
    }

    /// Digest the lockfile pins for the overlay right after the edit.
    fn pinned_overlay_digest(&self) -> String {
        self.workspace.read_lockfile_yaml()["imports"]["release-notes"]["hashes"]["overlay"]
            .as_str()
            .expect("overlay digest is recorded")
            .to_string()
    }

    fn sync(&self) -> Value {
        let assert = Command::cargo_bin("skillctl")
            .expect("binary exists")
            .current_dir(self.workspace.path())
            .env("HOME", self.workspace.home_path())
            .args(["--json", "sync"])
            .assert();

        let output = assert.get_output();
        serde_json::from_slice(&output.stdout).expect("stdout is valid json")
    }

    fn override_again(&self) {
        Command::cargo_bin("skillctl")
            .expect("binary exists")
            .current_dir(self.workspace.path())
            .env("HOME", self.workspace.home_path())
            .args(["--json", "--no-input", "override", "release-notes"])
            .assert()
            .success()
            .stderr(predicate::str::is_empty());
    }
}

#[test]
fn sync_warns_and_exits_two_when_the_edited_overlay_no_longer_matches_its_pin() {
    let fixture = OverlayEditFixture::new();
    let pinned_overlay_digest = fixture.pinned_overlay_digest();
    let lockfile_before =
        fs::read_to_string(fixture.workspace.path().join(".agents/skillctl.lock"))
            .expect("lockfile is readable");

    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(fixture.workspace.path())
        .env("HOME", fixture.workspace.home_path())
        .args(["--json", "sync"])
        .assert()
        .code(2)
        .stderr(predicate::str::is_empty());

    let body: Value =
        serde_json::from_slice(&assert.get_output().stdout).expect("stdout is valid json");
    assert_eq!(body["ok"], true, "sync still completes: {body:#?}");
    let warnings = body["warnings"]
        .as_array()
        .expect("warnings are an array")
        .iter()
        .map(|warning| warning.as_str().expect("warning is a string"))
        .collect::<Vec<_>>();
    assert_eq!(warnings.len(), 1, "one drift warning: {body:#?}");
    assert!(
        warnings[0].contains(&pinned_overlay_digest),
        "the warning must name the stale pinned digest, found: {}",
        warnings[0]
    );
    assert!(
        warnings[0].contains("skillctl override release-notes"),
        "the warning must name the re-pinning command, found: {}",
        warnings[0]
    );

    // The projection itself is not held back: the edit is the content the user
    // asked for, and the warning is what makes the stale pin visible.
    assert!(
        fs::read_to_string(
            fixture
                .workspace
                .path()
                .join(".agents/skills/release-notes/SKILL.md")
        )
        .expect("the projected manifest is readable")
        .contains("team edition"),
        "the edited overlay content is still projected"
    );
    assert_eq!(
        fs::read_to_string(fixture.workspace.path().join(".agents/skillctl.lock"))
            .expect("lockfile is readable"),
        lockfile_before,
        "sync never re-pins the lockfile on its own"
    );
}

#[test]
fn re_pinning_the_edited_overlay_clears_the_sync_warning() {
    let fixture = OverlayEditFixture::new();
    let stale_overlay_digest = fixture.pinned_overlay_digest();

    let drifted = fixture.sync();
    assert!(
        !drifted["warnings"]
            .as_array()
            .expect("warnings are an array")
            .is_empty(),
        "the edit is unpinned before the re-pin: {drifted:#?}"
    );

    fixture.override_again();

    let body = fixture.sync();
    assert_eq!(body["ok"], true, "sync after the re-pin: {body:#?}");
    assert_eq!(
        body["warnings"],
        json!([]),
        "the pin now matches: {body:#?}"
    );

    let overlay_digest =
        fixture.workspace.read_lockfile_yaml()["imports"]["release-notes"]["hashes"]["overlay"]
            .as_str()
            .expect("overlay digest is recorded")
            .to_string();
    assert_ne!(
        overlay_digest, stale_overlay_digest,
        "the re-pin must record the edited overlay content"
    );
}

/// A checkout that holds the committed manifest and lockfile of a portable Git
/// import and a fresh local state store.
///
/// That is the state a fresh clone is in before its first
/// `skillctl restore --frozen`. The import cache and the state store are local
/// to each machine, so only the two control files travel with the repository.
struct LockfileOnlyImport {
    /// Author workspace that owns the Git source. It stays alive for as long
    /// as the checkout needs to clone from it.
    author: TestWorkspace,
    checkout: TestWorkspace,
}

impl LockfileOnlyImport {
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
        author.write_skill_source("portable-source", "release-notes");
        author.init_git_repo("portable-source");

        Command::cargo_bin("skillctl")
            .expect("binary exists")
            .current_dir(author.path())
            .env("HOME", author.home_path())
            .args([
                "--json",
                "--no-input",
                "--name",
                "release-notes",
                "install",
                &author.git_repo_url("portable-source"),
            ])
            .assert()
            .success()
            .stderr(predicate::str::is_empty());

        let checkout = TestWorkspace::new();
        for relative in [".agents/skillctl.yaml", ".agents/skillctl.lock"] {
            let contents =
                fs::read_to_string(author.path().join(relative)).unwrap_or_else(|error| {
                    panic!("{relative} is readable: {error}");
                });
            checkout.write_file(relative, &contents);
        }

        Self { author, checkout }
    }

    /// Copy the author's import cache into the checkout, so the stored source
    /// exists while the state store still holds no install record.
    fn copy_import_cache(&self) {
        let relative = self
            .checkout
            .workspace_import_root_relative("release-notes")
            .to_string_lossy()
            .to_string();
        self.checkout.copy_path(
            &self.author.workspace_import_root("release-notes"),
            &relative,
        );
    }

    fn run(&self, args: &[&str]) -> assert_cmd::assert::Assert {
        Self::run_in(&self.checkout, args)
    }

    fn run_in(workspace: &TestWorkspace, args: &[&str]) -> assert_cmd::assert::Assert {
        Command::cargo_bin("skillctl")
            .expect("binary exists")
            .current_dir(workspace.path())
            .env("HOME", workspace.home_path())
            .args(args)
            .assert()
    }

    /// Run skillctl as JSON and return the assert and the response body.
    fn run_json(&self, args: &[&str]) -> (assert_cmd::assert::Assert, Value) {
        let mut json_args = vec!["--json"];
        json_args.extend_from_slice(args);
        let assert = self
            .run(&json_args)
            .stdout(predicate::str::is_empty().not());
        let body: Value =
            serde_json::from_slice(&assert.get_output().stdout).expect("stdout is valid json");
        (assert, body)
    }

    /// Run skillctl as JSON in any workspace and return the response body.
    fn run_json_in(workspace: &TestWorkspace, args: &[&str]) -> Value {
        let mut json_args = vec!["--json"];
        json_args.extend_from_slice(args);
        let assert = Self::run_in(workspace, &json_args).stdout(predicate::str::is_empty().not());
        serde_json::from_slice(&assert.get_output().stdout).expect("stdout is valid json")
    }

    /// First error message of a JSON response.
    fn first_error(body: &Value) -> String {
        body["errors"][0].as_str().unwrap_or_default().to_string()
    }

    fn manifest(&self) -> String {
        fs::read_to_string(self.checkout.path().join(".agents/skillctl.yaml"))
            .expect("checkout manifest exists")
    }

    fn lockfile(&self) -> String {
        fs::read_to_string(self.checkout.path().join(".agents/skillctl.lock"))
            .expect("checkout lockfile exists")
    }
}

#[test]
fn override_and_fork_name_the_missing_stored_source_in_a_lockfile_only_checkout() {
    let fixture = LockfileOnlyImport::new();
    let manifest_before = fixture.manifest();
    let lockfile_before = fixture.lockfile();

    // The unscoped request and the workspace selector both resolve the import
    // the committed manifest declares, so both name the state that blocks the
    // command instead of denying that the checkout manages the import.
    for args in [
        vec!["override", "release-notes"],
        vec!["--scope", "workspace", "override", "release-notes"],
        vec!["fork", "release-notes"],
        vec!["--scope", "workspace", "fork", "release-notes"],
    ] {
        let (assert, body) = fixture.run_json(&args);
        assert.failure().code(3).stderr(predicate::str::is_empty());
        assert_eq!(body["ok"], false, "{args:?} should fail: {body:#?}");
        let message = LockfileOnlyImport::first_error(&body);
        assert!(
            message.contains("no stored source") && message.contains("restore --frozen"),
            "the refusal should name the missing source and the repair: {message}",
        );
        assert!(
            message.contains("skillctl remove release-notes"),
            "the refusal should name the exit a nonrestorable source has: {message}",
        );
        assert!(
            !message.contains("not a managed imported skill"),
            "the refusal must not deny the declared import: {message}",
        );
    }

    // The user selector names a scope the checkout does not declare the
    // import in, which stays a resolution failure.
    let (assert, body) = fixture.run_json(&["--scope", "user", "override", "release-notes"]);
    assert.failure().code(3).stderr(predicate::str::is_empty());
    assert_eq!(
        body["ok"], false,
        "user-scope override should fail: {body:#?}"
    );
    let message = LockfileOnlyImport::first_error(&body);
    assert!(
        message.contains("no manifest import declares it"),
        "the user selector should report the missing declaration: {message}",
    );

    // The refused commands leave the checkout exactly as it was.
    assert!(
        !fixture.checkout.path().join(".agents/overlays").exists(),
        "no overlay directory may be created",
    );
    assert!(
        !fixture
            .checkout
            .path()
            .join(".agents/skills/release-notes")
            .exists(),
        "no canonical skill may be created",
    );
    assert_eq!(fixture.manifest(), manifest_before);
    assert_eq!(fixture.lockfile(), lockfile_before);
}

#[test]
fn override_and_fork_name_the_missing_install_record_when_the_cache_survived() {
    let fixture = LockfileOnlyImport::new();
    fixture.copy_import_cache();

    // The stored source exists, so the obstacle the refusal names is the
    // missing local install record, which `restore --frozen` rebuilds from the
    // committed lockfile without contacting the source.
    for args in [
        vec!["--scope", "workspace", "override", "release-notes"],
        vec!["--scope", "workspace", "fork", "release-notes"],
    ] {
        let (assert, body) = fixture.run_json(&args);
        assert.failure().code(3).stderr(predicate::str::is_empty());
        assert_eq!(body["ok"], false, "{args:?} should fail: {body:#?}");
        let message = LockfileOnlyImport::first_error(&body);
        assert!(
            message.contains("no install record") && message.contains("restore --frozen"),
            "the refusal should name the missing record and the repair: {message}",
        );
        assert!(
            !message.contains("no stored source"),
            "the stored source exists, so the refusal must not deny it: {message}",
        );
    }
}

#[test]
fn override_and_fork_name_the_missing_stored_source_when_the_cache_was_deleted() {
    let fixture = LockfileOnlyImport::new();

    // The author workspace holds the install record and the lockfile pin, but
    // its import cache is gone, so the refusal names the missing source and the
    // restore instead of a raw filesystem failure. The projected copy in the
    // reused canonical root stays as the install left it.
    fs::remove_dir_all(fixture.author.workspace_import_root("release-notes"))
        .expect("the import cache is removed");
    let projected_copy = fs::read_to_string(
        fixture
            .author
            .path()
            .join(".agents/skills/release-notes/SKILL.md"),
    )
    .expect("the projected copy exists");

    for command in ["override", "fork"] {
        let body = LockfileOnlyImport::run_json_in(
            &fixture.author,
            &["--no-input", command, "release-notes"],
        );
        assert_eq!(body["ok"], false, "{command} should fail: {body:#?}");
        let message = LockfileOnlyImport::first_error(&body);
        assert!(
            message.contains("no stored source") && message.contains("restore --frozen"),
            "the refusal should name the missing source and the repair: {message}",
        );
    }

    assert!(
        !fixture
            .author
            .path()
            .join(".agents/overlays/release-notes")
            .exists(),
        "no overlay directory may be created",
    );
    assert_eq!(
        fs::read_to_string(
            fixture
                .author
                .path()
                .join(".agents/skills/release-notes/SKILL.md")
        )
        .expect("the projected copy still exists"),
        projected_copy,
        "the refused commands must not rewrite the projected copy",
    );
}

#[test]
fn a_frozen_restore_unlocks_override_and_fork_in_a_lockfile_only_checkout() {
    let fixture = LockfileOnlyImport::new();

    // The repair the refusals name reconstructs the import cache and the
    // install records from the committed lockfile alone.
    fixture
        .run(&["--no-input", "restore", "--frozen"])
        .success()
        .stderr(predicate::str::is_empty());

    let (assert, body) = fixture.run_json(&["--no-input", "override", "release-notes"]);
    let exit_code = assert.get_output().status.code().expect("exit code exists");
    assert!(
        matches!(exit_code, 0 | 2),
        "override after the restore should succeed, possibly with warnings: {body:#?}",
    );
    assert_eq!(body["ok"], true, "override after the restore: {body:#?}");
    assert_eq!(body["data"]["scope"], "workspace");
    assert!(
        fixture
            .checkout
            .path()
            .join(".agents/overlays/release-notes/SKILL.md")
            .is_file(),
        "the overlay must be created after the restore",
    );

    let (assert, body) = fixture.run_json(&["--no-input", "fork", "release-notes"]);
    let exit_code = assert.get_output().status.code().expect("exit code exists");
    assert!(
        matches!(exit_code, 0 | 2),
        "fork after the restore should succeed, possibly with warnings: {body:#?}",
    );
    assert_eq!(body["ok"], true, "fork after the restore: {body:#?}");
    assert!(
        fixture
            .checkout
            .path()
            .join(".agents/skills/release-notes/SKILL.md")
            .is_file(),
        "the forked copy must land in canonical workspace skills",
    );
    assert!(
        !fixture.manifest().contains("id: release-notes"),
        "the fork must remove the managed import: {}",
        fixture.manifest(),
    );
}
