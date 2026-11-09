//! History ledger APIs plus the current command entry points.

use std::{
    collections::BTreeMap,
    ffi::OsStr,
    path::{Path, PathBuf},
};

use serde::Serialize;
use serde_json::{Value, json};
use tempfile::TempDir;

use crate::{
    adapter::TargetScope,
    app::AppContext,
    cli::Scope,
    error::AppError,
    lockfile::WorkspaceLockfile,
    manifest::WorkspaceManifest,
    materialize,
    overlay::{NO_OVERLAY_HASH, hash_overlay_root},
    response::AppResponse,
    skill,
    source::{SourceKind, current_timestamp, stored_import_root},
    state::{
        HistoryDetails, HistoryEntry, HistoryQuery, InstallRecord, LocalModificationRecord,
        LocalStateStore, ManagedScope, ManagedSkillRef, PinRecord, ProjectionRecord,
        ReviewDecision, RollbackRecord, TelemetrySettings, TrustReviewRecord, UpdateCheckRecord,
        insert_history_entry_in, insert_local_modification_record_in, insert_rollback_record_in,
        insert_update_check_record_in, upsert_install_record_in, upsert_pin_record_in,
        upsert_projection_record_in, upsert_telemetry_settings_in, upsert_trust_review_in,
    },
    trust::SkillTrust,
};

/// Kinds of history events the ledger can record.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HistoryEventKind {
    /// Skill installation.
    Install,
    /// Upstream update check.
    UpdateCheck,
    /// Applied update.
    UpdateApplied,
    /// Projection materialization.
    Projection,
    /// Revision pinning.
    Pin,
    /// Rollback activation.
    Rollback,
    /// Overlay creation.
    OverlayCreated,
    /// Direct modification detection.
    DirectModificationDetected,
    /// Detach into local canonical ownership.
    Detach,
    /// Fork into local canonical ownership.
    Fork,
    /// Cleanup action.
    Cleanup,
    /// Stale projection prune.
    Prune,
    /// Telemetry consent change.
    TelemetryConsentChanged,
    /// Recorded local review decision for an imported skill.
    TrustReview,
}

impl HistoryEventKind {
    /// Return the stable persisted identifier.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Install => "install",
            Self::UpdateCheck => "update-check",
            Self::UpdateApplied => "update-applied",
            Self::Projection => "projection",
            Self::Pin => "pin",
            Self::Rollback => "rollback",
            Self::OverlayCreated => "overlay-created",
            Self::DirectModificationDetected => "direct-modification-detected",
            Self::Detach => "detach",
            Self::Fork => "fork",
            Self::Cleanup => "cleanup",
            Self::Prune => "prune",
            Self::TelemetryConsentChanged => "telemetry-consent-changed",
            Self::TrustReview => "trust-review",
        }
    }

    /// Parse a persisted identifier into a typed event kind.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "install" => Some(Self::Install),
            "update-check" => Some(Self::UpdateCheck),
            "update-applied" => Some(Self::UpdateApplied),
            "projection" => Some(Self::Projection),
            "pin" => Some(Self::Pin),
            "rollback" => Some(Self::Rollback),
            "overlay-created" => Some(Self::OverlayCreated),
            "direct-modification-detected" => Some(Self::DirectModificationDetected),
            "detach" => Some(Self::Detach),
            "fork" => Some(Self::Fork),
            "cleanup" => Some(Self::Cleanup),
            "prune" => Some(Self::Prune),
            "telemetry-consent-changed" => Some(Self::TelemetryConsentChanged),
            "trust-review" => Some(Self::TrustReview),
            _ => None,
        }
    }
}

/// Deterministic history-writing facade over the SQLite state store.
pub struct HistoryLedger<'store> {
    store: &'store mut LocalStateStore,
}

impl<'store> HistoryLedger<'store> {
    /// Build a ledger facade over an already-open state store.
    pub fn new(store: &'store mut LocalStateStore) -> Self {
        Self { store }
    }

    /// Record a newly installed skill and append a matching history entry.
    pub fn record_install(&mut self, record: &InstallRecord) -> Result<i64, AppError> {
        self.record_install_with_optional_trust(record, None)
    }

    /// Record a newly installed skill and attach trust details to local history.
    pub fn record_install_with_trust(
        &mut self,
        record: &InstallRecord,
        trust: &SkillTrust,
    ) -> Result<i64, AppError> {
        self.record_install_with_optional_trust(record, Some(trust))
    }

    fn record_install_with_optional_trust(
        &mut self,
        record: &InstallRecord,
        trust: Option<&SkillTrust>,
    ) -> Result<i64, AppError> {
        let entry = HistoryEntry {
            id: None,
            kind: HistoryEventKind::Install,
            scope: Some(record.skill.scope),
            skill_id: Some(record.skill.skill_id.clone()),
            target: None,
            occurred_at: record.updated_at.clone(),
            summary: format!(
                "Installed {} at {}",
                record.skill.skill_id, record.resolved_revision
            ),
            details: install_details(record, trust),
        };
        let workspace_key = self
            .store
            .workspace_key_for_scope(record.skill.scope)
            .to_string();

        self.store
            .with_transaction("record install", |connection, path| {
                upsert_install_record_in(connection, path, &workspace_key, record)?;
                insert_history_entry_in(connection, path, &workspace_key, &entry)
            })
    }

    /// Record an upstream update check and append a matching history entry.
    pub fn record_update_check(&mut self, record: &UpdateCheckRecord) -> Result<i64, AppError> {
        self.record_update_check_with_trust(record, None)
    }

    /// Record an upstream update check with trust details.
    pub fn record_update_check_with_trust(
        &mut self,
        record: &UpdateCheckRecord,
        trust: Option<&SkillTrust>,
    ) -> Result<i64, AppError> {
        let entry = HistoryEntry {
            id: None,
            kind: HistoryEventKind::UpdateCheck,
            scope: Some(record.skill.scope),
            skill_id: Some(record.skill.skill_id.clone()),
            target: None,
            occurred_at: record.checked_at.clone(),
            summary: format!("Checked {} for updates", record.skill.skill_id),
            details: update_check_details(record, trust),
        };
        let workspace_key = self
            .store
            .workspace_key_for_scope(record.skill.scope)
            .to_string();

        self.store
            .with_transaction("record update check", |connection, path| {
                insert_update_check_record_in(connection, path, &workspace_key, record)?;
                insert_history_entry_in(connection, path, &workspace_key, &entry)
            })
    }

    /// Record an applied update by replacing install state and appending history.
    pub fn record_update_applied(
        &mut self,
        previous_revision: &str,
        record: &InstallRecord,
    ) -> Result<i64, AppError> {
        let mut details = install_details(record, None);
        details.insert("previous_revision".to_string(), json!(previous_revision));

        let entry = HistoryEntry {
            id: None,
            kind: HistoryEventKind::UpdateApplied,
            scope: Some(record.skill.scope),
            skill_id: Some(record.skill.skill_id.clone()),
            target: None,
            occurred_at: record.updated_at.clone(),
            summary: format!(
                "Updated {} to {}",
                record.skill.skill_id, record.resolved_revision
            ),
            details,
        };
        let workspace_key = self
            .store
            .workspace_key_for_scope(record.skill.scope)
            .to_string();

        self.store
            .with_transaction("record applied update", |connection, path| {
                upsert_install_record_in(connection, path, &workspace_key, record)?;
                insert_history_entry_in(connection, path, &workspace_key, &entry)
            })
    }

    /// Record a projection and append a matching history entry.
    pub fn record_projection(&mut self, record: &ProjectionRecord) -> Result<i64, AppError> {
        let entry = HistoryEntry {
            id: None,
            kind: HistoryEventKind::Projection,
            scope: Some(record.skill.scope),
            skill_id: Some(record.skill.skill_id.clone()),
            target: Some(record.target),
            occurred_at: record.generated_at.clone(),
            summary: format!(
                "Projected {} into {}",
                record.skill.skill_id,
                record.target.as_str()
            ),
            details: projection_details(record),
        };
        let workspace_key = self
            .store
            .workspace_key_for_scope(record.skill.scope)
            .to_string();

        self.store
            .with_transaction("record projection", |connection, path| {
                upsert_projection_record_in(connection, path, &workspace_key, record)?;
                insert_history_entry_in(connection, path, &workspace_key, &entry)
            })
    }

    /// Record a detected local modification and append a matching history entry.
    pub fn record_local_modification(
        &mut self,
        record: &LocalModificationRecord,
    ) -> Result<i64, AppError> {
        let entry = HistoryEntry {
            id: None,
            kind: HistoryEventKind::DirectModificationDetected,
            scope: Some(record.skill.scope),
            skill_id: Some(record.skill.skill_id.clone()),
            target: None,
            occurred_at: record.detected_at.clone(),
            summary: format!("Detected local modification for {}", record.skill.skill_id),
            details: local_modification_details(record),
        };
        let workspace_key = self
            .store
            .workspace_key_for_scope(record.skill.scope)
            .to_string();

        self.store
            .with_transaction("record local modification", |connection, path| {
                insert_local_modification_record_in(connection, path, &workspace_key, record)?;
                insert_history_entry_in(connection, path, &workspace_key, &entry)
            })
    }

    /// Record a current active pin and append a matching history entry.
    pub fn record_pin(&mut self, record: &PinRecord) -> Result<i64, AppError> {
        let entry = HistoryEntry {
            id: None,
            kind: HistoryEventKind::Pin,
            scope: Some(record.skill.scope),
            skill_id: Some(record.skill.skill_id.clone()),
            target: None,
            occurred_at: record.pinned_at.clone(),
            summary: format!(
                "Pinned {} to {}",
                record.skill.skill_id, record.requested_reference
            ),
            details: pin_details(record),
        };
        let workspace_key = self
            .store
            .workspace_key_for_scope(record.skill.scope)
            .to_string();

        self.store
            .with_transaction("record pin", |connection, path| {
                upsert_pin_record_in(connection, path, &workspace_key, record)?;
                insert_history_entry_in(connection, path, &workspace_key, &entry)
            })
    }

    /// Record a local review decision and append a matching history entry.
    ///
    /// This is the only write path for `skill_reviews`, so a decision can never
    /// exist without its audit entry: both rows commit in one transaction.
    pub fn record_trust_review(&mut self, record: &TrustReviewRecord) -> Result<i64, AppError> {
        let entry = HistoryEntry {
            id: None,
            kind: HistoryEventKind::TrustReview,
            scope: Some(record.skill.scope),
            skill_id: Some(record.skill.skill_id.clone()),
            target: None,
            occurred_at: record.reviewed_at.clone(),
            summary: match record.decision {
                ReviewDecision::Approved => format!(
                    "Reviewed {} at {}",
                    record.skill.skill_id, record.resolved_revision
                ),
                ReviewDecision::Revoked => {
                    format!("Revoked the recorded review of {}", record.skill.skill_id)
                }
            },
            details: trust_review_details(record),
        };
        let workspace_key = self
            .store
            .workspace_key_for_scope(record.skill.scope)
            .to_string();

        self.store
            .with_transaction("record trust review", |connection, path| {
                upsert_trust_review_in(connection, path, &workspace_key, record)?;
                insert_history_entry_in(connection, path, &workspace_key, &entry)
            })
    }

    /// Record a rollback transition and append a matching history entry.
    pub fn record_rollback(&mut self, record: &RollbackRecord) -> Result<i64, AppError> {
        let entry = HistoryEntry {
            id: None,
            kind: HistoryEventKind::Rollback,
            scope: Some(record.skill.scope),
            skill_id: Some(record.skill.skill_id.clone()),
            target: None,
            occurred_at: record.rolled_back_at.clone(),
            summary: format!("Rolled back {}", record.skill.skill_id),
            details: rollback_details(record),
        };
        let workspace_key = self
            .store
            .workspace_key_for_scope(record.skill.scope)
            .to_string();

        self.store
            .with_transaction("record rollback", |connection, path| {
                insert_rollback_record_in(connection, path, &workspace_key, record)?;
                insert_history_entry_in(connection, path, &workspace_key, &entry)
            })
    }

    /// Record overlay creation in the history ledger.
    pub fn record_overlay_created(
        &mut self,
        skill: &ManagedSkillRef,
        overlay_root: &str,
        occurred_at: &str,
    ) -> Result<i64, AppError> {
        let mut details = BTreeMap::new();
        details.insert("overlay_root".to_string(), json!(overlay_root));

        self.store.append_history_entry(&HistoryEntry {
            id: None,
            kind: HistoryEventKind::OverlayCreated,
            scope: Some(skill.scope),
            skill_id: Some(skill.skill_id.clone()),
            target: None,
            occurred_at: occurred_at.to_string(),
            summary: format!("Created overlay for {}", skill.skill_id),
            details,
        })
    }

    /// Record a detach transition by replacing install state and appending history.
    pub fn record_detach(
        &mut self,
        record: &InstallRecord,
        local_root: &str,
    ) -> Result<i64, AppError> {
        if !record.detached {
            return Err(AppError::LocalStateValidation {
                path: self.store.path().to_path_buf(),
                message: "detached install records must set detached=true".to_string(),
            });
        }

        let mut details = install_details(record, None);
        details.insert("local_root".to_string(), json!(local_root));

        let entry = HistoryEntry {
            id: None,
            kind: HistoryEventKind::Detach,
            scope: Some(record.skill.scope),
            skill_id: Some(record.skill.skill_id.clone()),
            target: None,
            occurred_at: record.updated_at.clone(),
            summary: format!("Detached {} into {}", record.skill.skill_id, local_root),
            details,
        };
        let workspace_key = self
            .store
            .workspace_key_for_scope(record.skill.scope)
            .to_string();

        self.store
            .with_transaction("record detach", |connection, path| {
                upsert_install_record_in(connection, path, &workspace_key, record)?;
                insert_history_entry_in(connection, path, &workspace_key, &entry)
            })
    }

    /// Record a fork transition by replacing install state and appending history.
    pub fn record_fork(
        &mut self,
        record: &InstallRecord,
        local_root: &str,
    ) -> Result<i64, AppError> {
        if !record.forked {
            return Err(AppError::LocalStateValidation {
                path: self.store.path().to_path_buf(),
                message: "forked install records must set forked=true".to_string(),
            });
        }

        let mut details = install_details(record, None);
        details.insert("local_root".to_string(), json!(local_root));

        let entry = HistoryEntry {
            id: None,
            kind: HistoryEventKind::Fork,
            scope: Some(record.skill.scope),
            skill_id: Some(record.skill.skill_id.clone()),
            target: None,
            occurred_at: record.updated_at.clone(),
            summary: format!("Forked {} into {}", record.skill.skill_id, local_root),
            details,
        };
        let workspace_key = self
            .store
            .workspace_key_for_scope(record.skill.scope)
            .to_string();

        self.store
            .with_transaction("record fork", |connection, path| {
                upsert_install_record_in(connection, path, &workspace_key, record)?;
                insert_history_entry_in(connection, path, &workspace_key, &entry)
            })
    }

    /// Record a cleanup action in the append-only history ledger.
    pub fn record_cleanup(
        &mut self,
        skill: Option<&ManagedSkillRef>,
        cleaned_path: &str,
        occurred_at: &str,
    ) -> Result<i64, AppError> {
        self.store.append_history_entry(&HistoryEntry {
            id: None,
            kind: HistoryEventKind::Cleanup,
            scope: skill.map(|skill| skill.scope),
            skill_id: skill.map(|skill| skill.skill_id.clone()),
            target: None,
            occurred_at: occurred_at.to_string(),
            summary: cleanup_summary("Cleaned", skill, cleaned_path),
            details: path_details(cleaned_path),
        })
    }

    /// Record a stale projection prune action in the append-only history ledger.
    pub fn record_prune(
        &mut self,
        skill: Option<&ManagedSkillRef>,
        pruned_path: &str,
        occurred_at: &str,
    ) -> Result<i64, AppError> {
        self.store.append_history_entry(&HistoryEntry {
            id: None,
            kind: HistoryEventKind::Prune,
            scope: skill.map(|skill| skill.scope),
            skill_id: skill.map(|skill| skill.skill_id.clone()),
            target: None,
            occurred_at: occurred_at.to_string(),
            summary: cleanup_summary("Pruned", skill, pruned_path),
            details: path_details(pruned_path),
        })
    }

    /// Record telemetry consent state and append a matching history entry.
    pub fn record_telemetry_change(
        &mut self,
        settings: &TelemetrySettings,
    ) -> Result<i64, AppError> {
        let mut details = BTreeMap::new();
        details.insert("consent".to_string(), json!(settings.consent.as_str()));
        if let Some(notice_seen_at) = &settings.notice_seen_at {
            details.insert("notice_seen_at".to_string(), json!(notice_seen_at));
        }

        let entry = HistoryEntry {
            id: None,
            kind: HistoryEventKind::TelemetryConsentChanged,
            scope: None,
            skill_id: None,
            target: None,
            occurred_at: settings.updated_at.clone(),
            summary: format!("Telemetry consent changed to {}", settings.consent.as_str()),
            details,
        };
        let workspace_key = self
            .store
            .workspace_key_for_history_scope(entry.scope)
            .to_string();

        self.store
            .with_transaction("record telemetry change", |connection, path| {
                upsert_telemetry_settings_in(connection, path, settings)?;
                insert_history_entry_in(connection, path, &workspace_key, &entry)
            })
    }
}

/// Typed request for `skillctl pin`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PinRequest {
    /// Managed skill name.
    pub skill: String,
    /// Exact revision to pin.
    pub reference: String,
}

impl PinRequest {
    /// Create a pin request from parsed CLI arguments.
    pub fn new(skill: String, reference: String) -> Self {
        Self { skill, reference }
    }
}

/// Typed request for `skillctl rollback`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RollbackRequest {
    /// Managed skill name.
    pub skill: String,
    /// Previous version or commit identifier.
    pub version_or_commit: String,
}

impl RollbackRequest {
    /// Create a rollback request from parsed CLI arguments.
    pub fn new(skill: String, version_or_commit: String) -> Self {
        Self {
            skill,
            version_or_commit,
        }
    }
}

/// Typed request for `skillctl history`.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct HistoryRequest {
    /// Optional managed skill filter.
    pub skill: Option<String>,
}

impl HistoryRequest {
    /// Create a history request from parsed CLI arguments.
    pub fn new(skill: Option<String>) -> Self {
        Self { skill }
    }
}

/// Handle `skillctl pin`.
pub fn handle_pin(context: &AppContext, request: PinRequest) -> Result<AppResponse, AppError> {
    let store = LocalStateStore::open_default_for(&context.working_directory)?;
    // The state store answers the first question, so a name no install covers,
    // such as a canonical local skill, hears that it is not installed even in a
    // workspace that declares no imports at all.
    let managed_skill =
        skill::resolve_installed_skill(&store, &request.skill, context.selector.scope)?;
    let prestate =
        load_revision_move_prestate(&store, context, "pin", &request.skill, &managed_skill)?;

    // The checkout clones the pinned git source, so it runs before the guarded
    // transaction opens: the coordination lock must not be held across a
    // network clone, which can outlast every other command's lock timeout.
    let checked_out =
        checkout_git_revision(&prestate.install_record.source_url, &request.reference)?;

    run_guarded_revision_move(
        context,
        &request.skill,
        &managed_skill,
        prestate,
        RevisionMove {
            kind: RevisionMoveKind::Pin,
            requested_reference: &request.reference,
            checked_out: &checked_out,
            recorded_effective_version: None,
        },
    )
}

/// Handle `skillctl rollback`.
pub fn handle_rollback(
    context: &AppContext,
    request: RollbackRequest,
) -> Result<AppResponse, AppError> {
    let store = LocalStateStore::open_default_for(&context.working_directory)?;
    // As with `pin`, the state store answers first, so a name no install covers
    // hears that it is not installed instead of a manifest read error.
    let managed_skill =
        skill::resolve_installed_skill(&store, &request.skill, context.selector.scope)?;
    let prestate =
        load_revision_move_prestate(&store, context, "rollback", &request.skill, &managed_skill)?;

    // The recorded effective version pairs the target revision with the
    // overlay that existed when it was recorded. An overlay that changed
    // since then recomputes to a new effective version, so the move keeps
    // the current overlay and records that new effective version, exactly
    // as `pin` does for the same revision.
    let target =
        resolve_recorded_version(&store, &managed_skill, &prestate.install_record, &request)?;
    // As with `pin`, the checkout clones the git source of the recorded
    // revision before the guarded transaction opens: the coordination lock
    // must not be held across a network clone, which can outlast every other
    // command's lock timeout.
    let checked_out = checkout_git_revision(
        &prestate.install_record.source_url,
        &target.resolved_revision,
    )?;

    run_guarded_revision_move(
        context,
        &request.skill,
        &managed_skill,
        prestate,
        RevisionMove {
            kind: RevisionMoveKind::Rollback,
            requested_reference: &request.version_or_commit,
            checked_out: &checked_out,
            recorded_effective_version: target.effective_version_hash,
        },
    )
}

/// Which revision move one handler runs.
///
/// The two moves share their whole guarded sequence and differ only in the
/// labels they report, the mutation and trust-gate operations they are
/// classified under, and which records the settled move writes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RevisionMoveKind {
    /// `pin` records the move through the pin history callback.
    Pin,
    /// `rollback` writes the pin row directly and records the rollback.
    Rollback,
}

impl RevisionMoveKind {
    /// Command label, for required-state refusals and the response.
    const fn command(self) -> &'static str {
        match self {
            Self::Pin => "pin",
            Self::Rollback => "rollback",
        }
    }

    /// Mutation operation the plan records.
    const fn operation(self) -> crate::apply::MutationOperation {
        match self {
            Self::Pin => crate::apply::MutationOperation::Pin,
            Self::Rollback => crate::apply::MutationOperation::Rollback,
        }
    }

    /// Trust-gate operation the move is decided under.
    const fn gate(self) -> crate::error::TrustGateOperation {
        match self {
            Self::Pin => crate::error::TrustGateOperation::Pin,
            Self::Rollback => crate::error::TrustGateOperation::Rollback,
        }
    }

    /// Past-tense verb the response summary opens with.
    const fn summary_verb(self) -> &'static str {
        match self {
            Self::Pin => "Pinned",
            Self::Rollback => "Rolled back",
        }
    }
}

/// The per-command inputs of one guarded revision move.
struct RevisionMove<'a> {
    /// Which command the move serves.
    kind: RevisionMoveKind,
    /// The reference the request named.
    requested_reference: &'a str,
    /// Staged checkout of the revision the move installs.
    checked_out: &'a CheckedOutSource,
    /// Recorded effective version a rollback reports back.
    recorded_effective_version: Option<String>,
}

/// The pre-state `pin` and `rollback` share.
struct RevisionMovePrestate {
    /// Committed manifest whose import declaration the move rewrites.
    manifest: WorkspaceManifest,
    /// Committed lockfile whose entry the move rewrites.
    lockfile: WorkspaceLockfile,
    /// Install record the move rewrites.
    install_record: InstallRecord,
    /// Expected import state the plan pins against a moved workspace.
    expected: crate::apply::ExpectedImport,
    /// Context scoped to the skill's own scope.
    scoped_context: AppContext,
    /// Stored import root the move replaces.
    stored_root: PathBuf,
}

/// Load the pre-state `pin` and `rollback` share.
///
/// Both commands require the committed manifest and lockfile, re-read the
/// install record they rewrite, and plan against the stored import root. A
/// missing lockfile refuses with the repairs instead of a raw read error,
/// because `skillctl init` would write an empty lockfile and drop every pin
/// the workspace already holds.
fn load_revision_move_prestate(
    store: &LocalStateStore,
    context: &AppContext,
    command: &'static str,
    skill_name: &str,
    managed_skill: &ManagedSkillRef,
) -> Result<RevisionMovePrestate, AppError> {
    // The mutation rewrites the import declaration, so it needs the committed
    // manifest rather than the documented defaults a bare workspace runs on.
    let manifest = WorkspaceManifest::load_required_from_workspace(context, command)?;
    let lockfile = WorkspaceLockfile::load_required_from_workspace(context, command)?;
    let install_record =
        store
            .install_record(managed_skill)?
            .ok_or_else(|| AppError::ResolutionValidation {
                message: format!("skill '{skill_name}' does not have an installed state record"),
            })?;
    ensure_pin_supported(&install_record)?;
    let expected = crate::apply::capture_expected_import(context, &install_record)?;
    let scoped_context = context_for_scope(context, managed_skill.scope);
    let stored_root = stored_import_root(
        managed_skill.scope,
        &context.working_directory,
        &managed_skill.skill_id,
    )?;

    Ok(RevisionMovePrestate {
        manifest,
        lockfile,
        install_record,
        expected,
        scoped_context,
        stored_root,
    })
}

/// Move one import to an explicitly named revision inside one guarded
/// transaction.
///
/// Both handlers resolve their target revision and check it out while no
/// transaction is open, then hand the checkout here. The guarded closure
/// re-reads the install record under the lock, and the plan verification
/// re-checks the pre-state, so a workspace that moved underneath the checkout
/// refuses before anything changes.
fn run_guarded_revision_move(
    context: &AppContext,
    skill_name: &str,
    managed_skill: &ManagedSkillRef,
    prestate: RevisionMovePrestate,
    revision_move: RevisionMove<'_>,
) -> Result<AppResponse, AppError> {
    let RevisionMove {
        kind,
        requested_reference,
        checked_out,
        recorded_effective_version,
    } = revision_move;
    let RevisionMovePrestate {
        manifest,
        lockfile,
        install_record: _,
        expected,
        scoped_context,
        stored_root,
    } = prestate;
    let plan = crate::apply::MutationPlanBuilder::new(kind.operation(), &context.working_directory)
        .skill(managed_skill.clone())
        .expect_import(expected)
        .propose(crate::apply::ProposedImport {
            import_id: managed_skill.skill_id.clone(),
            revision: Some(requested_reference.to_string()),
            bundle_digest: None,
            bundle_policy: crate::apply::BundlePolicy::Recompute,
            summary: match kind {
                RevisionMoveKind::Pin => format!(
                    "pin '{}' to '{}'",
                    managed_skill.skill_id, requested_reference
                ),
                RevisionMoveKind::Rollback => format!(
                    "roll '{}' back to '{}'",
                    managed_skill.skill_id, requested_reference
                ),
            },
        })
        .target(manifest.path.clone(), crate::apply::TargetAction::Update)
        .target(lockfile.path, crate::apply::TargetAction::Update)
        .targets(
            materialize::sync_mutation_root_paths(
                &scoped_context,
                &manifest,
                TargetScope::from(managed_skill.scope),
            )?,
            crate::apply::TargetAction::Replace,
        )
        .target(stored_root, crate::apply::TargetAction::Replace)
        .seal();

    crate::apply::run_guarded(&plan, context, |transaction| {
        transaction.track_state_database()?;
        let mut manifest = WorkspaceManifest::load_from_workspace(&context.working_directory)?;
        let mut lockfile = WorkspaceLockfile::load_from_workspace(&context.working_directory)?;
        let mut store = LocalStateStore::open_default_for(&context.working_directory)?;
        let install_record =
            store
                .install_record(managed_skill)?
                .ok_or_else(|| AppError::ResolutionValidation {
                    message: format!(
                        "skill '{skill_name}' does not have an installed state record"
                    ),
                })?;
        ensure_pin_supported(&install_record)?;

        // A revision move could run as an update apply, so the same trust
        // gate decides it. The refusal names the review and the fork that let
        // an explicit revision move through, so a gated move keeps its
        // recovery path instead of dead-ending the workspace.
        crate::apply::ensure_revision_apply_is_allowed(
            context,
            &store,
            &lockfile,
            &install_record,
            &checked_out.resolved_revision,
            &checked_out.root,
            kind.gate(),
        )?;
        let outcome = crate::apply::apply_import_revision(
            context,
            transaction,
            &mut manifest,
            &mut lockfile,
            managed_skill,
            &crate::apply::RevisionTarget {
                manifest_reference: Some(requested_reference),
                resolved_revision: &checked_out.resolved_revision,
                source_root: &checked_out.root,
                discovery: None,
            },
        )?;

        let sync_report = materialize::sync_workspace(&scoped_context)?;
        let timestamp = current_timestamp();

        let updated_install = InstallRecord {
            resolved_revision: outcome.resolved_revision.clone(),
            // The move names one revision explicitly, so it observes nothing
            // upstream and no upstream revision is recorded for it.
            upstream_revision: crate::source::observed_upstream_revision(
                Some(requested_reference),
                &outcome.resolved_revision,
            ),
            content_hash: outcome.content_hash.clone(),
            overlay_hash: outcome.overlay_hash.clone(),
            effective_version_hash: outcome.effective_version_hash.clone(),
            updated_at: timestamp.clone(),
            detached: false,
            forked: false,
            ..install_record
        };
        let pin_record = PinRecord {
            skill: managed_skill.clone(),
            requested_reference: requested_reference.to_string(),
            resolved_revision: outcome.resolved_revision.clone(),
            effective_version_hash: Some(outcome.effective_version_hash.clone()),
            pinned_at: timestamp.clone(),
        };
        // A rollback also records what the import moved from, which is the
        // revision the guarded closure re-read, not the one the plan was built
        // against.
        let rollback_record = match kind {
            RevisionMoveKind::Rollback => Some(RollbackRecord {
                id: None,
                skill: managed_skill.clone(),
                rolled_back_at: timestamp.clone(),
                from_reference: install_record.resolved_revision,
                to_reference: requested_reference.to_string(),
            }),
            RevisionMoveKind::Pin => None,
        };

        store.upsert_install_record(&updated_install)?;
        if rollback_record.is_some() {
            store.upsert_pin_record(&pin_record)?;
        }
        match &rollback_record {
            Some(rollback_record) => {
                materialize::refresh_projection_state_for_scope(
                    &mut store,
                    context,
                    managed_skill.scope,
                    &sync_report,
                    &timestamp,
                    |ledger| ledger.record_rollback(rollback_record).map(|_| ()),
                )?;
            }
            None => {
                materialize::refresh_projection_state_for_scope(
                    &mut store,
                    context,
                    managed_skill.scope,
                    &sync_report,
                    &timestamp,
                    |ledger| ledger.record_pin(&pin_record).map(|_| ()),
                )?;
            }
        }

        let mut data = json!({
            "skill": skill_name,
            "scope": managed_skill.scope.as_str(),
            "resolved_revision": outcome.resolved_revision,
            "effective_version_hash": outcome.effective_version_hash,
            "bundle": outcome.bundle,
            "diff": outcome.diff,
            "projection": sync_report,
        });
        match kind {
            RevisionMoveKind::Pin => {
                data["requested_reference"] = json!(requested_reference);
            }
            RevisionMoveKind::Rollback => {
                data["requested_version"] = json!(requested_reference);
                data["recorded_effective_version"] = json!(recorded_effective_version);
            }
        }
        let mut response = AppResponse::success(kind.command())
            .with_summary(format!(
                "{} {} to {}",
                kind.summary_verb(),
                skill_name,
                outcome.resolved_revision
            ))
            .with_data(data);
        for warning in &sync_report.warnings {
            response = response.with_warning(warning.clone());
        }

        Ok(response)
    })
}

/// Handle `skillctl history`.
///
/// An install record or a recorded ledger row answers first. A canonical
/// local skill in the workspace skills root has neither, because the ledger
/// records lifecycle events for managed imports, so a name without one falls
/// back to that root and reports the empty ledger instead of refusing.
/// `explain` and `path` resolve the same skills, and the README lists
/// `history` among the four commands to remember, so the commands must not
/// disagree about a canonical skill.
///
/// A scope selector without a skill name filters the listing, the same way
/// `list` and the TUI history panel filter it, so a ledger with rows in both
/// scopes answers with the requested scope only.
pub fn handle_history(
    context: &AppContext,
    request: HistoryRequest,
) -> Result<AppResponse, AppError> {
    let requested_skill = match (&request.skill, &context.selector.skill_name) {
        (Some(positional), Some(global)) if positional != global => {
            return Err(AppError::ResolutionValidation {
                message: format!(
                    "conflicting skill selectors '{}'(argument) and '{}'(--name)",
                    positional, global
                ),
            });
        }
        (Some(positional), _) => Some(positional.clone()),
        (None, Some(global)) => Some(global.clone()),
        (None, None) => None,
    };
    let store = LocalStateStore::open_default_for(&context.working_directory)?;

    let (query, filter) = match requested_skill {
        Some(skill_name) => {
            let resolved = resolve_history_skill(context, &store, &skill_name)?;
            (
                HistoryQuery::for_skill(resolved.skill().clone()),
                Some(resolved),
            )
        }
        None => (HistoryQuery::default(), None),
    };
    let mut entries = store.history_entries(&query)?;

    // The ledger returns both scopes' rows when no skill is named, so the
    // requested scope narrows the listing the same way the TUI history panel
    // does. Scope-less events stay, because they belong to neither scope.
    if filter.is_none()
        && let Some(scope) = context.selector.scope.map(cli_scope)
    {
        entries.retain(|entry| entry.scope.is_none_or(|entry_scope| entry_scope == scope));
    }

    let summary = match &filter {
        Some(resolved) if entries.is_empty() => {
            format!(
                "No history entries recorded for {}.",
                resolved.skill().skill_id
            )
        }
        Some(resolved) => format!(
            "Showing {} history entr{} for {}.",
            entries.len(),
            if entries.len() == 1 { "y" } else { "ies" },
            resolved.skill().skill_id
        ),
        None => format!(
            "Showing {} history entr{}.",
            entries.len(),
            if entries.len() == 1 { "y" } else { "ies" }
        ),
    };

    let mut data = json!({
        "skill": filter.as_ref().map(|resolved| resolved.skill().skill_id.clone()),
        "scope": filter.as_ref().map(|resolved| resolved.skill().scope.as_str()),
        "entries": entries,
    });
    let mut detail = crate::human::history_detail_lines(&data);
    if let Some(HistorySkill::CanonicalLocal {
        skill,
        canonical_root,
    }) = filter.as_ref()
    {
        data["source"] = json!("canonical-local");
        data["canonical_root"] = json!(crate::planner::display_path(context, canonical_root));
        data["note"] = json!(canonical_history_note(skill));
        detail.extend(canonical_history_detail_lines(
            context,
            skill,
            canonical_root,
        ));
    }

    Ok(AppResponse::success("history")
        .with_summary(summary)
        .with_detail_lines(detail)
        .with_data(data))
}

/// Where one per-skill `history` request resolved.
enum HistorySkill {
    /// An install record or a recorded ledger row names the skill.
    Managed(ManagedSkillRef),
    /// No install record and no ledger row names the skill, and the workspace
    /// skills root holds an authored canonical skill with that name instead.
    CanonicalLocal {
        skill: ManagedSkillRef,
        canonical_root: PathBuf,
    },
}

impl HistorySkill {
    fn skill(&self) -> &ManagedSkillRef {
        match self {
            Self::Managed(skill) | Self::CanonicalLocal { skill, .. } => skill,
        }
    }
}

fn resolve_history_skill(
    context: &AppContext,
    store: &LocalStateStore,
    skill_name: &str,
) -> Result<HistorySkill, AppError> {
    match skill::resolve_installed_skill(store, skill_name, context.selector.scope) {
        Ok(skill) => Ok(HistorySkill::Managed(skill)),
        Err(AppError::ResolutionValidation { .. }) => {
            let entries = store.history_entries(&HistoryQuery::default())?;
            let mut matches = BTreeMap::new();

            for entry in entries {
                let Some(entry_skill) = entry.skill_id.as_deref() else {
                    continue;
                };
                if entry_skill != skill_name {
                    continue;
                }

                let Some(entry_scope) = entry.scope else {
                    continue;
                };
                if context
                    .selector
                    .scope
                    .is_some_and(|scope| entry_scope != cli_scope(scope))
                {
                    continue;
                }
                matches.insert(entry_scope.as_str().to_string(), entry_scope);
            }

            match matches.len() {
                0 => canonical_history_skill(context, skill_name),
                1 => matches.values().next().copied().map_or_else(
                    || canonical_history_skill(context, skill_name),
                    |scope| {
                        Ok(HistorySkill::Managed(ManagedSkillRef::new(
                            scope, skill_name,
                        )))
                    },
                ),
                _ => Err(AppError::ResolutionValidation {
                    message: format!(
                        "skill '{}' has recorded history in multiple scopes; re-run with --scope",
                        skill_name
                    ),
                }),
            }
        }
        Err(error) => Err(error),
    }
}

/// Resolve a name no install record and no ledger row covers.
///
/// This mirrors the `path` fallback: a canonical local skill never enters the
/// ledger, because `sync` materializes it straight from its canonical root
/// and only managed imports carry records, so the request answers from the
/// workspace layout. An explicit user-scope selector keeps refusing, because
/// canonical local skills belong to the workspace.
fn canonical_history_skill(
    context: &AppContext,
    skill_name: &str,
) -> Result<HistorySkill, AppError> {
    let manifest = skill::load_manifest_or_default(&context.working_directory)?;
    if context.selector.scope == Some(Scope::User) {
        return Err(history_refusal(context, &manifest, skill_name));
    }
    let Some(canonical_root) = skill::canonical_local_root(context, &manifest, skill_name) else {
        return Err(history_refusal(context, &manifest, skill_name));
    };
    if materialize::is_skillctl_generated_directory(&canonical_root)? {
        // The directory holds a generated copy of some install, not a skill
        // someone authored, so the ledger-scope refusal stays.
        return Err(AppError::ResolutionValidation {
            message: format!(
                "skill '{skill_name}' has no recorded history and '{}' is a generated projection \
                 rather than a canonical local skill; the history ledger records lifecycle events \
                 for managed imports, so run 'skillctl explain {skill_name}' to see every \
                 candidate it found",
                crate::planner::display_path(context, &canonical_root),
            ),
        });
    }

    Ok(HistorySkill::CanonicalLocal {
        skill: ManagedSkillRef::new(ManagedScope::Workspace, skill_name),
        canonical_root,
    })
}

/// The refusal `skillctl history` reports for a name the ledger does not cover.
///
/// The message names the ledger's scope and the root it consulted, because a
/// bare "no recorded history" hides that the ledger only follows managed
/// imports. `explain` is the command that reports every candidate it found,
/// shadowed or not, so the refusal hands the name over to it.
fn history_refusal(context: &AppContext, manifest: &WorkspaceManifest, skill: &str) -> AppError {
    let skills_root = crate::planner::display_path(
        context,
        &context
            .working_directory
            .join(manifest.layout.skills_dir.as_str()),
    );
    let message = if context.selector.scope == Some(Scope::User) {
        format!(
            "skill '{skill}' has no recorded history at user scope and canonical local skills live \
             in the workspace at '{skills_root}'; the history ledger records lifecycle events for \
             managed imports, so run 'skillctl explain {skill}' to see every candidate it found",
        )
    } else {
        format!(
            "skill '{skill}' has no recorded history and no canonical local skill exists at \
             '{skills_root}/{skill}'; the history ledger records lifecycle events for managed \
             imports, so run 'skillctl explain {skill}' to see every candidate it found",
        )
    };
    AppError::ResolutionValidation { message }
}

/// The note that documents why one canonical local skill holds no entries.
///
/// `sync` projects a canonical skill straight from its canonical root, so it
/// records no install and no projection event for it: the ledger follows
/// managed imports, and the projections live in `explain` and `path` instead.
fn canonical_history_note(skill: &ManagedSkillRef) -> String {
    format!(
        "The history ledger records lifecycle events for managed imports. A canonical local skill \
         is projected by 'skillctl sync' straight from its canonical root without a recorded \
         event; run 'skillctl explain {}' to see its projections.",
        skill.skill_id
    )
}

/// The detail lines that carry the canonical local note in human output.
fn canonical_history_detail_lines(
    context: &AppContext,
    skill: &ManagedSkillRef,
    canonical_root: &Path,
) -> Vec<String> {
    vec![
        format!(
            "  canonical local skill at {}",
            crate::planner::display_path(context, canonical_root),
        ),
        format!(
            "  no ledger events: sync projects it straight from that root; run 'skillctl explain \
             {}' for its projections",
            skill.skill_id
        ),
    ]
}

fn cli_scope(scope: Scope) -> ManagedScope {
    match scope {
        Scope::Workspace => ManagedScope::Workspace,
        Scope::User => ManagedScope::User,
    }
}

pub(crate) fn context_for_scope(context: &AppContext, scope: ManagedScope) -> AppContext {
    let mut scoped = context.clone();
    scoped.selector.scope = Some(match scope {
        ManagedScope::Workspace => Scope::Workspace,
        ManagedScope::User => Scope::User,
    });
    scoped
}

#[derive(Debug)]
pub(crate) struct CheckedOutSource {
    pub(crate) root: PathBuf,
    pub(crate) resolved_revision: String,
    _staging_dir: TempDir,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RecordedVersion {
    requested_reference: Option<String>,
    resolved_revision: String,
    effective_version_hash: Option<String>,
}

impl RecordedVersion {
    fn matches(&self, target: &str) -> bool {
        self.requested_reference.as_deref() == Some(target)
            || self.resolved_revision == target
            || self.effective_version_hash.as_deref() == Some(target)
    }
}

fn ensure_pin_supported(install: &InstallRecord) -> Result<(), AppError> {
    if install.detached || install.forked {
        return Err(AppError::ResolutionValidation {
            message: format!(
                "skill '{}' is detached from upstream lifecycle management",
                install.skill.skill_id
            ),
        });
    }

    if install.source_kind != SourceKind::Git {
        return Err(AppError::ResolutionValidation {
            message: format!(
                "skill '{}' only supports pin and rollback for git imports",
                install.skill.skill_id
            ),
        });
    }

    Ok(())
}

pub(crate) fn overlay_hash(
    manifest: &WorkspaceManifest,
    working_directory: &Path,
    import_id: &str,
) -> Result<String, AppError> {
    manifest
        .overrides
        .get(import_id)
        .map(|overlay_path| hash_overlay_root(&working_directory.join(overlay_path.as_str())))
        .transpose()?
        .map_or_else(|| Ok(NO_OVERLAY_HASH.to_string()), Ok)
}

pub(crate) fn checkout_git_revision(
    source_url: &str,
    reference: &str,
) -> Result<CheckedOutSource, AppError> {
    let staging_dir = TempDir::new().map_err(|source| AppError::FilesystemOperation {
        action: "create pinned git staging directory",
        path: std::env::temp_dir(),
        source,
    })?;
    let checkout_path = staging_dir.path().join("checkout");

    run_git(
        &[
            OsStr::new("clone"),
            OsStr::new("--quiet"),
            OsStr::new(source_url),
            checkout_path.as_os_str(),
        ],
        None,
        "clone pinned git source",
    )?;
    run_git(
        &[
            OsStr::new("checkout"),
            OsStr::new("--quiet"),
            OsStr::new(reference),
        ],
        Some(&checkout_path),
        "checkout pinned git revision",
    )?;
    let resolved_revision = run_git_capture(
        &[OsStr::new("rev-parse"), OsStr::new("HEAD")],
        Some(&checkout_path),
        "resolve pinned git revision",
    )?;

    Ok(CheckedOutSource {
        root: checkout_path,
        resolved_revision,
        _staging_dir: staging_dir,
    })
}

fn run_git(
    args: &[&OsStr],
    current_dir: Option<&Path>,
    action: &'static str,
) -> Result<(), AppError> {
    run_git_capture(args, current_dir, action).map(|_| ())
}

/// Run one git subprocess in a pinned checkout and capture standard output.
///
/// A spawn failure is reported against the `git` binary itself, matching every
/// other git subprocess the crate runs, instead of blaming the checkout path.
fn run_git_capture(
    args: &[&OsStr],
    current_dir: Option<&Path>,
    action: &'static str,
) -> Result<String, AppError> {
    // The shared builder applies the controlled git environment (no prompts,
    // no inherited git configuration), so pinned checkouts observe the same
    // rules as every other git subprocess the crate runs.
    let mut command = crate::source::git_command(args);
    if let Some(current_dir) = current_dir {
        command.current_dir(current_dir);
    }

    let output = command
        .output()
        .map_err(|source| AppError::ExternalCommand {
            command: "git".to_string(),
            source,
        })?;
    if !output.status.success() {
        let stderr = crate::source::bounded_command_stderr(&output.stderr);
        let detail = if stderr.is_empty() {
            "git command returned a non-zero exit status".to_string()
        } else {
            stderr
        };
        return Err(AppError::ResolutionValidation {
            message: format!("{action} failed: {detail}"),
        });
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn resolve_recorded_version(
    store: &LocalStateStore,
    skill: &ManagedSkillRef,
    install: &InstallRecord,
    request: &RollbackRequest,
) -> Result<RecordedVersion, AppError> {
    let requested = request.version_or_commit.as_str();
    let current = RecordedVersion {
        requested_reference: None,
        resolved_revision: install.resolved_revision.clone(),
        effective_version_hash: Some(install.effective_version_hash.clone()),
    };
    if current.matches(requested) {
        return Ok(current);
    }

    if let Some(pin) = store.pin_record(skill)? {
        let candidate = RecordedVersion {
            requested_reference: Some(pin.requested_reference),
            resolved_revision: pin.resolved_revision,
            effective_version_hash: pin.effective_version_hash,
        };
        if candidate.matches(requested) {
            return Ok(candidate);
        }
    }

    for entry in store.history_entries(&HistoryQuery::for_skill(skill.clone()))? {
        let Some(candidate) = recorded_version_from_history(&entry) else {
            continue;
        };
        if candidate.matches(requested) {
            return Ok(candidate);
        }
    }

    Err(AppError::ResolutionValidation {
        message: format!(
            "version '{}' is not recorded for skill '{}'; run 'skillctl pin {} {}' to move this \
             import to a revision the local history does not record",
            request.version_or_commit, request.skill, request.skill, request.version_or_commit
        ),
    })
}

fn recorded_version_from_history(entry: &HistoryEntry) -> Option<RecordedVersion> {
    match entry.kind {
        HistoryEventKind::Install
        | HistoryEventKind::UpdateApplied
        | HistoryEventKind::Detach
        | HistoryEventKind::Fork => Some(RecordedVersion {
            requested_reference: None,
            resolved_revision: history_string(&entry.details, "resolved_revision")?,
            effective_version_hash: history_string(&entry.details, "effective_version_hash"),
        }),
        HistoryEventKind::Pin => Some(RecordedVersion {
            requested_reference: history_string(&entry.details, "requested_reference"),
            resolved_revision: history_string(&entry.details, "resolved_revision")?,
            effective_version_hash: history_string(&entry.details, "effective_version_hash"),
        }),
        _ => None,
    }
}

fn history_string(details: &HistoryDetails, key: &str) -> Option<String> {
    details.get(key).and_then(Value::as_str).map(str::to_string)
}

fn install_details(record: &InstallRecord, trust: Option<&SkillTrust>) -> HistoryDetails {
    let mut details = BTreeMap::new();
    details.insert("scope".to_string(), json!(record.skill.scope.as_str()));
    details.insert("source_kind".to_string(), json!(record.source_kind.label()));
    details.insert("source_url".to_string(), json!(record.source_url));
    details.insert("source_subpath".to_string(), json!(record.source_subpath));
    details.insert(
        "resolved_revision".to_string(),
        json!(record.resolved_revision),
    );
    if let Some(upstream_revision) = &record.upstream_revision {
        details.insert("upstream_revision".to_string(), json!(upstream_revision));
    }
    details.insert("content_hash".to_string(), json!(record.content_hash));
    details.insert("overlay_hash".to_string(), json!(record.overlay_hash));
    details.insert(
        "effective_version_hash".to_string(),
        json!(record.effective_version_hash),
    );
    details.insert("detached".to_string(), json!(record.detached));
    details.insert("forked".to_string(), json!(record.forked));
    if let Some(trust) = trust {
        details.insert("trust".to_string(), json!(trust));
    }
    details
}

fn projection_details(record: &ProjectionRecord) -> HistoryDetails {
    let mut details = BTreeMap::new();
    details.insert("target".to_string(), json!(record.target.as_str()));
    details.insert(
        "generation_mode".to_string(),
        json!(record.generation_mode.as_str()),
    );
    details.insert("physical_root".to_string(), json!(record.physical_root));
    details.insert("projected_path".to_string(), json!(record.projected_path));
    details.insert(
        "effective_version_hash".to_string(),
        json!(record.effective_version_hash),
    );
    details
}

fn update_check_details(record: &UpdateCheckRecord, trust: Option<&SkillTrust>) -> HistoryDetails {
    let mut details = BTreeMap::new();
    details.insert("pinned_revision".to_string(), json!(record.pinned_revision));
    if let Some(latest_revision) = &record.latest_revision {
        details.insert("latest_revision".to_string(), json!(latest_revision));
    }
    details.insert("outcome".to_string(), json!(record.outcome.as_str()));
    details.insert(
        "overlay_detected".to_string(),
        json!(record.overlay_detected),
    );
    details.insert(
        "local_modification_detected".to_string(),
        json!(record.local_modification_detected),
    );
    if let Some(reason) = record.apply_blocked_by {
        details.insert("apply_blocked_by".to_string(), json!(reason.as_str()));
    }
    if let Some(notes) = &record.notes {
        details.insert("notes".to_string(), json!(notes));
    }
    if let Some(trust) = trust {
        details.insert("trust".to_string(), json!(trust));
    }
    details
}

fn local_modification_details(record: &LocalModificationRecord) -> HistoryDetails {
    let mut details = BTreeMap::new();
    details.insert("kind".to_string(), json!(record.kind.as_str()));
    if let Some(path) = &record.path {
        details.insert("path".to_string(), json!(path));
    }
    if let Some(detail) = &record.details {
        details.insert("details".to_string(), json!(detail));
    }
    details
}

fn trust_review_details(record: &TrustReviewRecord) -> HistoryDetails {
    let mut details = BTreeMap::new();
    details.insert("decision".to_string(), json!(record.decision.as_str()));
    details.insert(
        "resolved_revision".to_string(),
        json!(record.resolved_revision),
    );
    details.insert(
        "effective_version_hash".to_string(),
        json!(record.effective_version_hash),
    );
    details.insert(
        "contains_scripts".to_string(),
        json!(record.contains_scripts),
    );
    if let Some(note) = &record.note {
        details.insert("note".to_string(), json!(note));
    }
    details
}

fn pin_details(record: &PinRecord) -> HistoryDetails {
    let mut details = BTreeMap::new();
    details.insert(
        "requested_reference".to_string(),
        json!(record.requested_reference),
    );
    details.insert(
        "resolved_revision".to_string(),
        json!(record.resolved_revision),
    );
    if let Some(hash) = &record.effective_version_hash {
        details.insert("effective_version_hash".to_string(), json!(hash));
    }
    details
}

fn rollback_details(record: &RollbackRecord) -> HistoryDetails {
    let mut details = BTreeMap::new();
    details.insert("from_reference".to_string(), json!(record.from_reference));
    details.insert("to_reference".to_string(), json!(record.to_reference));
    details
}

fn path_details(path: &str) -> HistoryDetails {
    let mut details = BTreeMap::new();
    details.insert("path".to_string(), json!(path));
    details
}

fn cleanup_summary(action: &str, skill: Option<&ManagedSkillRef>, path: &str) -> String {
    match skill {
        Some(skill) => format!("{action} {} at {}", skill.skill_id, path),
        None => format!("{action} generated state at {}", path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        adapter::TargetRuntime,
        source::SourceKind,
        state::{ManagedScope, ProjectionMode, UpdateCheckOutcome},
    };

    #[test]
    fn history_ledger_records_install_projection_updates_and_cleanup() {
        let mut store = LocalStateStore::open_in_memory().expect("state store opens");
        let skill = ManagedSkillRef::new(ManagedScope::Workspace, "ai-sdk");

        let install = InstallRecord {
            skill: skill.clone(),
            source_kind: SourceKind::Git,
            source_url: "https://github.com/vercel/ai.git".to_string(),
            source_subpath: "skills/ai-sdk".to_string(),
            resolved_revision: "0123456789abcdef".to_string(),
            upstream_revision: Some("0123456789abcdef".to_string()),
            content_hash: "sha256:content".to_string(),
            overlay_hash: "sha256:overlay".to_string(),
            effective_version_hash: "sha256:v1".to_string(),
            installed_at: "2026-03-19T11:00:00Z".to_string(),
            updated_at: "2026-03-19T11:00:00Z".to_string(),
            detached: false,
            forked: false,
        };
        let projection = ProjectionRecord {
            skill: skill.clone(),
            target: TargetRuntime::ClaudeCode,
            generation_mode: ProjectionMode::Copy,
            physical_root: ".claude/skills".to_string(),
            projected_path: "ai-sdk".to_string(),
            effective_version_hash: "sha256:v1".to_string(),
            generated_at: "2026-03-19T11:01:00Z".to_string(),
        };
        let update_check = UpdateCheckRecord {
            id: None,
            skill: skill.clone(),
            checked_at: "2026-03-19T11:02:00Z".to_string(),
            pinned_revision: "0123456789abcdef".to_string(),
            latest_revision: Some("1111111111111111".to_string()),
            outcome: UpdateCheckOutcome::UpdateAvailable,
            overlay_detected: false,
            local_modification_detected: false,
            apply_blocked_by: None,
            notes: None,
        };
        let updated_install = InstallRecord {
            updated_at: "2026-03-19T11:03:00Z".to_string(),
            resolved_revision: "1111111111111111".to_string(),
            upstream_revision: Some("1111111111111111".to_string()),
            effective_version_hash: "sha256:v2".to_string(),
            ..install.clone()
        };
        let detached_install = InstallRecord {
            updated_at: "2026-03-19T11:04:00Z".to_string(),
            detached: true,
            ..updated_install.clone()
        };

        {
            let mut ledger = HistoryLedger::new(&mut store);
            ledger.record_install(&install).expect("install records");
            ledger
                .record_projection(&projection)
                .expect("projection records");
            ledger
                .record_update_check(&update_check)
                .expect("update check records");
            ledger
                .record_update_applied(&install.resolved_revision, &updated_install)
                .expect("applied update records");
            ledger
                .record_detach(&detached_install, ".agents/skills/ai-sdk")
                .expect("detach records");
            ledger
                .record_cleanup(
                    Some(&skill),
                    ".claude/skills/ai-sdk",
                    "2026-03-19T11:05:00Z",
                )
                .expect("cleanup records");
        }

        let history = store
            .history_entries(&HistoryQuery::for_skill(skill.clone()))
            .expect("history loads");
        let kinds: Vec<HistoryEventKind> = history.iter().map(|entry| entry.kind).collect();
        assert_eq!(
            kinds,
            vec![
                HistoryEventKind::Cleanup,
                HistoryEventKind::Detach,
                HistoryEventKind::UpdateApplied,
                HistoryEventKind::UpdateCheck,
                HistoryEventKind::Projection,
                HistoryEventKind::Install,
            ]
        );
        assert_eq!(
            history[0].summary,
            "Cleaned ai-sdk at .claude/skills/ai-sdk".to_string()
        );

        let current_install = store
            .install_record(&skill)
            .expect("install loads")
            .expect("install exists");
        assert_eq!(current_install.resolved_revision, "1111111111111111");
        assert!(current_install.detached);
        assert_eq!(
            store
                .projection_records(Some(&skill))
                .expect("projection loads")
                .len(),
            1
        );
        assert_eq!(
            store
                .latest_update_check(&skill)
                .expect("update check loads")
                .expect("update check exists")
                .outcome,
            UpdateCheckOutcome::UpdateAvailable
        );
    }
}
