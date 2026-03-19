//! History ledger APIs plus the current command entry points.

use std::{
    collections::BTreeMap,
    ffi::OsStr,
    path::{Path, PathBuf},
    process::Command,
};

use serde::Serialize;
use serde_json::{Value, json};
use tempfile::TempDir;

use crate::{
    app::AppContext,
    cli::Scope,
    error::AppError,
    lockfile::{LockfileTimestamp, WorkspaceLockfile},
    manifest::{ManifestScope, WorkspaceManifest},
    materialize::{self, MaterializationReport},
    overlay::{NO_OVERLAY_HASH, hash_overlay_root},
    response::AppResponse,
    source::{
        SourceKind, compute_effective_version_hash, copy_source_tree, current_timestamp,
        hash_directory_contents, imports_store_root,
    },
    state::{
        HistoryDetails, HistoryEntry, HistoryQuery, InstallRecord, LocalModificationRecord,
        LocalStateStore, ManagedScope, ManagedSkillRef, PinRecord, ProjectionMode,
        ProjectionRecord, RollbackRecord, TelemetrySettings, UpdateCheckRecord,
        insert_history_entry_in, insert_local_modification_record_in, insert_rollback_record_in,
        insert_update_check_record_in, upsert_install_record_in, upsert_pin_record_in,
        upsert_projection_record_in, upsert_telemetry_settings_in,
    },
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
            details: install_details(record),
        };

        self.store
            .with_transaction("record install", |connection, path| {
                upsert_install_record_in(connection, path, record)?;
                insert_history_entry_in(connection, path, &entry)
            })
    }

    /// Record an upstream update check and append a matching history entry.
    pub fn record_update_check(&mut self, record: &UpdateCheckRecord) -> Result<i64, AppError> {
        let entry = HistoryEntry {
            id: None,
            kind: HistoryEventKind::UpdateCheck,
            scope: Some(record.skill.scope),
            skill_id: Some(record.skill.skill_id.clone()),
            target: None,
            occurred_at: record.checked_at.clone(),
            summary: format!("Checked {} for updates", record.skill.skill_id),
            details: update_check_details(record),
        };

        self.store
            .with_transaction("record update check", |connection, path| {
                insert_update_check_record_in(connection, path, record)?;
                insert_history_entry_in(connection, path, &entry)
            })
    }

    /// Record an applied update by replacing install state and appending history.
    pub fn record_update_applied(
        &mut self,
        previous_revision: &str,
        record: &InstallRecord,
    ) -> Result<i64, AppError> {
        let mut details = install_details(record);
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

        self.store
            .with_transaction("record applied update", |connection, path| {
                upsert_install_record_in(connection, path, record)?;
                insert_history_entry_in(connection, path, &entry)
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

        self.store
            .with_transaction("record projection", |connection, path| {
                upsert_projection_record_in(connection, path, record)?;
                insert_history_entry_in(connection, path, &entry)
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

        self.store
            .with_transaction("record local modification", |connection, path| {
                insert_local_modification_record_in(connection, path, record)?;
                insert_history_entry_in(connection, path, &entry)
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

        self.store
            .with_transaction("record pin", |connection, path| {
                upsert_pin_record_in(connection, path, record)?;
                insert_history_entry_in(connection, path, &entry)
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

        self.store
            .with_transaction("record rollback", |connection, path| {
                insert_rollback_record_in(connection, path, record)?;
                insert_history_entry_in(connection, path, &entry)
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

        let mut details = install_details(record);
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

        self.store
            .with_transaction("record detach", |connection, path| {
                upsert_install_record_in(connection, path, record)?;
                insert_history_entry_in(connection, path, &entry)
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

        let mut details = install_details(record);
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

        self.store
            .with_transaction("record fork", |connection, path| {
                upsert_install_record_in(connection, path, record)?;
                insert_history_entry_in(connection, path, &entry)
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

        self.store
            .with_transaction("record telemetry change", |connection, path| {
                upsert_telemetry_settings_in(connection, path, settings)?;
                insert_history_entry_in(connection, path, &entry)
            })
    }

    /// Query history entries from the underlying store.
    pub fn entries(&self, query: &HistoryQuery) -> Result<Vec<HistoryEntry>, AppError> {
        self.store.history_entries(query)
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
    let mut manifest = WorkspaceManifest::load_from_workspace(&context.working_directory)?;
    let mut lockfile = WorkspaceLockfile::load_from_workspace(&context.working_directory)?;
    let mut store = LocalStateStore::open_default()?;
    let managed_skill = resolve_installed_skill(&store, &request.skill, context.selector.scope)?;
    let install_record =
        store
            .install_record(&managed_skill)?
            .ok_or_else(|| AppError::ResolutionValidation {
                message: format!(
                    "skill '{}' does not have an installed state record",
                    request.skill
                ),
            })?;

    ensure_pin_supported(&install_record)?;

    let import_index =
        managed_import_index(&manifest, &managed_skill.skill_id, managed_skill.scope)?;
    let import_id = manifest.imports[import_index].id.clone();
    let lockfile_entry =
        lockfile
            .imports
            .get_mut(&import_id)
            .ok_or_else(|| AppError::ResolutionValidation {
                message: format!(
                    "managed import '{}' is missing from the lockfile",
                    import_id
                ),
            })?;

    let checked_out = checkout_git_revision(&install_record.source_url, &request.reference)?;
    let skill_root = checked_out
        .root
        .join(lockfile_entry.source.subpath.as_str());
    ensure_directory(&skill_root, "inspect pinned imported skill")?;
    let content_hash = hash_directory_contents(&skill_root)?;
    let overlay_hash = overlay_hash(&manifest, &context.working_directory, &import_id)?;
    let effective_version_hash = compute_effective_version_hash(
        &checked_out.resolved_revision,
        &content_hash,
        &overlay_hash,
    );
    let timestamp = current_timestamp();

    manifest.imports[import_index].ref_spec = checked_out.resolved_revision.clone();
    lockfile_entry.revision.resolved = checked_out.resolved_revision.clone();
    lockfile_entry.revision.upstream = Some(checked_out.resolved_revision.clone());
    lockfile_entry.timestamps.fetched_at = LockfileTimestamp::new(timestamp.clone());
    lockfile_entry.timestamps.last_updated_at = LockfileTimestamp::new(timestamp.clone());
    lockfile_entry.hashes.content = content_hash.clone();
    lockfile_entry.hashes.overlay = overlay_hash.clone();
    lockfile_entry.hashes.effective_version = effective_version_hash.clone();

    manifest.write_to_path()?;
    lockfile.write_to_path()?;

    copy_source_tree(&checked_out.root, &imports_store_root()?.join(&import_id))?;

    let sync_report =
        materialize::sync_workspace(&context_for_scope(context, managed_skill.scope))?;

    let updated_install = InstallRecord {
        resolved_revision: checked_out.resolved_revision.clone(),
        upstream_revision: Some(checked_out.resolved_revision.clone()),
        content_hash,
        overlay_hash,
        effective_version_hash: effective_version_hash.clone(),
        updated_at: timestamp.clone(),
        detached: false,
        forked: false,
        ..install_record
    };
    let pin_record = PinRecord {
        skill: managed_skill.clone(),
        requested_reference: request.reference.clone(),
        resolved_revision: checked_out.resolved_revision.clone(),
        effective_version_hash: Some(effective_version_hash.clone()),
        pinned_at: timestamp.clone(),
    };

    store.upsert_install_record(&updated_install)?;
    let projection_records = projection_records_for_skill(
        &sync_report,
        &managed_skill,
        &effective_version_hash,
        &timestamp,
    );
    let mut ledger = HistoryLedger::new(&mut store);
    ledger.record_pin(&pin_record)?;
    for record in projection_records {
        ledger.record_projection(&record)?;
    }

    Ok(AppResponse::success("pin")
        .with_summary(format!(
            "Pinned {} to {}",
            request.skill, checked_out.resolved_revision
        ))
        .with_data(json!({
            "skill": request.skill,
            "scope": managed_skill.scope.as_str(),
            "requested_reference": request.reference,
            "resolved_revision": checked_out.resolved_revision,
            "effective_version_hash": effective_version_hash,
            "projection": sync_report,
        })))
}

/// Handle `skillctl rollback`.
pub fn handle_rollback(
    context: &AppContext,
    request: RollbackRequest,
) -> Result<AppResponse, AppError> {
    let mut manifest = WorkspaceManifest::load_from_workspace(&context.working_directory)?;
    let mut lockfile = WorkspaceLockfile::load_from_workspace(&context.working_directory)?;
    let mut store = LocalStateStore::open_default()?;
    let managed_skill = resolve_installed_skill(&store, &request.skill, context.selector.scope)?;
    let install_record =
        store
            .install_record(&managed_skill)?
            .ok_or_else(|| AppError::ResolutionValidation {
                message: format!(
                    "skill '{}' does not have an installed state record",
                    request.skill
                ),
            })?;

    ensure_pin_supported(&install_record)?;

    let import_index =
        managed_import_index(&manifest, &managed_skill.skill_id, managed_skill.scope)?;
    let import_id = manifest.imports[import_index].id.clone();
    let lockfile_entry =
        lockfile
            .imports
            .get_mut(&import_id)
            .ok_or_else(|| AppError::ResolutionValidation {
                message: format!(
                    "managed import '{}' is missing from the lockfile",
                    import_id
                ),
            })?;
    let target = resolve_recorded_version(&store, &managed_skill, &install_record, &request)?;

    let checked_out = checkout_git_revision(&install_record.source_url, &target.resolved_revision)?;
    let skill_root = checked_out
        .root
        .join(lockfile_entry.source.subpath.as_str());
    ensure_directory(&skill_root, "inspect rolled back imported skill")?;
    let content_hash = hash_directory_contents(&skill_root)?;
    let overlay_hash = overlay_hash(&manifest, &context.working_directory, &import_id)?;
    let effective_version_hash = compute_effective_version_hash(
        &checked_out.resolved_revision,
        &content_hash,
        &overlay_hash,
    );
    if let Some(expected) = target.effective_version_hash.as_deref()
        && effective_version_hash != expected
    {
        return Err(AppError::ResolutionValidation {
            message: format!(
                "recorded effective version '{}' no longer matches the current overlay state",
                request.version_or_commit
            ),
        });
    }

    let timestamp = current_timestamp();
    manifest.imports[import_index].ref_spec = checked_out.resolved_revision.clone();
    lockfile_entry.revision.resolved = checked_out.resolved_revision.clone();
    lockfile_entry.revision.upstream = Some(checked_out.resolved_revision.clone());
    lockfile_entry.timestamps.fetched_at = LockfileTimestamp::new(timestamp.clone());
    lockfile_entry.timestamps.last_updated_at = LockfileTimestamp::new(timestamp.clone());
    lockfile_entry.hashes.content = content_hash.clone();
    lockfile_entry.hashes.overlay = overlay_hash.clone();
    lockfile_entry.hashes.effective_version = effective_version_hash.clone();

    manifest.write_to_path()?;
    lockfile.write_to_path()?;

    copy_source_tree(&checked_out.root, &imports_store_root()?.join(&import_id))?;

    let sync_report =
        materialize::sync_workspace(&context_for_scope(context, managed_skill.scope))?;

    let updated_install = InstallRecord {
        resolved_revision: checked_out.resolved_revision.clone(),
        upstream_revision: Some(checked_out.resolved_revision.clone()),
        content_hash,
        overlay_hash,
        effective_version_hash: effective_version_hash.clone(),
        updated_at: timestamp.clone(),
        detached: false,
        forked: false,
        ..install_record.clone()
    };
    let pin_record = PinRecord {
        skill: managed_skill.clone(),
        requested_reference: request.version_or_commit.clone(),
        resolved_revision: checked_out.resolved_revision.clone(),
        effective_version_hash: Some(effective_version_hash.clone()),
        pinned_at: timestamp.clone(),
    };
    let rollback_record = RollbackRecord {
        id: None,
        skill: managed_skill.clone(),
        rolled_back_at: timestamp.clone(),
        from_reference: install_record.resolved_revision.clone(),
        to_reference: request.version_or_commit.clone(),
    };

    store.upsert_install_record(&updated_install)?;
    store.upsert_pin_record(&pin_record)?;
    let projection_records = projection_records_for_skill(
        &sync_report,
        &managed_skill,
        &effective_version_hash,
        &timestamp,
    );
    let mut ledger = HistoryLedger::new(&mut store);
    ledger.record_rollback(&rollback_record)?;
    for record in projection_records {
        ledger.record_projection(&record)?;
    }

    Ok(AppResponse::success("rollback")
        .with_summary(format!(
            "Rolled back {} to {}",
            request.skill, checked_out.resolved_revision
        ))
        .with_data(json!({
            "skill": request.skill,
            "scope": managed_skill.scope.as_str(),
            "requested_version": request.version_or_commit,
            "resolved_revision": checked_out.resolved_revision,
            "effective_version_hash": effective_version_hash,
            "projection": sync_report,
        })))
}

/// Handle `skillctl history`.
pub fn handle_history(
    _context: &AppContext,
    _request: HistoryRequest,
) -> Result<AppResponse, AppError> {
    Err(AppError::NotYetImplemented { command: "history" })
}

pub(crate) fn context_for_scope(context: &AppContext, scope: ManagedScope) -> AppContext {
    let mut scoped = context.clone();
    scoped.selector.scope = Some(match scope {
        ManagedScope::Workspace => Scope::Workspace,
        ManagedScope::User => Scope::User,
    });
    scoped
}

pub(crate) fn projection_records_for_skill(
    sync_report: &MaterializationReport,
    skill: &ManagedSkillRef,
    effective_version_hash: &str,
    generated_at: &str,
) -> Vec<ProjectionRecord> {
    let targets_by_root: BTreeMap<_, _> = sync_report
        .plan
        .physical_roots
        .iter()
        .map(|root| (root.path.clone(), root.targets.clone()))
        .collect();
    let mut records = Vec::new();

    for generated_root in &sync_report.generated_roots {
        if !generated_root
            .materialized
            .iter()
            .any(|name| name == &skill.skill_id)
        {
            continue;
        }

        let Some(targets) = targets_by_root.get(&generated_root.path) else {
            continue;
        };

        for target in targets {
            records.push(ProjectionRecord {
                skill: skill.clone(),
                target: *target,
                generation_mode: ProjectionMode::Copy,
                physical_root: generated_root.path.clone(),
                projected_path: skill.skill_id.clone(),
                effective_version_hash: effective_version_hash.to_string(),
                generated_at: generated_at.to_string(),
            });
        }
    }

    records
}

#[derive(Debug)]
struct CheckedOutSource {
    root: PathBuf,
    resolved_revision: String,
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

fn resolve_installed_skill(
    store: &LocalStateStore,
    skill: &str,
    scope: Option<Scope>,
) -> Result<ManagedSkillRef, AppError> {
    let scopes = match scope {
        Some(Scope::Workspace) => vec![ManagedScope::Workspace],
        Some(Scope::User) => vec![ManagedScope::User],
        None => vec![ManagedScope::Workspace, ManagedScope::User],
    };
    let mut matches = Vec::new();

    for managed_scope in scopes {
        let candidate = ManagedSkillRef::new(managed_scope, skill);
        if store.install_record(&candidate)?.is_some() {
            matches.push(candidate);
        }
    }

    match matches.len() {
        1 => Ok(matches.remove(0)),
        0 => Err(AppError::ResolutionValidation {
            message: format!("skill '{}' is not installed", skill),
        }),
        _ => Err(AppError::ResolutionValidation {
            message: format!(
                "skill '{}' exists in multiple scopes; re-run with --scope",
                skill
            ),
        }),
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

fn managed_import_index(
    manifest: &WorkspaceManifest,
    skill: &str,
    scope: ManagedScope,
) -> Result<usize, AppError> {
    let manifest_scope = match scope {
        ManagedScope::Workspace => ManifestScope::Workspace,
        ManagedScope::User => ManifestScope::User,
    };

    manifest
        .imports
        .iter()
        .position(|import| import.id == skill && import.scope == manifest_scope)
        .ok_or_else(|| AppError::ResolutionValidation {
            message: format!(
                "skill '{}' is not a managed import in the workspace manifest",
                skill
            ),
        })
}

fn overlay_hash(
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

fn checkout_git_revision(source_url: &str, reference: &str) -> Result<CheckedOutSource, AppError> {
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
        &checkout_path,
    )?;
    run_git(
        &[
            OsStr::new("checkout"),
            OsStr::new("--quiet"),
            OsStr::new(reference),
        ],
        Some(&checkout_path),
        "checkout pinned git revision",
        &checkout_path,
    )?;
    let resolved_revision = run_git_capture(
        &[OsStr::new("rev-parse"), OsStr::new("HEAD")],
        Some(&checkout_path),
        "resolve pinned git revision",
        &checkout_path,
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
    path: &Path,
) -> Result<(), AppError> {
    run_git_capture(args, current_dir, action, path).map(|_| ())
}

fn run_git_capture(
    args: &[&OsStr],
    current_dir: Option<&Path>,
    action: &'static str,
    path: &Path,
) -> Result<String, AppError> {
    let mut command = Command::new("git");
    command.args(args);
    if let Some(current_dir) = current_dir {
        command.current_dir(current_dir);
    }

    let output = command
        .output()
        .map_err(|source| AppError::FilesystemOperation {
            action,
            path: path.to_path_buf(),
            source,
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
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

fn ensure_directory(path: &Path, action: &'static str) -> Result<(), AppError> {
    let metadata = std::fs::metadata(path).map_err(|source| AppError::FilesystemOperation {
        action,
        path: path.to_path_buf(),
        source,
    })?;
    if metadata.is_dir() {
        Ok(())
    } else {
        Err(AppError::PathConflict {
            path: path.to_path_buf(),
            expected: "directory",
        })
    }
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
            "version '{}' is not recorded for skill '{}'",
            request.version_or_commit, request.skill
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

fn install_details(record: &InstallRecord) -> HistoryDetails {
    let mut details = BTreeMap::new();
    details.insert("scope".to_string(), json!(record.skill.scope.as_str()));
    details.insert("source_kind".to_string(), json!(record_source_kind(record)));
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

fn update_check_details(record: &UpdateCheckRecord) -> HistoryDetails {
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
    if let Some(notes) = &record.notes {
        details.insert("notes".to_string(), json!(notes));
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

fn record_source_kind(record: &InstallRecord) -> &'static str {
    match record.source_kind {
        crate::source::SourceKind::Git => "git",
        crate::source::SourceKind::LocalPath => "local-path",
        crate::source::SourceKind::Archive => "archive",
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
