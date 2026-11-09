use super::*;

/// Files and directories one teammate commits so another checkout can restore
/// the same content. Overlays are portable state too: the lockfile pins their
/// digest but does not carry their files.
const PORTABLE_STATE: [&str; 3] = [
    ".agents/skillctl.yaml",
    ".agents/skillctl.lock",
    ".agents/overlays",
];

struct RestoreFixture {
    /// Checkout that installs the skill and commits the portable state.
    author: TestWorkspace,
    /// Upstream Git repository the import is pinned to.
    #[allow(dead_code)]
    repo_url: String,
}

impl RestoreFixture {
    fn new() -> Self {
        let workspace = TestWorkspace::new();
        workspace.write_manifest(concat!(
            "version: 1\n",
            "\n",
            "targets:\n",
            "  - claude-code\n",
        ));
        workspace.write_skill_source("git-source", "release-notes");
        workspace.write_file(
            "git-source/.agents/skills/release-notes/references/style.md",
            "# Style\n",
        );
        workspace.init_git_repo("git-source");
        let repo_url = workspace.git_repo_url("git-source");

        install(&workspace, &workspace.home_path(), &repo_url);

        Self {
            author: workspace,
            repo_url,
        }
    }

    /// Clone only the declared portable state into a new checkout.
    fn clone_portable_state(&self) -> TestWorkspace {
        let clone = TestWorkspace::new();
        copy_portable_state(&self.author, &clone);
        clone
    }

    fn change_upstream(&self) {
        self.author.write_skill_source_at(
            "git-source",
            ".agents/skills/release-notes",
            "release-notes",
            "Rewritten upstream helper that must never be restored silently.",
        );
        self.author
            .commit_all("git-source", "rewrite upstream skill");
    }
}

/// Copy the portable state from one checkout into another.
///
/// Directories such as `.agents/overlays` are copied as trees and are simply
/// absent when the author has none.
fn copy_portable_state(from: &TestWorkspace, to: &TestWorkspace) {
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

fn install(workspace: &TestWorkspace, home: &Path, source: &str) -> Value {
    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", home)
        .args([
            "--json",
            "--no-input",
            "--name",
            "release-notes",
            "install",
            source,
        ])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    let body: Value =
        serde_json::from_slice(&assert.get_output().stdout).expect("stdout is valid json");
    assert_eq!(body["ok"], true, "install failed: {body:#?}");
    body
}

fn restore(workspace: &TestWorkspace, home: &Path, flags: &[&str]) -> assert_cmd::assert::Assert {
    let mut args = vec!["--json", "restore"];
    args.extend_from_slice(flags);
    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", home)
        .args(&args)
        .assert()
}

fn verify(workspace: &TestWorkspace, home: &Path) -> Value {
    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", home)
        .args(["--json", "verify"])
        .assert();

    serde_json::from_slice(&assert.get_output().stdout).expect("stdout is valid json")
}

fn restore_text(workspace: &TestWorkspace, home: &Path, flags: &[&str]) -> String {
    let output = restore(workspace, home, flags).get_output().clone();
    format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn lockfile_bytes(workspace: &TestWorkspace) -> Vec<u8> {
    fs::read(workspace.path().join(".agents/skillctl.lock")).expect("lockfile is readable")
}

fn projected_manifest(workspace: &TestWorkspace, home: &Path) -> String {
    // Projections are rebuilt by the restore itself, so read whatever the
    // workspace now projects for claude-code.
    let _ = home;
    fs::read_to_string(
        workspace
            .path()
            .join(".claude/skills/release-notes/SKILL.md"),
    )
    .unwrap_or_else(|error| panic!("projected manifest is readable: {error}"))
}

#[test]
fn restore_frozen_reproduces_the_approved_digest_in_a_fresh_checkout() {
    let fixture = RestoreFixture::new();
    let approved_digest = fixture.author.read_lockfile_yaml()["imports"]["release-notes"]["bundle"]
        ["digest"]
        .as_str()
        .expect("bundle digest is recorded")
        .to_string();
    assert!(
        approved_digest.starts_with("sha256:"),
        "install records a canonical bundle digest, found {approved_digest}"
    );
    let approved_content_hash =
        fixture.author.read_lockfile_yaml()["imports"]["release-notes"]["hashes"]["content"]
            .as_str()
            .expect("content hash is recorded")
            .to_string();
    let approved_manifest = projected_manifest(&fixture.author, &fixture.author.home_path());
    let committed_lockfile = lockfile_bytes(&fixture.author);

    // Upstream moves on after the lockfile was committed.
    fixture.change_upstream();

    // A different path with a fresh home reconstructs the pinned content.
    let clone = fixture.clone_portable_state();
    let fresh_home = clone.home_path();
    let assert = restore(&clone, &fresh_home, &["--frozen"])
        .success()
        .stderr(predicate::str::is_empty());

    let body: Value =
        serde_json::from_slice(&assert.get_output().stdout).expect("stdout is valid json");
    assert_eq!(body["ok"], true, "restore failed: {body:#?}");
    assert_eq!(body["data"]["frozen"], true);
    assert_eq!(body["data"]["lockfile"]["changed"], false);
    assert_eq!(
        body["data"]["imports"][0]["restored_from"],
        "source-at-locked-revision"
    );
    assert_eq!(body["data"]["imports"][0]["bundle_digest"], approved_digest);
    assert_eq!(body["data"]["imports"][0]["verified"], true);

    // The lockfile is byte-for-byte unchanged by the restore.
    assert_eq!(
        lockfile_bytes(&clone),
        committed_lockfile,
        "a frozen restore must not rewrite the lockfile"
    );

    // The restored projection is the approved content, not the new upstream one.
    assert_eq!(projected_manifest(&clone, &fresh_home), approved_manifest);
    let restored = verify(&clone, &fresh_home);
    assert_eq!(restored["ok"], true, "verify failed: {restored:#?}");
    assert_eq!(
        restored["data"]["imports"][0]["integrity"]["status"],
        "verified"
    );
    assert_eq!(
        restored["data"]["imports"][0]["bundle_digest"],
        approved_digest
    );

    // The same revision is still what the lockfile pins, and the cached copy
    // hashes to the approved content hash.
    let lockfile = clone.read_lockfile_yaml();
    assert_eq!(
        lockfile["imports"]["release-notes"]["hashes"]["content"]
            .as_str()
            .expect("content hash is recorded"),
        approved_content_hash
    );
    let stored_root = clone.workspace_import_root("release-notes");
    let stored_manifest =
        fs::read_to_string(stored_root.join(".agents/skills/release-notes/SKILL.md"))
            .expect("stored import exists");
    assert!(stored_manifest.contains("Summarize release notes."));
    assert!(
        !stored_manifest.contains("Rewritten upstream helper"),
        "restore must never substitute newer upstream content"
    );
}

#[test]
fn restore_after_a_moved_checkout_and_cache_eviction_uses_the_retained_artifact() {
    let fixture = RestoreFixture::new();

    // Move the checkout to another path and evict the workspace import cache.
    let moved = fixture.clone_portable_state();
    let same_home = fixture.author.home_path();
    let imports_root = same_home.join(".skillctl/store/imports");
    fs::remove_dir_all(&imports_root).expect("import cache is evicted");

    let assert = restore(&moved, &same_home, &["--frozen", "--offline"])
        .success()
        .stderr(predicate::str::is_empty());
    let body: Value =
        serde_json::from_slice(&assert.get_output().stdout).expect("stdout is valid json");
    assert_eq!(body["ok"], true, "offline restore failed: {body:#?}");
    assert_eq!(
        body["data"]["imports"][0]["restored_from"], "retained-artifact",
        "an evicted cache must be rebuilt from the retained artifact: {body:#?}"
    );

    let report = verify(&moved, &same_home);
    assert_eq!(
        report["data"]["imports"][0]["integrity"]["status"],
        "verified"
    );
}

#[test]
fn restore_offline_fails_without_pretending_to_check_upstream() {
    let fixture = RestoreFixture::new();
    fixture.change_upstream();

    let clone = fixture.clone_portable_state();
    let fresh_home = clone.home_path();
    let before = lockfile_bytes(&clone);

    restore(&clone, &fresh_home, &["--frozen", "--offline"])
        .failure()
        .code(3);
    assert!(
        restore_text(&clone, &fresh_home, &["--frozen", "--offline"])
            .contains("offline restore requires a verified cached artifact"),
        "expected the offline failure, found: {}",
        restore_text(&clone, &fresh_home, &["--frozen", "--offline"])
    );

    assert_eq!(
        lockfile_bytes(&clone),
        before,
        "a failed restore must not rewrite the lockfile"
    );
    assert!(
        !clone
            .workspace_import_root("release-notes")
            .join(".agents/skills/release-notes")
            .exists(),
        "a failed restore must not leave dependency state behind"
    );

    // The same checkout restores once a source may be contacted.
    restore(&clone, &fresh_home, &["--frozen"])
        .success()
        .stderr(predicate::str::is_empty());
}

#[test]
fn restore_never_substitutes_the_branch_head_for_a_missing_revision() {
    let fixture = RestoreFixture::new();
    fixture.change_upstream();

    let clone = fixture.clone_portable_state();
    let fresh_home = clone.home_path();
    let upstream_head = fixture.author.git_head("git-source");

    // Point the lockfile at a revision that does not exist upstream.
    let lockfile = String::from_utf8(lockfile_bytes(&clone)).expect("lockfile is utf-8");
    let locked_revision = lockfile
        .lines()
        .find(|line| line.starts_with("      resolved: "))
        .expect("resolved revision is recorded");
    let missing = "0123456789abcdef0123456789abcdef01234567";
    clone.write_lockfile(&lockfile.replace(locked_revision, &format!("      resolved: {missing}")));
    let before = lockfile_bytes(&clone);

    restore(&clone, &fresh_home, &["--frozen"])
        .failure()
        .code(3);
    let failure = restore_text(&clone, &fresh_home, &["--frozen"]);
    assert!(
        failure.contains("is unavailable from"),
        "expected an unavailable-revision failure, found: {failure}"
    );
    assert!(
        failure.contains(missing),
        "expected the fabricated revision in the failure, found: {failure}"
    );
    assert!(
        !failure.contains("git checkout"),
        "a missing revision must be reported by skillctl, not by git's checkout usage error: \
         {failure}"
    );

    assert_eq!(lockfile_bytes(&clone), before);
    assert!(
        !upstream_head.is_empty() && upstream_head != missing,
        "the upstream head must differ from the fabricated revision"
    );
    assert!(
        !clone
            .workspace_import_root("release-notes")
            .join(".agents/skills/release-notes")
            .exists(),
        "no content may be installed for a revision that is not the locked one"
    );

    let report = verify(&clone, &fresh_home);
    assert_eq!(
        report["data"]["imports"][0]["integrity"]["status"],
        "missing"
    );
}

#[test]
fn restore_names_a_mistyped_locked_revision_as_one_the_source_does_not_serve() {
    let fixture = RestoreFixture::new();
    fixture.change_upstream();

    let clone = fixture.clone_portable_state();
    let fresh_home = clone.home_path();

    // Introduce a one-character typo, as a hand edit of the lockfile does.
    // The typo keeps the field a plain token, so the lockfile still parses and
    // the failure has to come from acquiring the revision.
    let original = String::from_utf8(lockfile_bytes(&clone)).expect("lockfile is utf-8");
    let locked_revision = original
        .lines()
        .find(|line| line.starts_with("      resolved: "))
        .expect("resolved revision is recorded");
    let pinned = locked_revision
        .trim_start_matches("      resolved: ")
        .to_string();
    let mistyped = format!("{}g", &pinned[..pinned.len() - 1]);
    assert_ne!(mistyped, pinned, "the typo must change the revision");
    clone
        .write_lockfile(&original.replace(locked_revision, &format!("      resolved: {mistyped}")));
    let before = lockfile_bytes(&clone);

    restore(&clone, &fresh_home, &["--frozen"])
        .failure()
        .code(3);
    let failure = restore_text(&clone, &fresh_home, &["--frozen"]);
    assert!(
        failure.contains("is unavailable from"),
        "expected an unavailable-revision failure, found: {failure}"
    );
    assert!(
        failure.contains("the source was contacted but does not serve this revision"),
        "expected the source-versus-transport distinction, found: {failure}"
    );
    assert!(
        failure.contains(&mistyped),
        "expected the mistyped revision in the failure, found: {failure}"
    );
    assert!(
        !failure.contains("git checkout") && !failure.contains("--detach"),
        "git's checkout usage error must not surface for a mistyped revision: {failure}"
    );

    assert_eq!(
        lockfile_bytes(&clone),
        before,
        "a failed restore must not rewrite the lockfile"
    );

    // Correcting the revision restores the same checkout, so the report is a
    // diagnosis of the pin rather than of the source or the environment.
    clone.write_lockfile(&original);
    restore(&clone, &fresh_home, &["--frozen"])
        .success()
        .stderr(predicate::str::is_empty());
}

#[test]
fn restore_and_verify_refuse_content_that_no_longer_matches_the_pin() {
    let fixture = RestoreFixture::new();
    fixture.change_upstream();

    let clone = fixture.clone_portable_state();
    let home = clone.home_path();

    // Restore once so a cache exists, then tamper with it.
    restore(&clone, &home, &["--frozen"])
        .success()
        .stderr(predicate::str::is_empty());

    // Evict the retained artifact so the tampered cache cannot be repaired.
    let artifact_root = home.join(".skillctl/store/artifacts");
    fs::remove_dir_all(&artifact_root).expect("artifact store is evicted");

    let stored_manifest = clone
        .workspace_import_root("release-notes")
        .join(".agents/skills/release-notes/SKILL.md");
    fs::write(&stored_manifest, "# tampered\n").expect("cached content is modified");
    let before = lockfile_bytes(&clone);

    restore(&clone, &home, &["--frozen"]).failure().code(3);
    let failure = restore_text(&clone, &home, &["--frozen"]);
    assert!(
        failure.contains("cached content does not match the pinned digests"),
        "expected a tampered-cache failure, found: {failure}"
    );
    assert_eq!(lockfile_bytes(&clone), before);

    let report = verify(&clone, &home);
    assert_eq!(report["ok"], false, "verify must fail: {report:#?}");
    assert_eq!(
        report["data"]["imports"][0]["integrity"]["status"],
        "mismatch"
    );

    // The import is a Git source this machine can reach, so the portability
    // reason must blame the mismatched cache, not an unavailable local path.
    assert_eq!(
        report["data"]["imports"][0]["portability"]["source_kind"],
        "git"
    );
    assert_eq!(
        report["data"]["imports"][0]["portability"]["restorable"],
        json!(false)
    );
    let reason = report["data"]["imports"][0]["portability"]["reason"]
        .as_str()
        .expect("reason is a string");
    assert!(
        reason.contains("the cached content no longer matches its pin"),
        "the reason must name the mismatched cache: {reason}"
    );
    assert!(
        !reason.contains("local path"),
        "a git import must not be blamed on an unavailable local path: {reason}"
    );
}

#[test]
fn verify_reports_a_tampered_retained_artifact_without_repairing_it() {
    let fixture = RestoreFixture::new();
    let home = fixture.author.home_path();

    let digest =
        fixture.author.read_lockfile_yaml()["imports"]["release-notes"]["bundle"]["digest"]
            .as_str()
            .expect("bundle digest is recorded")
            .to_string();
    let hex = digest.trim_start_matches("sha256:");
    let retained = home
        .join(".skillctl/store/artifacts")
        .join(hex)
        .join("content/SKILL.md");
    assert!(retained.exists(), "artifact content is retained");
    fs::write(&retained, "# tampered artifact\n").expect("artifact is modified");

    let report = verify(&fixture.author, &home);
    assert_eq!(
        report["ok"], true,
        "cached workspace content still verifies"
    );
    assert!(
        report["warnings"]
            .as_array()
            .expect("warnings exist")
            .iter()
            .any(|warning| warning
                .as_str()
                .expect("warning is a string")
                .contains("retained artifact for 'release-notes' failed verification")),
        "verify must report the tampered artifact: {report:#?}"
    );
}

#[test]
fn verify_keeps_its_warnings_visible_beside_an_integrity_failure() {
    let fixture = RestoreFixture::new();
    let modern = String::from_utf8(lockfile_bytes(&fixture.author)).expect("lockfile is utf-8");
    // A legacy entry records no canonical bundle digest, which verify reports
    // as a warning, and which leaves a tampered cache unrepairable.
    let legacy = rewrite_legacy_lockfile(&modern);

    let clone = fixture.clone_portable_state();
    clone.write_lockfile(&legacy);
    let home = clone.home_path();
    restore(&clone, &home, &["--frozen"])
        .success()
        .stderr(predicate::str::is_empty());

    let stored_manifest = clone
        .workspace_import_root("release-notes")
        .join(".agents/skills/release-notes/SKILL.md");
    fs::write(&stored_manifest, "# tampered\n").expect("cached content is modified");

    let output = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(clone.path())
        .env("HOME", &home)
        .args(["--json", "verify"])
        .output()
        .expect("command runs");
    assert_eq!(
        output.status.code(),
        Some(3),
        "the tampered cache fails verification"
    );
    let report: Value = serde_json::from_slice(&output.stdout).expect("stdout is valid json");
    assert_eq!(report["ok"], false, "verify fails: {report:#?}");

    // `counts` must never report warnings the response body hides.
    let warnings = report["warnings"]
        .as_array()
        .expect("warnings are attached");
    assert_eq!(
        warnings.len() as u64,
        report["data"]["counts"]["warnings"]
            .as_u64()
            .expect("warning count is reported"),
        "every counted warning must be attached: {report:#?}"
    );
    assert!(
        warnings.iter().any(|warning| warning
            .as_str()
            .expect("warning is a string")
            .contains("records no canonical bundle digest")),
        "the legacy-digest hint stays visible beside the failure: {report:#?}"
    );
}

#[test]
fn restore_keeps_retained_snapshots_for_folder_imports() {
    let workspace = TestWorkspace::new();
    workspace.write_manifest(concat!(
        "version: 1\n",
        "\n",
        "targets:\n",
        "  - claude-code\n",
    ));
    workspace.write_skill_source("shared-skills", "release-notes");
    workspace.write_file(
        "shared-skills/.agents/skills/release-notes/references/style.md",
        "# Style\n",
    );
    install(&workspace, &workspace.home_path(), "shared-skills");
    let digest = workspace.read_lockfile_yaml()["imports"]["release-notes"]["bundle"]["digest"]
        .as_str()
        .expect("bundle digest is recorded")
        .to_string();
    assert!(
        workspace
            .home_path()
            .join(".skillctl/store/artifacts")
            .join(digest.trim_start_matches("sha256:"))
            .join("bundle.json")
            .is_file(),
        "a folder install retains an immutable snapshot"
    );

    // The folder source is reachable on another machine only while the same
    // path exists. A fresh checkout with a fresh home cannot reconstruct it.
    let clone = fixture_with_rewritten_source_url(&workspace, "/nonexistent-skillctl-folder");
    restore(&clone, &clone.home_path(), &["--frozen"])
        .failure()
        .code(3);

    // The retained snapshot is enough on its own, without contacting anything.
    let shared_home = workspace.home_path();
    let assert = restore(&clone, &shared_home, &["--frozen", "--offline"])
        .success()
        .stderr(predicate::str::is_empty());
    let body: Value =
        serde_json::from_slice(&assert.get_output().stdout).expect("stdout is valid json");
    assert_eq!(
        body["data"]["imports"][0]["restored_from"],
        "retained-artifact"
    );

    let report = verify(&clone, &shared_home);
    assert_eq!(
        report["data"]["imports"][0]["integrity"]["status"],
        "verified"
    );
    assert_eq!(
        report["data"]["imports"][0]["freshness"]["upstream_check"],
        "unavailable-retained-snapshot"
    );
    assert!(
        clone
            .path()
            .join(".claude/skills/release-notes/references/style.md")
            .is_file(),
        "the retained snapshot projects every inventoried file"
    );
}

#[test]
fn restore_keeps_retained_snapshots_for_zip_imports() {
    let workspace = TestWorkspace::new();
    workspace.write_manifest(concat!(
        "version: 1\n",
        "\n",
        "targets:\n",
        "  - claude-code\n",
    ));
    // The committed fixture is copied in so the recorded source URL points
    // inside this workspace.
    let archive = workspace.path().join("release-notes.zip");
    fs::copy(
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/archives/release-notes.zip"
        ),
        &archive,
    )
    .expect("fixture archive is copied");

    install(
        &workspace,
        &workspace.home_path(),
        archive.to_str().expect("archive path is utf-8"),
    );
    let lockfile = workspace.read_lockfile_yaml();
    assert_eq!(
        lockfile["imports"]["release-notes"]["source"]["type"],
        "archive"
    );
    let digest = lockfile["imports"]["release-notes"]["bundle"]["digest"]
        .as_str()
        .expect("bundle digest is recorded")
        .to_string();

    // A fresh checkout with a fresh home cannot reach the recorded archive, so
    // only a retained snapshot can reproduce it.
    let clone = fixture_with_rewritten_source_url(&workspace, "file:///nonexistent/archive.zip");
    restore(&clone, &clone.home_path(), &["--frozen"])
        .failure()
        .code(3);

    let shared_home = workspace.home_path();
    let assert = restore(&clone, &shared_home, &["--frozen", "--offline"])
        .success()
        .stderr(predicate::str::is_empty());
    let body: Value =
        serde_json::from_slice(&assert.get_output().stdout).expect("stdout is valid json");
    assert_eq!(
        body["data"]["imports"][0]["restored_from"],
        "retained-artifact"
    );
    assert_eq!(body["data"]["imports"][0]["bundle_digest"], digest);

    let report = verify(&clone, &shared_home);
    assert_eq!(
        report["data"]["imports"][0]["integrity"]["status"],
        "verified"
    );
    assert!(
        clone
            .path()
            .join(".claude/skills/release-notes/SKILL.md")
            .is_file(),
        "the zip snapshot projects the archived skill"
    );
}

/// Clone the portable state while pointing the recorded source elsewhere.
fn fixture_with_rewritten_source_url(
    workspace: &TestWorkspace,
    replacement: &str,
) -> TestWorkspace {
    let clone = TestWorkspace::new();
    for relative in PORTABLE_STATE {
        let source = workspace.path().join(relative);
        if !source.is_file() {
            continue;
        }
        let contents = fs::read_to_string(&source)
            .unwrap_or_else(|error| panic!("{relative} is readable: {error}"));
        clone.write_file(
            relative,
            &contents.replace(&file_url(workspace.path()), replacement),
        );
    }
    clone
}

#[test]
fn restore_reports_nonportable_local_path_sources_instead_of_guessing() {
    let workspace = TestWorkspace::new();
    workspace.write_manifest(concat!("version: 1\n", "\n", "targets:\n", "  - codex\n",));
    workspace.write_skill_source("shared-skills", "release-notes");
    install(&workspace, &workspace.home_path(), "shared-skills");

    // Rewrite the recorded source URL so the path cannot exist here. The
    // replacement keeps a shape `Url::to_file_path` resolves on every
    // platform, because the failure under test is the unreachable path, not
    // an unresolvable URL.
    let missing_source = workspace.path().join("nonexistent-skillctl-source");
    let clone = fixture_with_rewritten_source_url(&workspace, &file_url(&missing_source));
    let fresh_home = clone.home_path();
    let before = lockfile_bytes(&clone);

    restore(&clone, &fresh_home, &["--frozen"])
        .failure()
        .code(3);
    let failure = restore_text(&clone, &fresh_home, &["--frozen"]);
    assert!(
        failure.contains(&format!(
            "locked local source '{}/shared-skills/' is not available",
            portable(&missing_source)
        )),
        "expected a nonportable-source failure, found: {failure}"
    );
    assert!(
        failure.contains("skillctl remove <skill>"),
        "expected an actionable fix, found: {failure}"
    );
    assert_eq!(lockfile_bytes(&clone), before);

    let report = verify(&clone, &fresh_home);
    assert_eq!(
        report["ok"], false,
        "an unreachable local source is not restorable"
    );
    assert_eq!(
        report["data"]["imports"][0]["portability"]["restorable"].as_bool(),
        Some(false)
    );
    // The local-path wording is reserved for this case: missing content, an
    // unreachable local source, and no verified retained artifact.
    assert_eq!(
        report["data"]["imports"][0]["portability"]["reason"],
        concat!(
            "the recorded source is a local path that is unavailable here and no verified ",
            "artifact is retained"
        )
    );
    assert_eq!(
        report["data"]["imports"][0]["freshness"]["upstream_check"],
        "unavailable-retained-snapshot"
    );
}

#[test]
fn restore_keeps_retained_snapshots_for_archive_imports() {
    let workspace = TestWorkspace::new();
    workspace.write_manifest(concat!("version: 1\n", "\n", "targets:\n", "  - codex\n",));
    workspace.write_skill_source("archive-source", "release-notes");
    let archive = workspace.path().join("release-notes.tar.gz");
    let archive_status = ProcessCommand::new("tar")
        .args([
            "-czf",
            archive.to_str().expect("archive path is utf-8"),
            ".",
        ])
        .current_dir(workspace.path().join("archive-source"))
        .status()
        .expect("tar launches");
    assert!(archive_status.success(), "fixture archive is created");

    install(
        &workspace,
        &workspace.home_path(),
        archive.to_str().expect("path is utf-8"),
    );
    let digest = workspace.read_lockfile_yaml()["imports"]["release-notes"]["bundle"]["digest"]
        .as_str()
        .expect("bundle digest is recorded")
        .to_string();

    // A fresh checkout with a fresh home restores only from the retained
    // snapshot, because an archive import has no upstream to contact.
    let clone = TestWorkspace::new();
    for relative in PORTABLE_STATE {
        let source = workspace.path().join(relative);
        if !source.is_file() {
            continue;
        }
        let contents = fs::read_to_string(&source)
            .unwrap_or_else(|error| panic!("{relative} is readable: {error}"));
        clone.write_file(
            relative,
            &contents.replace(
                &file_url(&archive),
                "file:///nonexistent-skillctl-archive.tar.gz",
            ),
        );
    }
    let fresh_home = clone.home_path();

    restore(&clone, &fresh_home, &["--frozen", "--offline"])
        .failure()
        .code(3);
    assert!(
        restore_text(&clone, &fresh_home, &["--frozen", "--offline"])
            .contains("offline restore requires a verified cached artifact"),
        "expected the offline failure, found: {}",
        restore_text(&clone, &fresh_home, &["--frozen", "--offline"])
    );

    // With the artifact store carried over, the retained snapshot is enough.
    let shared_home = workspace.home_path();
    let assert = restore(&clone, &shared_home, &["--frozen", "--offline"])
        .success()
        .stderr(predicate::str::is_empty());
    let body: Value =
        serde_json::from_slice(&assert.get_output().stdout).expect("stdout is valid json");
    assert_eq!(
        body["data"]["imports"][0]["restored_from"],
        "retained-artifact"
    );
    assert_eq!(body["data"]["imports"][0]["bundle_digest"], digest);
    let subpath = clone.read_lockfile_yaml()["imports"]["release-notes"]["source"]["subpath"]
        .as_str()
        .expect("subpath is recorded")
        .to_string();
    let store_root = shared_home.join(
        clone
            .workspace_import_root_relative("release-notes")
            .strip_prefix("home")
            .expect("the store path is home relative"),
    );
    assert!(
        store_root.join(&subpath).join("SKILL.md").exists(),
        "the retained snapshot materializes the import at {}",
        store_root.join(&subpath).display()
    );
    let _ = fresh_home;
}

#[test]
fn restore_upgrades_a_legacy_lockfile_without_moving_any_pin() {
    let fixture = RestoreFixture::new();
    let modern = String::from_utf8(lockfile_bytes(&fixture.author)).expect("lockfile is utf-8");

    let legacy = rewrite_legacy_lockfile(&modern);

    let clone = fixture.clone_portable_state();
    clone.write_lockfile(&legacy);
    let fresh_home = clone.home_path();

    // Frozen restores never rewrite the lockfile, including legacy ones.
    restore(&clone, &fresh_home, &["--frozen"])
        .success()
        .stderr(predicate::str::is_empty());
    assert_eq!(lockfile_bytes(&clone), legacy.as_bytes());

    // A plain restore performs the deliberate upgrade and records the digest of
    // the content it verified, without moving the pinned revision.
    let pinned_revision =
        clone.read_lockfile_yaml()["imports"]["release-notes"]["revision"]["resolved"]
            .as_str()
            .expect("revision is pinned")
            .to_string();
    restore(&clone, &fresh_home, &[])
        .success()
        .stderr(predicate::str::is_empty());

    let upgraded = clone.read_lockfile_yaml();
    assert_eq!(upgraded["version"].as_u64(), Some(2));
    assert!(
        !upgraded["imports"]["release-notes"]
            .as_mapping()
            .expect("import exists")
            .contains_key("timestamps"),
        "the upgrade drops local bookkeeping"
    );
    assert_eq!(
        upgraded["imports"]["release-notes"]["revision"]["resolved"]
            .as_str()
            .expect("revision is pinned"),
        pinned_revision,
        "the upgrade must not move the pinned revision"
    );
    assert_eq!(
        upgraded["imports"]["release-notes"]["bundle"]["digest"],
        fixture.author.read_lockfile_yaml()["imports"]["release-notes"]["bundle"]["digest"],
        "the upgrade records the digest of the verified content"
    );
}

#[test]
fn restore_reports_a_missing_lockfile_and_an_unpinned_import() {
    let workspace = TestWorkspace::new();
    workspace.write_manifest(concat!("version: 1\n", "\n", "targets:\n", "  - codex\n",));

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--json", "restore", "--frozen"])
        .assert()
        .failure()
        .code(3)
        .stdout(predicate::str::contains("no lockfile at"));

    workspace.write_lockfile(MINIMAL_LOCKFILE.as_str());
    workspace.write_file(
        ".agents/skillctl.yaml",
        concat!(
            "version: 1\n",
            "\n",
            "imports:\n",
            "  - id: release-notes\n",
            "    type: git\n",
            "    url: https://example.invalid/skills.git\n",
            "    ref: HEAD\n",
            "    path: .agents/skills/release-notes\n",
            "    scope: workspace\n",
            "    enabled: true\n",
            "\n",
            "targets:\n",
            "  - codex\n",
        ),
    );
    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--json", "restore", "--frozen"])
        .assert()
        .failure()
        .code(3)
        .stdout(predicate::str::contains(
            "enabled import 'release-notes' is missing from",
        ));
}

#[test]
fn verify_distinguishes_a_missing_cache_from_broken_content() {
    let fixture = RestoreFixture::new();
    let clone = fixture.clone_portable_state();
    let fresh_home = clone.home_path();

    // A fresh clone is consistent but not materialized: a warning, not a failure.
    let report = verify(&clone, &fresh_home);
    assert_eq!(report["ok"], true, "verify failed: {report:#?}");
    assert_eq!(
        report["data"]["imports"][0]["integrity"]["status"],
        "missing"
    );
    assert_eq!(
        report["data"]["imports"][0]["portability"]["restorable"].as_bool(),
        Some(true)
    );

    restore(&clone, &fresh_home, &["--frozen"])
        .success()
        .stderr(predicate::str::is_empty());
    let report = verify(&clone, &fresh_home);
    assert_eq!(report["ok"], true);
    assert_eq!(report["data"]["counts"]["verified"].as_u64(), Some(1));
}

/// Turn a current lockfile body into an equivalent version 1 document.
///
/// The version is lowered, the canonical bundle block is removed, and the
/// legacy per-import timestamp block is restored.
fn rewrite_legacy_lockfile(modern: &str) -> String {
    let mut legacy = String::new();
    let mut skip_bundle_fields = 0_usize;
    for line in modern.lines() {
        if line == "    bundle:" {
            skip_bundle_fields = 4;
            continue;
        }
        if skip_bundle_fields > 0 {
            skip_bundle_fields -= 1;
            continue;
        }

        let line = if line == "version: 2" {
            "version: 1".to_string()
        } else {
            line.to_string()
        };
        if line == "    hashes:" {
            legacy.push_str("    timestamps:\n");
            legacy.push_str("      fetched_at: 2026-01-01T00:00:00Z\n");
            legacy.push_str("      first_installed_at: 2026-01-01T00:00:00Z\n");
            legacy.push_str("      last_updated_at: 2026-01-01T00:00:00Z\n");
        }
        legacy.push_str(&line);
        legacy.push('\n');
    }

    legacy
}

/// A checkout whose import carries an overlay the team commits.
struct OverlayFixture {
    /// Author checkout that installs, overrides, and commits the overlay.
    author: TestWorkspace,
}

impl OverlayFixture {
    fn new() -> Self {
        let workspace = TestWorkspace::new();
        workspace.write_manifest(concat!(
            "version: 1\n",
            "\n",
            "targets:\n",
            "  - claude-code\n",
        ));
        workspace.write_skill_source("git-source", "release-notes");
        workspace.init_git_repo("git-source");
        let repo_url = workspace.git_repo_url("git-source");
        install(&workspace, &workspace.home_path(), &repo_url);

        // The team customizes the import through an overlay and commits it, so
        // the lockfile pins the overlay content instead of sha256:none.
        run_override(&workspace, "release-notes");
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
        run_override(&workspace, "release-notes");

        Self { author: workspace }
    }

    /// The overlay digest the author's lockfile pins.
    fn pinned_overlay_digest(&self) -> String {
        self.author.read_lockfile_yaml()["imports"]["release-notes"]["hashes"]["overlay"]
            .as_str()
            .expect("overlay digest is recorded")
            .to_string()
    }

    /// Clone the portable state minus the overlay directory.
    fn clone_without_overlays(&self) -> TestWorkspace {
        let clone = TestWorkspace::new();
        copy_portable_state(&self.author, &clone);
        fs::remove_dir_all(clone.path().join(".agents/overlays"))
            .expect("the cloned overlay directory is removed");
        clone
    }

    /// Clone the portable state, overlays included.
    fn clone_with_overlays(&self) -> TestWorkspace {
        let clone = TestWorkspace::new();
        copy_portable_state(&self.author, &clone);
        clone
    }
}

fn run_override(workspace: &TestWorkspace, skill: &str) {
    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--json", "--no-input", "override", skill])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
}

#[test]
fn restore_frozen_reproduces_an_overlaid_import_from_committed_portable_state() {
    let fixture = OverlayFixture::new();
    let pinned_overlay_digest = fixture.pinned_overlay_digest();
    assert_ne!(
        pinned_overlay_digest, "sha256:none",
        "the author must pin real overlay content"
    );
    let committed_lockfile = lockfile_bytes(&fixture.author);
    let overlay_manifest = fs::read_to_string(
        fixture
            .author
            .path()
            .join(".agents/overlays/release-notes/SKILL.md"),
    )
    .expect("the author overlay is readable");

    // A teammate clones the documented portable state, overlays included.
    let clone = fixture.clone_with_overlays();
    let fresh_home = clone.home_path();

    restore(&clone, &fresh_home, &["--frozen"])
        .success()
        .stderr(predicate::str::is_empty());
    assert_eq!(
        lockfile_bytes(&clone),
        committed_lockfile,
        "a frozen restore must not rewrite the lockfile"
    );

    // The projection is the overlaid content the lockfile pins.
    assert_eq!(
        fs::read_to_string(clone.path().join(".claude/skills/release-notes/SKILL.md"))
            .expect("the projected manifest is readable"),
        overlay_manifest,
    );

    let report = verify(&clone, &fresh_home);
    assert_eq!(report["ok"], true, "verify failed: {report:#?}");
    assert_eq!(
        report["data"]["imports"][0]["overlay"]["status"],
        json!("verified")
    );
    assert_eq!(
        report["data"]["imports"][0]["overlay"]["pinned_digest"],
        json!(pinned_overlay_digest)
    );
    assert_eq!(
        report["data"]["imports"][0]["portability"]["restorable"],
        json!(true)
    );
}

#[test]
fn restore_frozen_names_the_missing_overlay_directory_and_its_pinned_digest() {
    let fixture = OverlayFixture::new();
    let pinned_overlay_digest = fixture.pinned_overlay_digest();
    let clone = fixture.clone_without_overlays();
    let fresh_home = clone.home_path();
    let before = lockfile_bytes(&clone);

    let output = restore(&clone, &fresh_home, &["--frozen"])
        .failure()
        .code(3)
        .get_output()
        .clone();
    let failure = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let overlay_directory = clone.path().join(".agents/overlays/release-notes");
    assert!(
        failure.contains(&portable(&overlay_directory)),
        "the failure must name the missing overlay directory, found: {failure}"
    );
    assert!(
        failure.contains(&pinned_overlay_digest),
        "the failure must name the pinned overlay digest, found: {failure}"
    );
    assert!(
        failure.contains(".agents/skillctl.yaml"),
        "the failure must point at the committable portable state, found: {failure}"
    );
    assert_eq!(lockfile_bytes(&clone), before);

    // The refusal happens before any content is acquired or retained.
    assert!(
        !fresh_home.join(".skillctl/store/artifacts").exists(),
        "a refused restore must not populate the artifact store"
    );
    assert!(
        !clone.workspace_import_root("release-notes").exists(),
        "a refused restore must not populate the import cache"
    );

    // Committing the documented portable state is the fix.
    clone.copy_path(
        &fixture.author.path().join(".agents/overlays/release-notes"),
        ".agents/overlays/release-notes",
    );
    restore(&clone, &fresh_home, &["--frozen"])
        .success()
        .stderr(predicate::str::is_empty());
    let report = verify(&clone, &fresh_home);
    assert_eq!(report["ok"], true, "verify failed: {report:#?}");
}

#[test]
fn verify_reports_an_absent_pinned_overlay_as_not_restorable() {
    let fixture = OverlayFixture::new();
    let pinned_overlay_digest = fixture.pinned_overlay_digest();
    let clone = fixture.clone_without_overlays();
    let fresh_home = clone.home_path();

    let report = verify(&clone, &fresh_home);
    assert_eq!(
        report["ok"], false,
        "an absent pinned overlay is not restorable: {report:#?}"
    );
    assert_eq!(
        report["data"]["imports"][0]["overlay"]["status"],
        json!("missing")
    );
    assert_eq!(
        report["data"]["imports"][0]["overlay"]["path"],
        json!(".agents/overlays/release-notes")
    );
    assert_eq!(
        report["data"]["imports"][0]["overlay"]["pinned_digest"],
        json!(pinned_overlay_digest)
    );
    assert_eq!(
        report["data"]["imports"][0]["portability"]["restorable"].as_bool(),
        Some(false)
    );
    assert!(
        report["data"]["integrity_failures"][0]
            .as_str()
            .expect("an integrity failure is recorded")
            .contains(&pinned_overlay_digest),
        "the failure must name the pinned digest: {report:#?}"
    );
}

#[test]
fn restore_frozen_refuses_an_overlay_that_no_longer_matches_its_pin() {
    let fixture = OverlayFixture::new();
    let pinned_overlay_digest = fixture.pinned_overlay_digest();
    let clone = fixture.clone_with_overlays();
    let fresh_home = clone.home_path();
    let before = lockfile_bytes(&clone);

    clone.write_file(
        ".agents/overlays/release-notes/SKILL.md",
        concat!(
            "---\n",
            "name: release-notes\n",
            "description: Edited behind the lockfile's back.\n",
            "---\n",
            "\n",
            "# Release notes\n",
        ),
    );

    let output = restore(&clone, &fresh_home, &["--frozen"])
        .failure()
        .code(3)
        .get_output()
        .clone();
    let failure = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        failure.contains(&pinned_overlay_digest),
        "the failure must name the pinned overlay digest, found: {failure}"
    );
    assert!(
        failure.contains("skillctl override release-notes"),
        "the failure must name the re-pinning command, found: {failure}"
    );
    assert_eq!(lockfile_bytes(&clone), before);

    let report = verify(&clone, &fresh_home);
    assert_eq!(
        report["data"]["imports"][0]["overlay"]["status"],
        json!("mismatch")
    );
    assert_eq!(
        report["data"]["imports"][0]["portability"]["restorable"].as_bool(),
        Some(false)
    );
}

#[test]
fn quickstart_flow_re_pins_the_edited_overlay_before_it_is_committed() {
    // The author follows the README quickstart: install, override, edit the
    // overlay, sync, re-pin, and commit the portable state.
    let author = TestWorkspace::new();
    author.write_manifest(concat!(
        "version: 1\n",
        "\n",
        "targets:\n",
        "  - claude-code\n",
    ));
    author.write_skill_source("git-source", "release-notes");
    author.init_git_repo("git-source");
    install(
        &author,
        &author.home_path(),
        &author.git_repo_url("git-source"),
    );
    run_override(&author, "release-notes");

    let edited_overlay = concat!(
        "---\n",
        "name: release-notes\n",
        "description: Team-tailored release notes skill.\n",
        "---\n",
        "\n",
        "# Release notes, team edition\n",
    );
    author.write_file(".agents/overlays/release-notes/SKILL.md", edited_overlay);
    let stale_digest = author.read_lockfile_yaml()["imports"]["release-notes"]["hashes"]["overlay"]
        .as_str()
        .expect("overlay digest is recorded")
        .to_string();

    // Sync projects the edit but refuses to stay silent about the stale pin.
    let sync = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(author.path())
        .env("HOME", author.home_path())
        .args(["--json", "sync"])
        .assert()
        .code(2)
        .get_output()
        .clone();
    let body: Value = serde_json::from_slice(&sync.stdout).expect("stdout is valid json");
    assert!(
        body["warnings"]
            .as_array()
            .expect("warnings are an array")
            .iter()
            .any(|warning| warning
                .as_str()
                .expect("warning is a string")
                .contains("skillctl override release-notes")),
        "the warning must name the re-pinning command: {body:#?}"
    );

    // The quickstart's re-pin step is what makes the committed pair restorable.
    run_override(&author, "release-notes");
    assert_ne!(
        author.read_lockfile_yaml()["imports"]["release-notes"]["hashes"]["overlay"]
            .as_str()
            .expect("overlay digest is recorded"),
        stale_digest
    );

    let clone = TestWorkspace::new();
    copy_portable_state(&author, &clone);
    let fresh_home = clone.home_path();

    restore(&clone, &fresh_home, &["--frozen"])
        .success()
        .stderr(predicate::str::is_empty());

    assert_eq!(
        fs::read_to_string(clone.path().join(".claude/skills/release-notes/SKILL.md"))
            .expect("the projected manifest is readable"),
        edited_overlay,
        "the teammate restores the overlaid content the lockfile pins"
    );
    let report = verify(&clone, &fresh_home);
    assert_eq!(report["ok"], true, "verify failed: {report:#?}");
    assert_eq!(
        report["data"]["imports"][0]["overlay"]["status"],
        json!("verified")
    );
}

#[test]
fn verify_and_restore_name_init_when_the_workspace_holds_no_manifest() {
    let workspace = TestWorkspace::new();
    workspace.write_workspace_skill("bare", "Bare skill.", &[]);
    let home = workspace.home_path();

    // `sync` keeps the documented local-only flow moving on the defaults, so
    // the refusals below are a deliberate split: `verify` and `restore`
    // answer from the imports a manifest declares, and none are declared.
    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", &home)
        .arg("sync")
        .assert()
        .code(2)
        .stdout(predicate::str::contains("skillctl init"));

    for (label, args) in [
        ("verify", vec!["verify"]),
        ("frozen restore", vec!["restore", "--frozen"]),
        ("plain restore", vec!["restore"]),
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
            "{label} must refuse as a validation failure"
        );
        assert!(
            output.stdout.is_empty(),
            "{label} reports the refusal on stderr, not as a report"
        );
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        assert!(
            stderr.contains(".agents/skillctl.yaml"),
            "{label} must name the missing manifest: {stderr}"
        );
        assert!(
            stderr.contains("skillctl init"),
            "{label} must name the repair: {stderr}"
        );
        assert!(
            !stderr.contains("os error"),
            "{label} must not surface the raw filesystem error: {stderr}"
        );
    }

    // The machine contract carries the same refusal, because the runtime wraps
    // the typed error in the JSON body.
    let output = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", &home)
        .args(["--json", "verify"])
        .output()
        .expect("command runs");
    let body: Value = serde_json::from_slice(&output.stdout).expect("stdout is valid json");
    assert_eq!(output.status.code(), Some(3), "the JSON run fails too");
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
                .contains("skillctl init")),
        "the JSON error must name the repair: {body:#?}"
    );

    // The named repair is real: once `init` declares the workspace, both
    // commands answer instead of refusing.
    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", &home)
        .arg("init")
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", &home)
        .arg("verify")
        .assert()
        .success()
        .stderr(predicate::str::is_empty())
        .stdout(predicate::str::contains(
            "Verified 0 of 0 pinned imports. Integrity clean",
        ));

    restore(&workspace, &home, &["--frozen"])
        .success()
        .stderr(predicate::str::is_empty());
}

/// `verify` and `restore` in a workspace that holds a manifest and an install
/// but lost its lockfile.
///
/// The manifest survives, so the missing-manifest refusal does not apply, and
/// an empty lockfile would verify zero imports and read as a healthy
/// workspace. Both commands refuse and name the two repairs: the committed
/// lockfile for a team checkout, `skillctl init` for a workspace that pinned
/// nothing yet.
#[test]
fn verify_and_restore_name_both_repairs_when_the_manifest_survives_a_lost_lockfile() {
    let workspace = TestWorkspace::new();
    workspace.write_skill_source("shared-skills", "release-notes");
    let home = workspace.home_path();
    install(&workspace, &home, "shared-skills");

    let committed_lockfile =
        String::from_utf8(lockfile_bytes(&workspace)).expect("lockfile is utf-8");
    fs::remove_file(workspace.path().join(".agents/skillctl.lock"))
        .expect("the lockfile is removed");

    for (label, args) in [
        ("verify", vec!["verify"]),
        ("frozen restore", vec!["restore", "--frozen"]),
        ("plain restore", vec!["restore"]),
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
            "{label} must refuse as a validation failure"
        );
        assert!(
            output.stdout.is_empty(),
            "{label} reports the refusal on stderr, not as a report"
        );
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        assert!(
            stderr.contains(".agents/skillctl.lock"),
            "{label} must name the missing lockfile: {stderr}"
        );
        assert!(
            stderr.contains("retrieve the committed .agents/skillctl.lock"),
            "{label} must name the team-checkout repair: {stderr}"
        );
        assert!(
            stderr.contains("skillctl init"),
            "{label} must name the new-workspace repair: {stderr}"
        );
        assert!(
            !stderr.contains("os error"),
            "{label} must not surface the raw filesystem error: {stderr}"
        );
    }

    // The refusal never claims the workspace is healthy: until the lockfile
    // returns, `verify` answers nothing.
    let output = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", &home)
        .args(["--json", "verify"])
        .output()
        .expect("command runs");
    let body: Value = serde_json::from_slice(&output.stdout).expect("stdout is valid json");
    assert_eq!(output.status.code(), Some(3), "the JSON run refuses too");
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
                .contains("retrieve the committed .agents/skillctl.lock")),
        "the JSON error must name the team-checkout repair: {body:#?}"
    );

    // The named team-checkout repair is real: the retrieved lockfile brings
    // the pins back, and `verify` reports one import instead of zero.
    workspace.write_file(".agents/skillctl.lock", &committed_lockfile);
    let report = verify(&workspace, &home);
    assert_eq!(report["ok"], true, "verify failed: {report:#?}");
    assert_eq!(
        report["data"]["imports"].as_array().map(Vec::len),
        Some(1),
        "the retrieved lockfile still pins one import: {report:#?}"
    );
}

#[test]
fn verify_refuses_instead_of_calling_a_lockfile_only_checkout_stale() {
    let workspace = TestWorkspace::new();
    let mut lockfile = MINIMAL_LOCKFILE.as_str().to_string();
    lockfile.push_str(concat!(
        "\nimports:\n",
        "  release-notes:\n",
        "    source:\n",
        "      type: local-path\n",
        "      url: file:///tmp/release-notes\n",
        "      subpath: .agents/skills/release-notes\n",
        "    revision:\n",
        "      resolved: deadbeef\n",
        "    hashes:\n",
        "      content: sha256:content\n",
        "      overlay: sha256:none\n",
        "      effective_version: sha256:effective\n",
    ));
    workspace.write_lockfile(&lockfile);

    // The checkout pins an import nothing declares. Running `verify` on the
    // documented defaults would invent that empty declaration and call the pin
    // stale, so the check refuses and names the missing manifest instead.
    let output = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .arg("verify")
        .output()
        .expect("command runs");
    let report = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        output.status.code(),
        Some(3),
        "verify must refuse rather than default: {report}"
    );
    assert!(
        report.contains(".agents/skillctl.yaml") && report.contains("skillctl init"),
        "the refusal must name the missing manifest and the repair: {report}"
    );
    assert!(
        !report.to_lowercase().contains("stale"),
        "an invented empty declaration must not label the pin: {report}"
    );
}

/// Author checkout whose manifest pins one workspace import and one user
/// import, so a restore has to reconstruct two scopes from one portable pair.
struct MixedScopeFixture {
    author: TestWorkspace,
}

impl MixedScopeFixture {
    fn new() -> Self {
        let workspace = TestWorkspace::new();
        workspace.write_manifest(concat!(
            "version: 1\n",
            "\n",
            "targets:\n",
            "  - claude-code\n",
        ));
        workspace.write_skill_source("git-source", "release-notes");
        workspace.init_git_repo("git-source");
        workspace.write_skill_source("user-git-source", "ai-sdk");
        workspace.init_git_repo("user-git-source");
        let home = workspace.home_path();

        install(&workspace, &home, &workspace.git_repo_url("git-source"));
        install_scoped(
            &workspace,
            &home,
            &workspace.git_repo_url("user-git-source"),
            "ai-sdk",
            "user",
        );

        Self { author: workspace }
    }

    /// Clone only the declared portable state into a new checkout.
    fn clone_portable_state(&self) -> TestWorkspace {
        let clone = TestWorkspace::new();
        copy_portable_state(&self.author, &clone);
        clone
    }

    /// The effective-version digest the author's lockfile pins for one import.
    fn pinned_effective_version(&self, import_id: &str) -> String {
        self.author.read_lockfile_yaml()["imports"][import_id]["hashes"]["effective_version"]
            .as_str()
            .unwrap_or_else(|| panic!("{import_id} pins an effective version"))
            .to_string()
    }
}

/// Install one import under an explicit scope.
fn install_scoped(
    workspace: &TestWorkspace,
    home: &Path,
    source: &str,
    name: &str,
    scope: &str,
) -> Value {
    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", home)
        .args([
            "--json",
            "--no-input",
            "--name",
            name,
            "--scope",
            scope,
            "install",
            source,
        ])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    let body: Value =
        serde_json::from_slice(&assert.get_output().stdout).expect("stdout is valid json");
    assert_eq!(body["ok"], true, "install failed: {body:#?}");
    body
}

/// Restore with an explicit global scope flag.
fn restore_with_scope(
    workspace: &TestWorkspace,
    home: &Path,
    scope: &str,
    flags: &[&str],
) -> assert_cmd::assert::Assert {
    let mut args = vec!["--json", "--scope", scope, "restore"];
    args.extend_from_slice(flags);
    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", home)
        .args(&args)
        .assert()
}

/// `(physical_root, effective_version_hash)` of one skill's projection records,
/// read straight from the state database a restore refreshes.
fn projection_state(home: &Path, scope: &str, skill_id: &str) -> Vec<(String, String)> {
    let connection =
        Connection::open(home.join(".skillctl/state.db")).expect("state database opens");
    let mut statement = connection
        .prepare(
            "SELECT physical_root, effective_version_hash FROM projection_records \
             WHERE scope = ?1 AND skill_id = ?2 ORDER BY physical_root",
        )
        .expect("projection query prepares");
    statement
        .query_map(params![scope, skill_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .expect("projection query runs")
        .collect::<Result<Vec<_>, _>>()
        .expect("projection rows read")
}

#[test]
fn restore_materializes_each_scope_of_a_mixed_manifest() {
    let fixture = MixedScopeFixture::new();
    let workspace_effective = fixture.pinned_effective_version("release-notes");
    let user_effective = fixture.pinned_effective_version("ai-sdk");

    // A teammate clones the portable state with a fresh home and runs the
    // documented default-scope restore.
    let clone = fixture.clone_portable_state();
    let fresh_home = clone.home_path();
    let assert = restore(&clone, &fresh_home, &["--frozen"])
        .success()
        .stderr(predicate::str::is_empty());

    let body: Value =
        serde_json::from_slice(&assert.get_output().stdout).expect("stdout is valid json");
    assert_eq!(body["ok"], true, "restore failed: {body:#?}");
    let restored: BTreeMap<&str, &Value> = body["data"]["imports"]
        .as_array()
        .expect("imports report")
        .iter()
        .map(|import| (import["skill"].as_str().expect("skill is a string"), import))
        .collect();
    assert_eq!(restored.len(), 2, "both imports are reported: {body:#?}");
    assert_eq!(restored["release-notes"]["scope"], "workspace");
    assert_eq!(
        restored["release-notes"]["restored_from"],
        "source-at-locked-revision"
    );
    assert_eq!(restored["ai-sdk"]["scope"], "user");
    assert_eq!(
        restored["ai-sdk"]["restored_from"],
        "source-at-locked-revision"
    );
    assert_eq!(restored["ai-sdk"]["verified"], true);

    // Both scopes were materialized, each into its own runtime roots.
    assert!(
        clone
            .path()
            .join(".claude/skills/release-notes/SKILL.md")
            .is_file(),
        "the workspace import is projected into the workspace root"
    );
    assert!(
        fresh_home.join(".claude/skills/ai-sdk/SKILL.md").is_file(),
        "the user import is projected into the user root"
    );

    // Each scope's projection state survives and describes its own projection:
    // the user record carries the user root and the user effective digest, not
    // a workspace one.
    assert_eq!(
        projection_state(&fresh_home, "workspace", "release-notes"),
        vec![(".claude/skills".to_string(), workspace_effective)],
        "the workspace record must describe the workspace projection"
    );
    assert_eq!(
        projection_state(&fresh_home, "user", "ai-sdk"),
        vec![("~/.claude/skills".to_string(), user_effective)],
        "the user record must describe the user projection"
    );

    // A mixed restore reports each scope's projections separately.
    let reported_scopes: Vec<&str> = body["data"]["scope_projections"]
        .as_array()
        .expect("a mixed restore reports per-scope projections")
        .iter()
        .map(|entry| entry["scope"].as_str().expect("scope is a string"))
        .collect();
    assert_eq!(reported_scopes, vec!["workspace", "user"]);

    let report = verify(&clone, &fresh_home);
    assert_eq!(report["ok"], true, "verify failed: {report:#?}");
    assert_eq!(report["data"]["counts"]["verified"].as_u64(), Some(2));
}

#[test]
fn restore_with_an_explicit_user_scope_still_restores_the_workspace_scope() {
    let fixture = MixedScopeFixture::new();

    // The invocation selects the user scope, but the committed manifest pins a
    // workspace import too, so a restore reconstructs both.
    let clone = fixture.clone_portable_state();
    let fresh_home = clone.home_path();
    let assert = restore_with_scope(&clone, &fresh_home, "user", &["--frozen"])
        .success()
        .stderr(predicate::str::is_empty());

    let body: Value =
        serde_json::from_slice(&assert.get_output().stdout).expect("stdout is valid json");
    assert_eq!(body["ok"], true, "restore failed: {body:#?}");

    assert!(
        clone
            .path()
            .join(".claude/skills/release-notes/SKILL.md")
            .is_file(),
        "the workspace import is projected even under --scope user"
    );
    assert!(
        fresh_home.join(".claude/skills/ai-sdk/SKILL.md").is_file(),
        "the user import is projected into the user root"
    );
    assert_eq!(
        projection_state(&fresh_home, "workspace", "release-notes").len(),
        1,
        "the user-scope restore must not wipe the workspace projection record"
    );
    assert_eq!(
        projection_state(&fresh_home, "user", "ai-sdk").len(),
        1,
        "the user projection record is written"
    );
}

#[test]
fn an_empty_home_env_refuses_instead_of_rooting_stores_at_the_working_directory() {
    let fixture = RestoreFixture::new();
    let clone = fixture.clone_portable_state();

    // A CI job may export HOME with an empty value. The state resolver has
    // always filtered it, and every home-rooted store shares that resolver,
    // so the command refuses instead of silently rooting the artifact and
    // import stores at the working directory.
    let output = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(clone.path())
        .env("HOME", "")
        .env_remove("USERPROFILE")
        .env_remove("HOMEDRIVE")
        .env_remove("HOMEPATH")
        .args(["--json", "verify"])
        .output()
        .expect("verify runs");
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(
        !output.status.success(),
        "an empty HOME must not verify as if the workspace held stores: {text}"
    );
    assert!(
        text.contains("failed to determine a home directory"),
        "expected the home-directory refusal, found: {text}"
    );
    assert!(
        !clone.path().join(".skillctl").exists(),
        "no store may be rooted at the working directory"
    );
}
