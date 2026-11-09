//! Frozen restore and read-only verification of locked dependencies.
//!
//! `skillctl restore --frozen` reconstructs every enabled import from the
//! portable lockfile and the committed overlays. It rebuilds an empty import
//! cache, verifies the reconstructed content against the digests recorded in
//! the lockfile, and leaves the lockfile byte-for-byte unchanged. It never
//! selects a newer revision: a locked revision that is no longer available is a
//! failure, not an opportunity to advance.
//!
//! The lockfile pins the digest of the overlay an import uses, but it does not
//! carry the overlay files. Overlays are committable portable state that lives
//! beside the manifest and the lockfile, so a checkout that omits them cannot
//! reproduce the pinned effective version. Both commands report that state
//! before any content is acquired.
//!
//! `skillctl verify` inspects the same state without repairing anything and
//! never contacts a source. It reports integrity, cache state, portability,
//! policy eligibility, and freshness separately so a missing cache is never
//! confused with tampered content.

use std::{
    fs,
    path::{Path, PathBuf},
};

use serde_json::{Value, json};

use crate::{
    app::AppContext,
    artifact::{ArtifactStore, ArtifactWrite},
    bundle::BundleManifest,
    error::{AppError, ExitStatus},
    history::{HistoryLedger, context_for_scope},
    lifecycle,
    lockfile::{LockedBundle, LockedImport, WorkspaceLockfile},
    manifest::{ImportDefinition, WorkspaceManifest},
    materialize::{self, MaterializationReport},
    overlay::{NO_OVERLAY_HASH, hash_overlay_root},
    response::AppResponse,
    source::{
        LockedSourceStaging, SourceKind, acquire_locked_source, copy_source_tree,
        current_timestamp, hash_directory_contents, stored_import_root,
    },
    state::{InstallRecord, LocalStateStore, ManagedScope, ManagedSkillRef},
};

/// Refusal for a restore whose staged revision disappeared before the
/// guarded transaction opened.
///
/// The staging happens before the transaction because acquisition must not run
/// under the coordination lock, so this race is detected rather than prevented.
const STAGING_CHANGED_MESSAGE: &str =
    "the import cache changed while this restore prepared; re-run 'skillctl restore'";

/// Typed request for `skillctl restore`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RestoreRequest {
    /// Do not change the lockfile and do not select newer revisions.
    pub frozen: bool,
    /// Reconstruct only from verified local artifacts.
    ///
    /// Offline restore never contacts a source and never implies an upstream
    /// check. A missing artifact is reported instead of worked around.
    pub offline: bool,
}

/// Typed request for `skillctl verify`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct VerifyRequest;

/// Where one import's content came from during a restore.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RestoreOrigin {
    /// The workspace import cache already held verified content.
    WorkspaceStore,
    /// A verified immutable artifact in the local artifact store.
    RetainedArtifact,
    /// The locked revision was acquired from its source.
    SourceAtLockedRevision,
}

impl RestoreOrigin {
    const fn as_str(self) -> &'static str {
        match self {
            Self::WorkspaceStore => "workspace-store",
            Self::RetainedArtifact => "retained-artifact",
            Self::SourceAtLockedRevision => "source-at-locked-revision",
        }
    }
}

/// One enabled import plus the paths and digests a restore needs.
struct RestoreTarget {
    import_id: String,
    scope: ManagedScope,
    locked: LockedImport,
    stored_root: PathBuf,
    skill_root: PathBuf,
    /// Overlay the manifest declares for this import, if any.
    overlay: OverlayExpectation,
    /// Reference the manifest asks the source to track.
    requested_reference: Option<String>,
}

/// Content found in one place, with the reason it did not verify.
enum ContentState {
    Verified(BundleManifest),
    Mismatch(String),
    Missing,
}

impl ContentState {
    fn status(&self) -> &'static str {
        match self {
            Self::Verified(_) => "verified",
            Self::Mismatch(_) => "mismatch",
            Self::Missing => "missing",
        }
    }
}

/// What the manifest and the lockfile together expect from one import's
/// overlay directory.
#[derive(Clone, Debug)]
enum OverlayExpectation {
    /// The manifest declares no override for this import.
    NotDeclared,
    /// An override is declared but the lockfile pins no overlay content.
    ///
    /// Resolution still needs the directory to exist, but no digest is pinned,
    /// so there is no portable content to reconstruct.
    DeclaredWithoutContent {
        /// Absolute overlay directory path.
        root: PathBuf,
        /// Overlay path exactly as the manifest records it.
        relative: String,
    },
    /// The lockfile pins overlay content that has to be present and to match.
    Pinned {
        /// Absolute overlay directory path.
        root: PathBuf,
        /// Overlay path exactly as the manifest records it.
        relative: String,
        /// Overlay digest recorded in the lockfile.
        digest: String,
    },
}

impl OverlayExpectation {
    /// Absolute overlay directory, when an override is declared.
    fn root(&self) -> Option<&Path> {
        match self {
            Self::NotDeclared => None,
            Self::DeclaredWithoutContent { root, .. } | Self::Pinned { root, .. } => Some(root),
        }
    }

    /// Overlay path as the manifest records it, when an override is declared.
    fn relative(&self) -> Option<&str> {
        match self {
            Self::NotDeclared => None,
            Self::DeclaredWithoutContent { relative, .. } | Self::Pinned { relative, .. } => {
                Some(relative)
            }
        }
    }

    /// Pinned overlay digest, when the lockfile records real content.
    fn pinned_digest(&self) -> Option<&str> {
        match self {
            Self::NotDeclared | Self::DeclaredWithoutContent { .. } => None,
            Self::Pinned { digest, .. } => Some(digest),
        }
    }
}

/// Observed state of the overlay one import declares.
enum OverlayState {
    /// No override is declared, or the lockfile pins no overlay content and the
    /// declared directory exists.
    NotPinned,
    /// The declared overlay directory holds exactly the pinned content.
    Verified,
    /// The declared overlay directory is absent from this checkout.
    Missing,
    /// The declared overlay directory does not match the pinned digest.
    Mismatch(String),
}

impl OverlayState {
    fn status(&self) -> &'static str {
        match self {
            Self::NotPinned => "not-pinned",
            Self::Verified => "verified",
            Self::Missing => "missing",
            Self::Mismatch(_) => "mismatch",
        }
    }

    /// Whether a frozen restore can reproduce the pinned effective version.
    fn allows_restore(&self) -> bool {
        matches!(self, Self::NotPinned | Self::Verified)
    }
}

/// Handle `skillctl restore`.
pub fn handle_restore(
    context: &AppContext,
    request: RestoreRequest,
) -> Result<AppResponse, AppError> {
    let manifest = WorkspaceManifest::load_required_from_workspace(context, "restore")?;
    // The lockfile is the portable pin set a restore reconstructs, so its
    // absence is the shared refusal that names both repairs.
    let lockfile = WorkspaceLockfile::load_required_from_workspace(context, "restore")?;
    let targets = restore_targets(&manifest, &lockfile, &context.working_directory)?;
    // Overlays travel with the repository, not with the lockfile, so they are
    // checked before anything is acquired or written.
    validate_declared_overlays(&targets)?;
    let artifacts = ArtifactStore::open_default()?;
    // Source acquisitions run before the guarded transaction opens: the
    // coordination lock must not be held across a network clone or a
    // well-known fetch, either of which can outlast every other command's
    // lock timeout. The guarded closure re-inspects every target under the
    // lock, so a cache that changed underneath the staging refuses instead of
    // quietly installing what the staging no longer describes.
    let staged = targets
        .iter()
        .map(|target| stage_restore_target(target, &artifacts, request))
        .collect::<Result<Vec<_>, AppError>>()?;
    let timestamp = current_timestamp();

    lifecycle::run_transaction("restore", |transaction| {
        transaction.track_state_database()?;
        for target in &targets {
            transaction.track_path(&target.stored_root)?;
        }
        for scope in restored_scopes(&targets) {
            for root in materialize::sync_mutation_root_paths(
                &context_for_scope(context, scope),
                &manifest,
                crate::adapter::TargetScope::from(scope),
            )? {
                transaction.track_path(root)?;
            }
        }
        if !request.frozen {
            transaction.track_path(&lockfile.path)?;
        }

        let mut outcomes = Vec::with_capacity(targets.len());
        let mut upgraded = lockfile.clone();
        let mut lockfile_upgrade_pending = false;

        for (target, staged) in targets.iter().zip(staged) {
            let (report, captured_bundle) = restore_one(target, &artifacts, request, staged)?;
            if let Some(bundle) = captured_bundle
                && upgraded
                    .imports
                    .get(&target.import_id)
                    .is_some_and(|entry| entry.bundle.is_none())
                && let Some(entry) = upgraded.imports.get_mut(&target.import_id)
            {
                entry.bundle = Some(bundle);
                lockfile_upgrade_pending = true;
            }
            outcomes.push(report);
        }

        // A frozen restore never writes the lockfile. A plain restore performs
        // the deliberate version upgrade only, and only when there is something
        // to record: pinned revisions and content digests never move.
        let mut lockfile_changed = false;
        if !request.frozen && (lockfile_upgrade_pending || lockfile.is_legacy_upgrade()) {
            upgraded.write_to_path()?;
            lockfile_changed = true;
        }

        // Every restored scope is materialized with its own scoped context, and
        // each scope's projection state is refreshed from its own report. A
        // single unscoped sync covers one scope only, so its report must never
        // rewrite the projection records of another restored scope.
        let mut scopes = restored_scopes(&targets);
        if scopes.is_empty() {
            // No import is enabled, so no scope is reconstructed. The selected
            // scope is still synced once, as a restore of no imports always
            // did, so stale projections are pruned and the response keeps its
            // projection report.
            scopes.push(selected_scope(context));
        }
        let mut scope_reports: Vec<(ManagedScope, MaterializationReport)> = Vec::new();
        for scope in scopes {
            let scoped = context_for_scope(context, scope);
            let sync_report = materialize::sync_workspace(&scoped)?;
            let scope_targets: Vec<&RestoreTarget> = targets
                .iter()
                .filter(|target| target.scope == scope)
                .collect();
            record_restore_state(&scoped, scope, &scope_targets, &sync_report, &timestamp)?;
            scope_reports.push((scope, sync_report));
        }

        let mut response = AppResponse::success("restore");
        let summary = restore_summary(&outcomes, request, lockfile_changed, &lockfile);
        let mut data = json!({
            "frozen": request.frozen,
            "offline": request.offline,
            "lockfile": {
                "path": crate::planner::portable_path(&lockfile.path),
                "loaded_version": lockfile.loaded_version.unwrap_or(lockfile.version),
                "version": lockfile.version,
                "changed": lockfile_changed,
            },
            "imports": outcomes,
            "projection": &scope_reports[0].1,
        });
        if scope_reports.len() > 1 {
            data["scope_projections"] = json!(
                scope_reports
                    .iter()
                    .map(|(scope, report)| json!({
                        "scope": scope.as_str(),
                        "projection": report,
                    }))
                    .collect::<Vec<_>>()
            );
        }
        response = response.with_summary(summary).with_data(data);

        Ok(response)
    })
}

/// Handle `skillctl verify`.
///
/// Verification reads the manifest, the lockfile, the import cache, and the
/// artifact store. It repairs nothing, records nothing, and never contacts a
/// source.
pub fn handle_verify(
    context: &AppContext,
    _request: VerifyRequest,
) -> Result<AppResponse, AppError> {
    let manifest = WorkspaceManifest::load_required_from_workspace(context, "verify")?;
    // The lockfile carries the pins this report answers from. An empty
    // lockfile would verify zero imports and read as a healthy workspace, so
    // its absence is a refusal that names both repairs instead.
    let lockfile = WorkspaceLockfile::load_required_from_workspace(context, "verify")?;
    let artifacts = ArtifactStore::open_default()?;
    let targets = restore_targets(&manifest, &lockfile, &context.working_directory)?;

    let mut reports = Vec::with_capacity(targets.len());
    let mut integrity_failures = Vec::new();
    let mut warnings = Vec::new();

    for target in &targets {
        let store_state = inspect_content(&target.skill_root, &target.locked);
        // An unreadable overlay directory is reported as content that cannot
        // match its pin, in the same way an unreadable import cache is.
        let overlay_state = inspect_overlay(&target.overlay)
            .unwrap_or_else(|error| OverlayState::Mismatch(error.to_string()));
        let artifact_state = match target.locked.bundle_digest() {
            Some(digest) => match artifacts.verify(digest) {
                Ok(Some(_)) => "verified",
                Ok(None) => "missing",
                Err(error) => {
                    warnings.push(format!(
                        "retained artifact for '{}' failed verification: {error}",
                        target.import_id
                    ));
                    "mismatch"
                }
            },
            None => "unrecorded",
        };

        let mut report = json!({
            "skill": target.import_id,
            "scope": target.scope.as_str(),
            "source_type": target.locked.source.kind.label(),
            "source_url": target.locked.source.url,
            "revision": target.locked.revision.resolved,
            "subpath": target.locked.source.subpath.as_str(),
            "bundle_digest": target.locked.bundle_digest(),
            "content_hash": target.locked.hashes.content,
            "integrity": {
                "status": store_state.status(),
                "detail": match &store_state {
                    ContentState::Verified(_) => Value::Null,
                    ContentState::Mismatch(detail) => json!(detail),
                    ContentState::Missing => json!(
                        "the import cache holds no content for this import"
                    ),
                },
            },
            "cache": {
                "workspace_store": store_state.status(),
                "path": crate::planner::portable_path(&target.stored_root),
                "artifact": artifact_state,
            },
            "overlay": {
                "declared": target.overlay.root().is_some(),
                "path": target.overlay.relative(),
                "pinned_digest": target.overlay.pinned_digest(),
                "status": overlay_state.status(),
                "detail": match &overlay_state {
                    OverlayState::NotPinned | OverlayState::Verified => Value::Null,
                    OverlayState::Missing => json!(
                        "the overlay directory is absent from this checkout"
                    ),
                    OverlayState::Mismatch(observed) => json!(format!(
                        "the overlay hashes to {observed} but the lockfile pins a different digest"
                    )),
                },
            },
        });

        let content_restorable = match &store_state {
            ContentState::Verified(_) => true,
            ContentState::Missing => match artifact_state {
                "verified" => true,
                _ => source_is_reachable(&target.locked),
            },
            ContentState::Mismatch(_) => artifact_state == "verified",
        };
        // Overlay files are committed with the repository instead of being
        // carried by the lockfile, so they gate portability on their own.
        let restorable = content_restorable && overlay_state.allows_restore();

        let bundle_recorded = target.locked.bundle_digest().is_some();
        if !bundle_recorded {
            warnings.push(format!(
                "import '{}' records no canonical bundle digest; it is verified with the legacy \
                 content hash only. Run 'skillctl restore' to upgrade the lockfile entry",
                target.import_id
            ));
        }

        let portability_reason = if !overlay_state.allows_restore() {
            match &overlay_state {
                OverlayState::Missing => concat!(
                    "the lockfile pins overlay content that is absent from this checkout; ",
                    "overlays are committed next to .agents/skillctl.yaml and ",
                    ".agents/skillctl.lock"
                ),
                OverlayState::Mismatch(_) => concat!(
                    "the overlay directory does not match the pinned overlay digest, so a ",
                    "frozen restore refuses to reproduce the recorded effective version"
                ),
                OverlayState::NotPinned | OverlayState::Verified => {
                    unreachable!("a restorable overlay never reaches this branch")
                }
            }
        } else if content_restorable {
            concat!(
                "the pinned content can be reconstructed from a retained artifact or the ",
                "recorded source"
            )
        } else {
            match &store_state {
                // A cache that disagrees with the pin is never trusted and is
                // never silently replaced from a source; only a verified
                // retained artifact repairs it, and none is available.
                ContentState::Mismatch(_) => concat!(
                    "the cached content no longer matches its pin and no verified ",
                    "retained artifact can replace it"
                ),
                ContentState::Missing => concat!(
                    "the recorded source is a local path that is unavailable here and no ",
                    "verified artifact is retained"
                ),
                ContentState::Verified(_) => {
                    unreachable!("verified content is always restorable")
                }
            }
        };
        report["portability"] = json!({
            "restorable": restorable,
            "retained_artifact": target.locked.bundle_digest().is_some(),
            "source_kind": target.locked.source.kind.label(),
            "reason": portability_reason,
        });
        report["freshness"] =
            freshness_report(&target.locked, target.requested_reference.as_deref());
        report["policy"] = policy_report(&target.locked);

        match &overlay_state {
            OverlayState::Missing => integrity_failures.push(format!(
                "import '{}' pins overlay content {} but the overlay directory '{}' is absent \
                 from this checkout; commit the overlay next to .agents/skillctl.yaml and \
                 .agents/skillctl.lock or remove its 'overrides.{}' entry",
                target.import_id,
                target.overlay.pinned_digest().unwrap_or(NO_OVERLAY_HASH),
                crate::planner::portable_path(
                    target
                        .overlay
                        .root()
                        .expect("a missing overlay is a declared one"),
                ),
                target.import_id
            )),
            OverlayState::Mismatch(observed) => warnings.push(format!(
                "import '{}' has an overlay that hashes to {observed} instead of the pinned {}; a \
                 frozen restore refuses it. Run 'skillctl override {}' to re-pin the change after \
                 reviewing it",
                target.import_id,
                target.overlay.pinned_digest().unwrap_or(NO_OVERLAY_HASH),
                target.import_id
            )),
            OverlayState::NotPinned | OverlayState::Verified => {}
        }

        match &store_state {
            ContentState::Mismatch(detail) => integrity_failures.push(format!(
                "import '{}' does not match the pinned content: {detail}; {}",
                target.import_id,
                crate::resolver::store_divergence_repair(&target.import_id)
            )),
            ContentState::Missing if !content_restorable => integrity_failures.push(format!(
                "import '{}' has no content, no retained artifact, and a source that cannot be \
                 reached from this machine",
                target.import_id
            )),
            ContentState::Missing if overlay_state.allows_restore() => warnings.push(format!(
                "import '{}' is not materialized in this checkout; run 'skillctl restore --frozen'",
                target.import_id
            )),
            // The overlay already produced the actionable failure above.
            ContentState::Missing => {}
            ContentState::Verified(_) => {}
        }

        reports.push(report);
    }

    for id in lockfile.imports.keys() {
        if !manifest.imports.iter().any(|import| import.id == *id) {
            warnings.push(format!(
                "lockfile entry '{id}' has no matching manifest import; it is stale"
            ));
        }
    }

    let verified = reports
        .iter()
        .filter(|report| report["integrity"]["status"] == json!("verified"))
        .count();

    let mut response = if integrity_failures.is_empty() {
        AppResponse::success("verify")
    } else {
        let mut response = AppResponse::failure("verify", integrity_failures.join("; "));
        response.status_override = Some(ExitStatus::ValidationFailure);
        response
    };
    // Warnings stay attached beside a failure too: `counts` reports them, and
    // the status override keeps the exit code unchanged either way.
    for warning in &warnings {
        response = response.with_warning(warning.clone());
    }

    response = response.with_summary(format!(
        "Verified {verified} of {} pinned imports. Integrity {}, portability {}, freshness {}.",
        reports.len(),
        if integrity_failures.is_empty() {
            "clean"
        } else {
            "failed"
        },
        reports
            .iter()
            .filter(|report| report["portability"]["restorable"] == json!(true))
            .count(),
        reports
            .iter()
            .filter(|report| report["freshness"]["upstream_check"] == json!("available"))
            .count(),
    ));

    let mut data = json!({
        "lockfile": {
            "path": crate::planner::portable_path(&lockfile.path),
            "loaded_version": lockfile.loaded_version.unwrap_or(lockfile.version),
            "version": lockfile.version,
        },
        "imports": reports,
        "counts": {
            "imports": reports.len(),
            "verified": verified,
            "integrity_failures": integrity_failures.len(),
            "warnings": warnings.len(),
        },
    });
    if !integrity_failures.is_empty() {
        data["integrity_failures"] = json!(integrity_failures);
    }

    Ok(response.with_data(data))
}

/// Reconstruct one import and report where the content came from.
///
/// Returns the structured report plus the canonical bundle captured from the
/// verified content, so a lockfile upgrade can record it without re-reading the
/// tree.
fn restore_one(
    target: &RestoreTarget,
    artifacts: &ArtifactStore,
    request: RestoreRequest,
    staged: Option<StagedImport>,
) -> Result<(Value, Option<LockedBundle>), AppError> {
    let skill_root = &target.skill_root;

    match inspect_content(skill_root, &target.locked) {
        ContentState::Verified(manifest) => {
            let artifact = retain_artifact(artifacts, skill_root)?;
            Ok((
                report(target, RestoreOrigin::WorkspaceStore, &artifact),
                Some(bundle_record(&manifest)),
            ))
        }
        ContentState::Mismatch(detail) => {
            // The workspace cache is repairable only from a verified immutable
            // artifact. Content that disagrees with the lockfile is never
            // trusted and never silently replaced from a source.
            let repairable = match target.locked.bundle_digest() {
                Some(digest) => artifacts.verify(digest)?.is_some(),
                None => false,
            };
            if !repairable {
                return Err(restore_error(
                    target,
                    format!(
                        "cached content does not match the pinned digests ({detail}) and no \
                         verified retained artifact can replace it. Remove the cached import at \
                         '{}' and run 'skillctl restore --frozen' again, or reinstall the skill \
                         after reviewing the change",
                        crate::planner::portable_path(&target.stored_root)
                    ),
                ));
            }

            let digest = required_bundle_digest(target)?;
            materialize_artifact_into_store(artifacts, digest, target)?;
            let manifest = expect_verified_content(target)?;
            let artifact = retain_artifact(artifacts, skill_root)?;
            Ok((
                report(target, RestoreOrigin::RetainedArtifact, &artifact)
                    .merged_repair_note(&detail),
                Some(bundle_record(&manifest)),
            ))
        }
        ContentState::Missing => {
            if let Some(digest) = target.locked.bundle_digest()
                && artifacts.verify(digest)?.is_some()
            {
                materialize_artifact_into_store(artifacts, digest, target)?;
                let manifest = expect_verified_content(target)?;
                let artifact = retain_artifact(artifacts, skill_root)?;
                return Ok((
                    report(target, RestoreOrigin::RetainedArtifact, &artifact),
                    Some(bundle_record(&manifest)),
                ));
            }

            if request.offline {
                return Err(restore_error(
                    target,
                    format!(
                        "offline restore requires a verified cached artifact. Neither the import \
                         cache nor the artifact store holds content for bundle {}; rerun without \
                         '--offline' to acquire the locked revision from its source",
                        target.locked.bundle_digest().unwrap_or("<not recorded>")
                    ),
                ));
            }

            // The revision was staged before the transaction opened. A missing
            // staging means the cache changed underneath the preparation, so
            // the restore refuses instead of acquiring from its source while
            // the coordination lock is held.
            let staged = staged.ok_or_else(|| restore_error(target, STAGING_CHANGED_MESSAGE))?;
            install_staged_import(target, artifacts, staged)
        }
    }
}

/// One locked revision staged from its source before the restore transaction
/// opens.
///
/// The staging owns its temporary directory, so the verified tree stays alive
/// until the guarded transaction has copied it into the workspace cache.
struct StagedImport {
    /// Staged skill root inside the staging directory.
    skill_root: PathBuf,
    /// Bundle manifest captured from the staged tree.
    manifest: BundleManifest,
    /// Staging that owns the temporary directory.
    _staging: LockedSourceStaging,
}

/// Stage and verify the locked revision of one import from its source.
///
/// This is the acquiring half of a source restore. It performs every network
/// step and every verification step and writes nothing to the workspace, so it
/// runs before the restore transaction opens and never under the coordination
/// lock.
fn stage_locked_import(target: &RestoreTarget) -> Result<StagedImport, AppError> {
    let staged = acquire_locked_source(&target.locked).map_err(|error| match error {
        AppError::SourceValidation { input, message } => restore_error(
            target,
            // `input` names the locked source, not the revision, so the
            // follow-up names the source the lockfile recorded.
            format!("{message}. The locked source is {input}"),
        ),
        other => restore_error(target, other.to_string()),
    })?;

    let staged_skill_root = staged.root.join(target.locked.source.subpath.as_str());
    if staged.resolved_revision != target.locked.revision.resolved {
        return Err(restore_error(
            target,
            format!(
                "the source resolved to revision {} but the lockfile pins {}; skillctl never \
                 substitutes one revision for another",
                staged.resolved_revision, target.locked.revision.resolved
            ),
        ));
    }
    if !staged_skill_root.is_dir() {
        return Err(restore_error(
            target,
            format!(
                "the locked subpath '{}' does not exist at revision {}; the source history no \
                 longer matches the lockfile",
                target.locked.source.subpath.as_str(),
                target.locked.revision.resolved
            ),
        ));
    }

    let manifest = BundleManifest::capture(&staged_skill_root)?;
    verify_against_lock(target, &manifest, &staged_skill_root)?;

    Ok(StagedImport {
        skill_root: staged_skill_root,
        manifest,
        _staging: staged,
    })
}

/// Decide whether one target needs its locked revision staged from its source.
///
/// The decision mirrors the branches `restore_one` takes under the lock: a
/// verified cache and a retained artifact need no network, a mismatched cache
/// is repaired from the artifact store or refused, and only a cache that is
/// both empty and uncovered acquires from the source.
fn stage_restore_target(
    target: &RestoreTarget,
    artifacts: &ArtifactStore,
    request: RestoreRequest,
) -> Result<Option<StagedImport>, AppError> {
    match inspect_content(&target.skill_root, &target.locked) {
        ContentState::Verified(_) | ContentState::Mismatch(_) => Ok(None),
        ContentState::Missing => {
            if let Some(digest) = target.locked.bundle_digest()
                && artifacts.verify(digest)?.is_some()
            {
                return Ok(None);
            }
            if request.offline {
                return Ok(None);
            }
            stage_locked_import(target).map(Some)
        }
    }
}

/// Copy one staged revision into the workspace cache and retain its artifact.
fn install_staged_import(
    target: &RestoreTarget,
    artifacts: &ArtifactStore,
    staged: StagedImport,
) -> Result<(Value, Option<LockedBundle>), AppError> {
    // Replace the workspace cache for this import, then retain the artifact.
    if target.stored_root.exists() {
        fs::remove_dir_all(&target.stored_root).map_err(|source| {
            AppError::FilesystemOperation {
                action: "replace stale import cache",
                path: target.stored_root.clone(),
                source,
            }
        })?;
    }
    copy_source_tree(&staged.skill_root, &target.skill_root)?;
    let artifact = retain_artifact(artifacts, &target.skill_root)?;

    Ok((
        report(target, RestoreOrigin::SourceAtLockedRevision, &artifact),
        Some(bundle_record(&staged.manifest)),
    ))
}

/// Retain the verified tree as an immutable artifact.
fn retain_artifact(artifacts: &ArtifactStore, skill_root: &Path) -> Result<Value, AppError> {
    let (_, outcome) = artifacts.store(skill_root)?;
    Ok(json!({
        "status": match outcome {
            ArtifactWrite::Created => "created",
            ArtifactWrite::AlreadyPresent => "already-present",
        },
        "store_root": crate::planner::portable_path(artifacts.root()),
    }))
}

/// Copy a verified artifact into the workspace import cache.
fn materialize_artifact_into_store(
    artifacts: &ArtifactStore,
    digest: &str,
    target: &RestoreTarget,
) -> Result<(), AppError> {
    if target.stored_root.exists() {
        fs::remove_dir_all(&target.stored_root).map_err(|source| {
            AppError::FilesystemOperation {
                action: "replace stale import cache",
                path: target.stored_root.clone(),
                source,
            }
        })?;
    }
    artifacts.hydrate_into(digest, &target.skill_root)
}

/// Re-read the restored tree and require it to verify.
fn expect_verified_content(target: &RestoreTarget) -> Result<BundleManifest, AppError> {
    match inspect_content(&target.skill_root, &target.locked) {
        ContentState::Verified(manifest) => Ok(manifest),
        ContentState::Mismatch(detail) => Err(restore_error(
            target,
            format!("restored content still does not match the pinned digests: {detail}"),
        )),
        ContentState::Missing => Err(restore_error(
            target,
            "restored content is absent after materialization",
        )),
    }
}

/// Compare captured content with every digest the lockfile pins.
fn verify_against_lock(
    target: &RestoreTarget,
    manifest: &BundleManifest,
    skill_root: &Path,
) -> Result<(), AppError> {
    if let Some(expected) = target.locked.bundle_digest()
        && manifest.digest() != expected
    {
        return Err(restore_error(
            target,
            format!(
                "content reconstructed from the locked {} source digests to {} but the lockfile \
                 pins {expected}; the source changed after the lockfile was written or the \
                 lockfile was edited. skillctl will not install content that differs from the \
                 approved digest",
                target.locked.source.kind.label(),
                manifest.digest()
            ),
        ));
    }

    let content_hash = hash_directory_contents(skill_root)?;
    if content_hash != target.locked.hashes.content {
        return Err(restore_error(
            target,
            format!(
                "content reconstructed from the locked {} source hashes to {content_hash} but \
                 the lockfile pins {}; the pinned source no longer reproduces the approved \
                 content",
                target.locked.source.kind.label(),
                target.locked.hashes.content
            ),
        ));
    }

    Ok(())
}

/// Inspect cached content without changing it.
fn inspect_content(skill_root: &Path, locked: &LockedImport) -> ContentState {
    if !skill_root.is_dir() {
        return ContentState::Missing;
    }

    let manifest = match BundleManifest::capture(skill_root) {
        Ok(manifest) => manifest,
        Err(error) => return ContentState::Mismatch(error.to_string()),
    };

    if let Some(expected) = locked.bundle_digest()
        && manifest.digest() != expected
    {
        return ContentState::Mismatch(format!(
            "canonical bundle digest is {} but the lockfile pins {expected}",
            manifest.digest()
        ));
    }

    match hash_directory_contents(skill_root) {
        Ok(observed) if observed == locked.hashes.content => ContentState::Verified(manifest),
        Ok(observed) => ContentState::Mismatch(format!(
            "content hash is {observed} but the lockfile pins {}",
            locked.hashes.content
        )),
        Err(error) => ContentState::Mismatch(error.to_string()),
    }
}

/// Inspect a declared overlay without changing it.
///
/// Returns an error only when the directory cannot be read at all, which is a
/// different problem from a digest the lockfile does not pin.
fn inspect_overlay(expectation: &OverlayExpectation) -> Result<OverlayState, AppError> {
    match expectation {
        OverlayExpectation::NotDeclared => Ok(OverlayState::NotPinned),
        OverlayExpectation::DeclaredWithoutContent { root, .. } => Ok(if root.is_dir() {
            OverlayState::NotPinned
        } else {
            OverlayState::Missing
        }),
        OverlayExpectation::Pinned { root, digest, .. } => {
            if !root.is_dir() {
                return Ok(OverlayState::Missing);
            }
            let observed = hash_overlay_root(root)?;
            Ok(if &observed == digest {
                OverlayState::Verified
            } else {
                OverlayState::Mismatch(observed)
            })
        }
    }
}

/// Refuse to touch anything while a declared overlay cannot satisfy its pin.
///
/// Overlay files are committable portable state rather than something the
/// lockfile carries, so an absent overlay is unrecoverable here. Reporting it
/// first keeps a frozen restore from acquiring sources and populating the
/// artifact store for a workspace it then cannot project.
fn validate_declared_overlays(targets: &[RestoreTarget]) -> Result<(), AppError> {
    for target in targets {
        match inspect_overlay(&target.overlay)? {
            OverlayState::NotPinned | OverlayState::Verified => {}
            OverlayState::Missing => match target.overlay.pinned_digest() {
                Some(digest) => {
                    return Err(AppError::RestoreValidation {
                        import_id: target.import_id.clone(),
                        message: format!(
                            "the lockfile pins overlay content {digest} but the overlay directory \
                             '{}' is absent from this checkout. Overlays are committable portable \
                             state next to .agents/skillctl.yaml and .agents/skillctl.lock, so \
                             commit the overlay for '{}' or remove its 'overrides.{}' entry before \
                             restoring",
                            crate::planner::portable_path(
                                target.overlay.root().expect("a pinned overlay has a root"),
                            ),
                            target.import_id,
                            target.import_id
                        ),
                    });
                }
                None => {
                    return Err(AppError::RestoreValidation {
                        import_id: target.import_id.clone(),
                        message: format!(
                            "the manifest declares an overlay at '{}' for '{}' but that directory \
                             does not exist and the lockfile pins no overlay content. Run \
                             'skillctl override {}' to create it, or remove the 'overrides.{}' \
                             entry from the manifest",
                            target
                                .overlay
                                .relative()
                                .expect("a declared overlay has a path"),
                            target.import_id,
                            target.import_id,
                            target.import_id
                        ),
                    });
                }
            },
            OverlayState::Mismatch(observed) => {
                let digest = target
                    .overlay
                    .pinned_digest()
                    .expect("a mismatching overlay has a pinned digest");
                return Err(AppError::RestoreValidation {
                    import_id: target.import_id.clone(),
                    message: format!(
                        "the overlay directory '{}' hashes to {observed} but the lockfile pins \
                         {digest}. Restore the committed overlay content for '{}' or, after \
                         reviewing the change, re-pin it with 'skillctl override {}'",
                        crate::planner::portable_path(
                            target.overlay.root().expect("a pinned overlay has a root"),
                        ),
                        target.import_id,
                        target.import_id
                    ),
                });
            }
        }
    }
    Ok(())
}

fn required_bundle_digest(target: &RestoreTarget) -> Result<&str, AppError> {
    target.locked.bundle_digest().ok_or_else(|| {
        restore_error(
            target,
            "the lockfile entry records no canonical bundle digest, so the retained artifact \
             store cannot be used to repair this import",
        )
    })
}

/// Build the restore targets for every enabled import.
fn restore_targets(
    manifest: &WorkspaceManifest,
    lockfile: &WorkspaceLockfile,
    working_directory: &Path,
) -> Result<Vec<RestoreTarget>, AppError> {
    let mut targets = Vec::new();
    for import in manifest.imports.iter().filter(|import| import.enabled) {
        let Some(locked) = lockfile.imports.get(&import.id) else {
            return Err(AppError::RestoreValidation {
                import_id: import.id.clone(),
                message: format!(
                    "enabled import '{}' is missing from '{}'; the lockfile cannot restore it",
                    import.id,
                    crate::planner::portable_path(&lockfile.path)
                ),
            });
        };
        let scope = ManagedScope::from(import.scope);
        let stored_root = stored_import_root(scope, working_directory, &import.id)?;
        let skill_root = stored_root.join(locked.source.subpath.as_str());
        let overlay = overlay_expectation(manifest, import, locked, working_directory);
        targets.push(RestoreTarget {
            import_id: import.id.clone(),
            scope,
            locked: locked.clone(),
            stored_root,
            skill_root,
            overlay,
            requested_reference: Some(import.ref_spec.clone()),
        });
    }
    Ok(targets)
}

/// Derive what the manifest and the lockfile expect from one import's overlay.
fn overlay_expectation(
    manifest: &WorkspaceManifest,
    import: &ImportDefinition,
    locked: &LockedImport,
    working_directory: &Path,
) -> OverlayExpectation {
    let Some(path) = manifest.overrides.get(&import.id) else {
        return OverlayExpectation::NotDeclared;
    };
    let relative = path.as_str().to_string();
    let root = working_directory.join(&relative);
    if locked.hashes.overlay == NO_OVERLAY_HASH {
        OverlayExpectation::DeclaredWithoutContent { root, relative }
    } else {
        OverlayExpectation::Pinned {
            root,
            relative,
            digest: locked.hashes.overlay.clone(),
        }
    }
}

/// Record local installation bookkeeping for the restored imports of one
/// scope.
///
/// Dependency identity is never advanced here: the install records repeat the
/// revisions and digests already pinned in the lockfile. Projection state is
/// refreshed from this scope's own sync report only, so one scope's report
/// never rewrites another scope's records.
fn record_restore_state(
    context: &AppContext,
    scope: ManagedScope,
    targets: &[&RestoreTarget],
    sync_report: &MaterializationReport,
    timestamp: &str,
) -> Result<(), AppError> {
    if targets.is_empty() {
        return Ok(());
    }

    let mut store = LocalStateStore::open_default_for(&context.working_directory)?;

    let mut records = Vec::with_capacity(targets.len());
    for target in targets {
        let skill = ManagedSkillRef::new(target.scope, target.import_id.clone());
        let installed_at = store
            .install_record(&skill)?
            .map_or_else(|| timestamp.to_string(), |record| record.installed_at);
        records.push(InstallRecord {
            skill,
            source_kind: target.locked.source.kind,
            source_url: target.locked.source.url.clone(),
            source_subpath: target.locked.source.subpath.as_str().to_string(),
            resolved_revision: target.locked.revision.resolved.clone(),
            upstream_revision: target.locked.revision.upstream.clone(),
            content_hash: target.locked.hashes.content.clone(),
            overlay_hash: target.locked.hashes.overlay.clone(),
            effective_version_hash: target.locked.hashes.effective_version.clone(),
            installed_at,
            updated_at: timestamp.to_string(),
            detached: false,
            forked: false,
        });
    }

    {
        let mut ledger = HistoryLedger::new(&mut store);
        for record in &records {
            ledger.record_install(record)?;
        }
    }

    materialize::refresh_projection_state_for_scope(
        &mut store,
        context,
        scope,
        sync_report,
        timestamp,
        |_| Ok(()),
    )?;

    Ok(())
}

/// Distinct scopes a restore touched, in a stable order.
fn restored_scopes(targets: &[RestoreTarget]) -> Vec<ManagedScope> {
    let mut scopes = Vec::new();
    for scope in [ManagedScope::Workspace, ManagedScope::User] {
        if targets.iter().any(|target| target.scope == scope) {
            scopes.push(scope);
        }
    }
    scopes
}

/// The scope the invocation selected, defaulting to the workspace.
fn selected_scope(context: &AppContext) -> ManagedScope {
    match context.selector.scope {
        Some(crate::cli::Scope::User) => ManagedScope::User,
        Some(crate::cli::Scope::Workspace) | None => ManagedScope::Workspace,
    }
}

fn bundle_record(manifest: &BundleManifest) -> LockedBundle {
    LockedBundle {
        format_version: manifest.format_version,
        digest: manifest.digest(),
        files: manifest.file_count() as u64,
        bytes: manifest.total_bytes(),
    }
}

fn report(target: &RestoreTarget, origin: RestoreOrigin, artifact: &Value) -> Value {
    json!({
        "skill": target.import_id,
        "scope": target.scope.as_str(),
        "source_type": target.locked.source.kind.label(),
        "source_url": target.locked.source.url,
        "revision": target.locked.revision.resolved,
        "subpath": target.locked.source.subpath.as_str(),
        "bundle_digest": target.locked.bundle_digest(),
        "content_hash": target.locked.hashes.content,
        "effective_version_hash": target.locked.hashes.effective_version,
        "upstream_revision": reportable_upstream_revision(
            &target.locked,
            target.requested_reference.as_deref(),
        ),
        "restored_from": origin.as_str(),
        "verified": true,
        "artifact": artifact,
        "store_path": crate::planner::portable_path(&target.skill_root),
    })
}

trait RepairNote {
    fn merged_repair_note(self, detail: &str) -> Value;
}

impl RepairNote for Value {
    fn merged_repair_note(mut self, detail: &str) -> Value {
        self["repaired"] = json!(detail);
        self
    }
}

fn restore_summary(
    outcomes: &[Value],
    request: RestoreRequest,
    lockfile_changed: bool,
    lockfile: &WorkspaceLockfile,
) -> String {
    let from_artifact = outcomes
        .iter()
        .filter(|outcome| outcome["restored_from"] == json!("retained-artifact"))
        .count();
    let from_source = outcomes
        .iter()
        .filter(|outcome| outcome["restored_from"] == json!("source-at-locked-revision"))
        .count();
    let already_present = outcomes.len().saturating_sub(from_artifact + from_source);

    let mut summary = format!(
        "Restored {} import{}: {already_present} already verified, {from_artifact} from retained \
         artifacts, {from_source} acquired at the locked revision.",
        outcomes.len(),
        if outcomes.len() == 1 { "" } else { "s" },
    );
    if request.frozen {
        summary.push_str(" The lockfile was not modified.");
    } else if lockfile_changed {
        summary.push_str(&format!(
            " The lockfile was upgraded to version {} without changing any pinned revision.",
            lockfile.version
        ));
    }
    if request.offline {
        summary.push_str(" No source was contacted.");
    }

    summary
}

/// The upstream revision one import's recorded state can honestly report.
///
/// A manifest reference that names an immutable revision leaves nothing for an
/// upstream check to observe, so a recorded value is withheld instead of being
/// presented as an observation.
fn reportable_upstream_revision<'a>(
    locked: &'a LockedImport,
    requested_reference: Option<&str>,
) -> Option<&'a String> {
    crate::source::reportable_upstream_revision(
        locked.revision.upstream.as_ref(),
        &locked.revision.resolved,
        requested_reference,
    )
}

fn freshness_report(locked: &LockedImport, requested_reference: Option<&str>) -> Value {
    // A manifest reference that names an immutable revision leaves nothing for
    // an upstream check to observe, so the report names that state instead of
    // presenting a recorded revision as an observation.
    let commit_pinned = locked.source.kind == SourceKind::Git
        && requested_reference.is_some_and(|reference| {
            crate::source::reference_pins_immutable_revision(reference, &locked.revision.resolved)
        });
    let upstream_check = match locked.source.kind {
        SourceKind::Git if commit_pinned => "unavailable-commit-pinned",
        SourceKind::Git => "available",
        SourceKind::LocalPath | SourceKind::Archive => "unavailable-retained-snapshot",
        SourceKind::WellKnown => "available-digest-index",
    };

    json!({
        "upstream_check": upstream_check,
        "upstream_revision": reportable_upstream_revision(locked, requested_reference),
        "requested_reference": requested_reference,
        "note": match locked.source.kind {
            SourceKind::Git if commit_pinned => concat!(
                "the import is pinned to an immutable revision, which names no upstream ref an ",
                "update check could observe; run 'skillctl pin <skill> <branch-or-tag>' to ",
                "resume upstream tracking"
            ),
            SourceKind::Git => "upstream checks compare the pinned revision with the remote",
            SourceKind::LocalPath | SourceKind::Archive => concat!(
                "folder and archive imports have no upstream; the retained artifact is the only ",
                "recorded source of this content"
            ),
            SourceKind::WellKnown => concat!(
                "well-known imports pin the verified artifact digest; 'skillctl update <skill>' ",
                "re-reads the discovery index and compares the digest it declares with the pin"
            ),
        },
    })
}

fn policy_report(locked: &LockedImport) -> Value {
    json!({
        "status": "not-configured",
        "trust": "locally-pinned",
        "note": "digest verification proves the content matches the lockfile; it does not prove \
                 the content was reviewed or is safe",
        "revision": locked.revision.resolved,
    })
}

/// Whether the recorded source can be reached from this machine at all.
fn source_is_reachable(locked: &LockedImport) -> bool {
    if matches!(locked.source.kind, SourceKind::Git | SourceKind::WellKnown) {
        return true;
    }
    match url::Url::parse(&locked.source.url)
        .ok()
        .and_then(|url| url.to_file_path().ok())
    {
        Some(path) => path.exists(),
        None => false,
    }
}

fn restore_error(target: &RestoreTarget, message: impl Into<String>) -> AppError {
    AppError::RestoreValidation {
        import_id: target.import_id.clone(),
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::STAGING_CHANGED_MESSAGE;

    /// The refusal renders verbatim, so a stray run of spaces inside the
    /// literal corrupts every error report that carries it.
    #[test]
    fn the_staging_changed_refusal_renders_single_spaced() {
        assert!(
            !STAGING_CHANGED_MESSAGE.contains("  "),
            "the refusal must not contain doubled spaces: {STAGING_CHANGED_MESSAGE}"
        );
        assert!(
            STAGING_CHANGED_MESSAGE.contains("re-run 'skillctl restore'"),
            "the refusal names the repair: {STAGING_CHANGED_MESSAGE}"
        );
    }
}
