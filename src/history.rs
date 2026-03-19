//! History ledger APIs plus the current command entry points.

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::json;

use crate::{
    app::AppContext,
    error::AppError,
    response::AppResponse,
    state::{
        HistoryDetails, HistoryEntry, HistoryQuery, InstallRecord, LocalModificationRecord,
        LocalStateStore, ManagedSkillRef, PinRecord, ProjectionRecord, RollbackRecord,
        TelemetrySettings, UpdateCheckRecord, insert_history_entry_in,
        insert_local_modification_record_in, insert_rollback_record_in,
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
pub fn handle_pin(_context: &AppContext, _request: PinRequest) -> Result<AppResponse, AppError> {
    Err(AppError::NotYetImplemented { command: "pin" })
}

/// Handle `skillctl rollback`.
pub fn handle_rollback(
    _context: &AppContext,
    _request: RollbackRequest,
) -> Result<AppResponse, AppError> {
    Err(AppError::NotYetImplemented {
        command: "rollback",
    })
}

/// Handle `skillctl history`.
pub fn handle_history(
    _context: &AppContext,
    _request: HistoryRequest,
) -> Result<AppResponse, AppError> {
    Err(AppError::NotYetImplemented { command: "history" })
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
