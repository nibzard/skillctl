//! Shared schema-version policy plus the versioned local SQLite state store.
//!
//! The store in this module is the authoritative local system of record for
//! installs, projections, update checks, local modifications, pins, rollbacks,
//! telemetry consent, and the append-only history ledger.

use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    path::{Path, PathBuf},
};

use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use serde_json::Value;

use crate::{
    adapter::TargetRuntime, error::AppError, history::HistoryEventKind, source::SourceKind,
};

/// Current schema version for `.agents/skillctl.yaml`.
pub const CURRENT_MANIFEST_VERSION: u32 = 1;
/// Current schema version for `.agents/skillctl.lock`.
pub const CURRENT_LOCKFILE_VERSION: u32 = 1;
/// Current schema version for `~/.skillctl/state.db`.
pub const CURRENT_LOCAL_STATE_VERSION: u32 = 1;

/// Default directory created under the home directory for local state.
pub const DEFAULT_LOCAL_STATE_DIR: &str = ".skillctl";
/// Default filename for the SQLite state database.
pub const DEFAULT_LOCAL_STATE_DATABASE_FILE: &str = "state.db";

/// Version policy for workspace manifests.
pub const MANIFEST_SCHEMA_POLICY: SchemaVersionPolicy =
    SchemaVersionPolicy::new(CURRENT_MANIFEST_VERSION, CURRENT_MANIFEST_VERSION);
/// Version policy for workspace lockfiles.
pub const LOCKFILE_SCHEMA_POLICY: SchemaVersionPolicy =
    SchemaVersionPolicy::new(CURRENT_LOCKFILE_VERSION, CURRENT_LOCKFILE_VERSION);
/// Version policy for the local state store.
pub const LOCAL_STATE_SCHEMA_POLICY: SchemaVersionPolicy =
    SchemaVersionPolicy::new(CURRENT_LOCAL_STATE_VERSION, CURRENT_LOCAL_STATE_VERSION);

const REQUIRED_TABLES: &[&str] = &[
    "history_events",
    "install_records",
    "local_modifications",
    "pins",
    "projection_records",
    "rollback_records",
    "telemetry_settings",
    "update_checks",
];

/// Declarative schema version policy for a state-bearing document.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SchemaVersionPolicy {
    current: u32,
    minimum_supported: u32,
}

impl SchemaVersionPolicy {
    /// Create a version policy with a current and minimum supported version.
    pub const fn new(current: u32, minimum_supported: u32) -> Self {
        Self {
            current,
            minimum_supported,
        }
    }

    /// Return the current schema version.
    pub const fn current(self) -> u32 {
        self.current
    }

    /// Return the oldest schema version that remains readable.
    pub const fn minimum_supported(self) -> u32 {
        self.minimum_supported
    }

    /// Classify a discovered schema version against this policy.
    pub const fn classify(self, found: u32) -> VersionDisposition {
        if found == self.current {
            VersionDisposition::Current
        } else if found >= self.minimum_supported && found < self.current {
            VersionDisposition::NeedsMigration {
                from: found,
                to: self.current,
            }
        } else {
            VersionDisposition::Unsupported {
                found,
                minimum_supported: self.minimum_supported,
                current: self.current,
            }
        }
    }
}

/// Outcome of comparing a discovered schema version to a policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VersionDisposition {
    /// The discovered version matches the current version.
    Current,
    /// The discovered version is readable but must be migrated.
    NeedsMigration {
        /// The discovered version on disk.
        from: u32,
        /// The current version the tool writes.
        to: u32,
    },
    /// The discovered version is outside the supported range.
    Unsupported {
        /// The discovered version on disk.
        found: u32,
        /// The oldest readable version.
        minimum_supported: u32,
        /// The current version the tool writes.
        current: u32,
    },
}

/// Managed skill scope recorded in local state.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ManagedScope {
    /// Workspace-local ownership.
    Workspace,
    /// User-wide ownership.
    User,
}

impl ManagedScope {
    /// Return the stable persisted identifier.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Workspace => "workspace",
            Self::User => "user",
        }
    }
}

/// Projection materialization mode recorded for generated runtime copies.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProjectionMode {
    /// Deterministic copy mode.
    Copy,
    /// Opt-in symlink mode.
    Symlink,
}

impl ProjectionMode {
    /// Return the stable persisted identifier.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Copy => "copy",
            Self::Symlink => "symlink",
        }
    }
}

/// Outcome captured for one upstream update check.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum UpdateCheckOutcome {
    /// Installed revision already matches upstream.
    UpToDate,
    /// A newer upstream revision is available.
    UpdateAvailable,
    /// Updating is blocked by local drift.
    Blocked,
    /// The skill has been detached from upstream lifecycle management.
    Detached,
    /// The source is local-only and does not support upstream checks.
    LocalSource,
    /// An upstream check failed.
    Failed,
}

impl UpdateCheckOutcome {
    /// Return the stable persisted identifier.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UpToDate => "up-to-date",
            Self::UpdateAvailable => "update-available",
            Self::Blocked => "blocked",
            Self::Detached => "detached",
            Self::LocalSource => "local-source",
            Self::Failed => "failed",
        }
    }
}

/// Local modification class captured during drift detection.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum LocalModificationKind {
    /// Managed overlay content changed.
    Overlay,
    /// A projected runtime copy was edited directly.
    ProjectedCopy,
    /// The skill was detached or forked from upstream.
    DetachedFork,
}

impl LocalModificationKind {
    /// Return the stable persisted identifier.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Overlay => "overlay",
            Self::ProjectedCopy => "projected-copy",
            Self::DetachedFork => "detached-fork",
        }
    }
}

/// Telemetry consent persisted independently from local history.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum TelemetryConsent {
    /// No first-run decision has been persisted yet.
    Unknown,
    /// Telemetry is permitted.
    Enabled,
    /// Telemetry is explicitly disabled.
    Disabled,
}

impl TelemetryConsent {
    /// Return the stable persisted identifier.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Enabled => "enabled",
            Self::Disabled => "disabled",
        }
    }
}

/// Strongly typed reference to a managed skill in one scope.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct ManagedSkillRef {
    /// Owning scope for the managed skill.
    pub scope: ManagedScope,
    /// Stable skill identifier.
    pub skill_id: String,
}

impl ManagedSkillRef {
    /// Construct a managed skill reference.
    pub fn new(scope: ManagedScope, skill_id: impl Into<String>) -> Self {
        Self {
            scope,
            skill_id: skill_id.into(),
        }
    }
}

/// Current install state for one managed skill.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct InstallRecord {
    /// Managed skill identity.
    pub skill: ManagedSkillRef,
    /// Source category pinned for this install.
    pub source_kind: SourceKind,
    /// Normalized source URL or file URL.
    pub source_url: String,
    /// Selected relative subpath inside the source.
    pub source_subpath: String,
    /// Exact installed revision or digest.
    pub resolved_revision: String,
    /// Last observed upstream revision.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream_revision: Option<String>,
    /// Source content hash.
    pub content_hash: String,
    /// Overlay content hash.
    pub overlay_hash: String,
    /// Effective version hash used for rollback and explain flows.
    pub effective_version_hash: String,
    /// First install timestamp.
    pub installed_at: String,
    /// Most recent update timestamp.
    pub updated_at: String,
    /// Whether the install has been detached from upstream lifecycle management.
    pub detached: bool,
    /// Whether the install has been forked into local canonical ownership.
    pub forked: bool,
}

/// Current projection state for one managed skill and runtime target.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ProjectionRecord {
    /// Managed skill identity.
    pub skill: ManagedSkillRef,
    /// Runtime target that received the projection.
    pub target: TargetRuntime,
    /// Projection materialization mode.
    pub generation_mode: ProjectionMode,
    /// Physical root used for the projection.
    pub physical_root: String,
    /// Relative projected path under the physical root.
    pub projected_path: String,
    /// Effective version hash materialized into the runtime root.
    pub effective_version_hash: String,
    /// Projection timestamp.
    pub generated_at: String,
}

/// One recorded upstream update check.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct UpdateCheckRecord {
    /// Row identifier when loaded back from SQLite.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<i64>,
    /// Managed skill identity.
    pub skill: ManagedSkillRef,
    /// Timestamp for the check.
    pub checked_at: String,
    /// Currently pinned revision that was evaluated.
    pub pinned_revision: String,
    /// Latest observed upstream revision, when one exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest_revision: Option<String>,
    /// Planner-relevant outcome of the check.
    pub outcome: UpdateCheckOutcome,
    /// Whether an overlay existed during the check.
    pub overlay_detected: bool,
    /// Whether unmanaged local changes were detected during the check.
    pub local_modification_detected: bool,
    /// Optional explanatory note captured with the check.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

/// One detected local modification relevant to lifecycle decisions.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct LocalModificationRecord {
    /// Row identifier when loaded back from SQLite.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<i64>,
    /// Managed skill identity.
    pub skill: ManagedSkillRef,
    /// Detection timestamp.
    pub detected_at: String,
    /// Classification of the detected local change.
    pub kind: LocalModificationKind,
    /// Optional affected path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Optional explanatory detail.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<String>,
}

/// Current active pin for one managed skill.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PinRecord {
    /// Managed skill identity.
    pub skill: ManagedSkillRef,
    /// User-requested pin reference.
    pub requested_reference: String,
    /// Resolved immutable revision the pin currently points to.
    pub resolved_revision: String,
    /// Effective version hash active at the time of pinning.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effective_version_hash: Option<String>,
    /// Timestamp when the pin was recorded.
    pub pinned_at: String,
}

/// One rollback transition recorded in local history.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RollbackRecord {
    /// Row identifier when loaded back from SQLite.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<i64>,
    /// Managed skill identity.
    pub skill: ManagedSkillRef,
    /// Rollback timestamp.
    pub rolled_back_at: String,
    /// Previously active reference or effective version.
    pub from_reference: String,
    /// Newly activated reference or effective version.
    pub to_reference: String,
}

/// Persisted telemetry consent and notice state.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct TelemetrySettings {
    /// Current consent state.
    pub consent: TelemetryConsent,
    /// Timestamp when the first-run notice was acknowledged.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notice_seen_at: Option<String>,
    /// Timestamp when the settings row was last updated.
    pub updated_at: String,
}

/// Additional structured detail attached to one history entry.
pub type HistoryDetails = BTreeMap<String, Value>;

/// One append-only history event in the local ledger.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct HistoryEntry {
    /// Row identifier when loaded back from SQLite.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<i64>,
    /// Stable event kind.
    pub kind: HistoryEventKind,
    /// Owning scope when the event is skill-specific.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<ManagedScope>,
    /// Managed skill identifier when the event is skill-specific.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skill_id: Option<String>,
    /// Related runtime target when the event is target-specific.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<TargetRuntime>,
    /// Event timestamp.
    pub occurred_at: String,
    /// Human-facing deterministic summary.
    pub summary: String,
    /// Structured event details for future explain, history, and TUI flows.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub details: HistoryDetails,
}

/// Query options for the append-only history ledger.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct HistoryQuery {
    /// Optional skill filter.
    pub skill: Option<ManagedSkillRef>,
    /// Optional maximum number of entries to return.
    pub limit: Option<usize>,
}

impl HistoryQuery {
    /// Create a query filtered to one managed skill.
    pub fn for_skill(skill: ManagedSkillRef) -> Self {
        Self {
            skill: Some(skill),
            limit: None,
        }
    }
}

/// Aggregated current view used by explain, update planning, and TUI reads.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SkillStateSnapshot {
    /// Managed skill identity.
    pub skill: ManagedSkillRef,
    /// Current install state.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub install: Option<InstallRecord>,
    /// Current pin state.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pin: Option<PinRecord>,
    /// All active projection records for the skill.
    pub projections: Vec<ProjectionRecord>,
    /// Latest update check, when one exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest_update_check: Option<UpdateCheckRecord>,
    /// Known local modifications ordered newest first.
    pub local_modifications: Vec<LocalModificationRecord>,
    /// Recorded rollbacks ordered newest first.
    pub rollbacks: Vec<RollbackRecord>,
}

/// SQLite-backed local system of record for skill lifecycle state.
pub struct LocalStateStore {
    path: PathBuf,
    connection: Connection,
}

impl std::fmt::Debug for LocalStateStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LocalStateStore")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl LocalStateStore {
    /// Open or create the default `~/.skillctl/state.db` database.
    pub fn open_default() -> Result<Self, AppError> {
        Self::open_at(default_state_database_path()?)
    }

    /// Open or create a store at an explicit database path.
    pub fn open_at(path: impl Into<PathBuf>) -> Result<Self, AppError> {
        let path = path.into();

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| AppError::FilesystemOperation {
                action: "create local state parent directory",
                path: parent.to_path_buf(),
                source,
            })?;
        }

        let connection = Connection::open(&path).map_err(|source| AppError::LocalStateOpen {
            path: path.clone(),
            source,
        })?;
        bootstrap_connection(&connection, &path)?;

        Ok(Self { path, connection })
    }

    /// Open an in-memory store. Intended for tests and pure logic callers.
    pub fn open_in_memory() -> Result<Self, AppError> {
        let path = PathBuf::from(":memory:");
        let connection =
            Connection::open_in_memory().map_err(|source| AppError::LocalStateOpen {
                path: path.clone(),
                source,
            })?;
        bootstrap_connection(&connection, &path)?;

        Ok(Self { path, connection })
    }

    /// Return the path to the backing SQLite database.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Return the current schema version persisted in the database.
    pub fn schema_version(&self) -> Result<u32, AppError> {
        schema_version(&self.connection, &self.path)
    }

    /// List every current install record in stable scope and skill order.
    pub fn list_install_records(&self) -> Result<Vec<InstallRecord>, AppError> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT scope, skill_id, source_kind, source_url, source_subpath, \
                 resolved_revision, upstream_revision, content_hash, overlay_hash, \
                 effective_version_hash, installed_at, updated_at, detached, forked \
                 FROM install_records ORDER BY scope, skill_id",
            )
            .map_err(|source| {
                local_state_query(&self.path, "prepare install records query", source)
            })?;

        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, String>(8)?,
                    row.get::<_, String>(9)?,
                    row.get::<_, String>(10)?,
                    row.get::<_, String>(11)?,
                    row.get::<_, i64>(12)?,
                    row.get::<_, i64>(13)?,
                ))
            })
            .map_err(|source| local_state_query(&self.path, "query install records", source))?;

        let mut records = Vec::new();
        for row in rows {
            let row =
                row.map_err(|source| local_state_query(&self.path, "read install record", source))?;
            records.push(decode_install_record(&self.path, row)?);
        }

        Ok(records)
    }

    /// Load the current install record for one managed skill.
    pub fn install_record(
        &self,
        skill: &ManagedSkillRef,
    ) -> Result<Option<InstallRecord>, AppError> {
        validate_skill_ref(&self.path, skill)?;

        let mut statement = self
            .connection
            .prepare(
                "SELECT scope, skill_id, source_kind, source_url, source_subpath, \
                 resolved_revision, upstream_revision, content_hash, overlay_hash, \
                 effective_version_hash, installed_at, updated_at, detached, forked \
                 FROM install_records WHERE scope = ?1 AND skill_id = ?2",
            )
            .map_err(|source| {
                local_state_query(&self.path, "prepare install record query", source)
            })?;

        let row = statement
            .query_row(params![skill.scope.as_str(), skill.skill_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, String>(8)?,
                    row.get::<_, String>(9)?,
                    row.get::<_, String>(10)?,
                    row.get::<_, String>(11)?,
                    row.get::<_, i64>(12)?,
                    row.get::<_, i64>(13)?,
                ))
            })
            .optional()
            .map_err(|source| local_state_query(&self.path, "load install record", source))?;

        row.map(|record| decode_install_record(&self.path, record))
            .transpose()
    }

    /// Insert or update the current install record for a managed skill.
    pub fn upsert_install_record(&mut self, record: &InstallRecord) -> Result<(), AppError> {
        upsert_install_record_in(&self.connection, &self.path, record)
    }

    /// List projection records, optionally filtered to one managed skill.
    pub fn projection_records(
        &self,
        skill: Option<&ManagedSkillRef>,
    ) -> Result<Vec<ProjectionRecord>, AppError> {
        let sql_all = concat!(
            "SELECT scope, skill_id, target, generation_mode, physical_root, projected_path, ",
            "effective_version_hash, generated_at FROM projection_records ",
            "ORDER BY scope, skill_id, target, physical_root"
        );
        let sql_filtered = concat!(
            "SELECT scope, skill_id, target, generation_mode, physical_root, projected_path, ",
            "effective_version_hash, generated_at FROM projection_records ",
            "WHERE scope = ?1 AND skill_id = ?2 ORDER BY target, physical_root"
        );

        let mut statement = self
            .connection
            .prepare(if skill.is_some() {
                sql_filtered
            } else {
                sql_all
            })
            .map_err(|source| {
                local_state_query(&self.path, "prepare projection records query", source)
            })?;

        let mut records = Vec::new();
        if let Some(skill) = skill {
            validate_skill_ref(&self.path, skill)?;
            let rows = statement
                .query_map(params![skill.scope.as_str(), skill.skill_id], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, String>(6)?,
                        row.get::<_, String>(7)?,
                    ))
                })
                .map_err(|source| {
                    local_state_query(&self.path, "query projection records", source)
                })?;

            for row in rows {
                let row = row.map_err(|source| {
                    local_state_query(&self.path, "read projection record", source)
                })?;
                records.push(decode_projection_record(&self.path, row)?);
            }
        } else {
            let rows = statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, String>(6)?,
                        row.get::<_, String>(7)?,
                    ))
                })
                .map_err(|source| {
                    local_state_query(&self.path, "query projection records", source)
                })?;

            for row in rows {
                let row = row.map_err(|source| {
                    local_state_query(&self.path, "read projection record", source)
                })?;
                records.push(decode_projection_record(&self.path, row)?);
            }
        }

        Ok(records)
    }

    /// Insert or update the current projection record for a managed skill.
    pub fn upsert_projection_record(&mut self, record: &ProjectionRecord) -> Result<(), AppError> {
        upsert_projection_record_in(&self.connection, &self.path, record)
    }

    /// Replace the current projection records for one scope atomically.
    pub fn replace_projection_records_for_scope(
        &mut self,
        scope: ManagedScope,
        records: &[ProjectionRecord],
    ) -> Result<(), AppError> {
        self.with_transaction("replace projection records", |connection, path| {
            connection
                .execute(
                    "DELETE FROM projection_records WHERE scope = ?1",
                    params![scope.as_str()],
                )
                .map_err(|source| {
                    local_state_query(path, "delete projection records for scope", source)
                })?;

            for record in records {
                if record.skill.scope != scope {
                    return Err(AppError::LocalStateValidation {
                        path: path.to_path_buf(),
                        message: format!(
                            "projection record for '{}' used scope '{}' during a '{}' replacement",
                            record.skill.skill_id,
                            record.skill.scope.as_str(),
                            scope.as_str()
                        ),
                    });
                }
                upsert_projection_record_in(connection, path, record)?;
            }

            Ok(())
        })
    }

    /// List update checks ordered newest first, optionally filtered to one skill.
    pub fn update_checks(
        &self,
        skill: Option<&ManagedSkillRef>,
    ) -> Result<Vec<UpdateCheckRecord>, AppError> {
        let sql_all = concat!(
            "SELECT id, scope, skill_id, checked_at, pinned_revision, latest_revision, outcome, ",
            "overlay_detected, local_modification_detected, notes FROM update_checks ",
            "ORDER BY checked_at DESC, id DESC"
        );
        let sql_filtered = concat!(
            "SELECT id, scope, skill_id, checked_at, pinned_revision, latest_revision, outcome, ",
            "overlay_detected, local_modification_detected, notes FROM update_checks ",
            "WHERE scope = ?1 AND skill_id = ?2 ORDER BY checked_at DESC, id DESC"
        );

        let mut statement = self
            .connection
            .prepare(if skill.is_some() {
                sql_filtered
            } else {
                sql_all
            })
            .map_err(|source| {
                local_state_query(&self.path, "prepare update checks query", source)
            })?;

        let mut records = Vec::new();
        if let Some(skill) = skill {
            validate_skill_ref(&self.path, skill)?;
            let rows = statement
                .query_map(params![skill.scope.as_str(), skill.skill_id], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, Option<String>>(5)?,
                        row.get::<_, String>(6)?,
                        row.get::<_, i64>(7)?,
                        row.get::<_, i64>(8)?,
                        row.get::<_, Option<String>>(9)?,
                    ))
                })
                .map_err(|source| local_state_query(&self.path, "query update checks", source))?;

            for row in rows {
                let row = row
                    .map_err(|source| local_state_query(&self.path, "read update check", source))?;
                records.push(decode_update_check_record(&self.path, row)?);
            }
        } else {
            let rows = statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, Option<String>>(5)?,
                        row.get::<_, String>(6)?,
                        row.get::<_, i64>(7)?,
                        row.get::<_, i64>(8)?,
                        row.get::<_, Option<String>>(9)?,
                    ))
                })
                .map_err(|source| local_state_query(&self.path, "query update checks", source))?;

            for row in rows {
                let row = row
                    .map_err(|source| local_state_query(&self.path, "read update check", source))?;
                records.push(decode_update_check_record(&self.path, row)?);
            }
        }

        Ok(records)
    }

    /// Load the most recent update check for one managed skill.
    pub fn latest_update_check(
        &self,
        skill: &ManagedSkillRef,
    ) -> Result<Option<UpdateCheckRecord>, AppError> {
        validate_skill_ref(&self.path, skill)?;

        let mut statement = self
            .connection
            .prepare(
                "SELECT id, scope, skill_id, checked_at, pinned_revision, latest_revision, \
                 outcome, overlay_detected, local_modification_detected, notes \
                 FROM update_checks WHERE scope = ?1 AND skill_id = ?2 \
                 ORDER BY checked_at DESC, id DESC LIMIT 1",
            )
            .map_err(|source| {
                local_state_query(&self.path, "prepare latest update check query", source)
            })?;

        let row = statement
            .query_row(params![skill.scope.as_str(), skill.skill_id], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, i64>(7)?,
                    row.get::<_, i64>(8)?,
                    row.get::<_, Option<String>>(9)?,
                ))
            })
            .optional()
            .map_err(|source| local_state_query(&self.path, "load latest update check", source))?;

        row.map(|record| decode_update_check_record(&self.path, record))
            .transpose()
    }

    /// Insert one immutable update check row.
    pub fn record_update_check(&mut self, record: &UpdateCheckRecord) -> Result<i64, AppError> {
        insert_update_check_record_in(&self.connection, &self.path, record)
    }

    /// List local modification records ordered newest first.
    pub fn local_modifications(
        &self,
        skill: Option<&ManagedSkillRef>,
    ) -> Result<Vec<LocalModificationRecord>, AppError> {
        let sql_all = concat!(
            "SELECT id, scope, skill_id, detected_at, kind, path, details ",
            "FROM local_modifications ORDER BY detected_at DESC, id DESC"
        );
        let sql_filtered = concat!(
            "SELECT id, scope, skill_id, detected_at, kind, path, details ",
            "FROM local_modifications WHERE scope = ?1 AND skill_id = ?2 ",
            "ORDER BY detected_at DESC, id DESC"
        );

        let mut statement = self
            .connection
            .prepare(if skill.is_some() {
                sql_filtered
            } else {
                sql_all
            })
            .map_err(|source| {
                local_state_query(&self.path, "prepare local modifications query", source)
            })?;

        let mut records = Vec::new();
        if let Some(skill) = skill {
            validate_skill_ref(&self.path, skill)?;
            let rows = statement
                .query_map(params![skill.scope.as_str(), skill.skill_id], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, Option<String>>(5)?,
                        row.get::<_, Option<String>>(6)?,
                    ))
                })
                .map_err(|source| {
                    local_state_query(&self.path, "query local modifications", source)
                })?;

            for row in rows {
                let row = row.map_err(|source| {
                    local_state_query(&self.path, "read local modification", source)
                })?;
                records.push(decode_local_modification_record(&self.path, row)?);
            }
        } else {
            let rows = statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, Option<String>>(5)?,
                        row.get::<_, Option<String>>(6)?,
                    ))
                })
                .map_err(|source| {
                    local_state_query(&self.path, "query local modifications", source)
                })?;

            for row in rows {
                let row = row.map_err(|source| {
                    local_state_query(&self.path, "read local modification", source)
                })?;
                records.push(decode_local_modification_record(&self.path, row)?);
            }
        }

        Ok(records)
    }

    /// Insert one immutable local modification record.
    pub fn record_local_modification(
        &mut self,
        record: &LocalModificationRecord,
    ) -> Result<i64, AppError> {
        insert_local_modification_record_in(&self.connection, &self.path, record)
    }

    /// Load the current pin record for one managed skill.
    pub fn pin_record(&self, skill: &ManagedSkillRef) -> Result<Option<PinRecord>, AppError> {
        validate_skill_ref(&self.path, skill)?;

        let mut statement = self
            .connection
            .prepare(
                "SELECT scope, skill_id, requested_reference, resolved_revision, \
                 effective_version_hash, pinned_at FROM pins WHERE scope = ?1 AND skill_id = ?2",
            )
            .map_err(|source| local_state_query(&self.path, "prepare pin query", source))?;

        let row = statement
            .query_row(params![skill.scope.as_str(), skill.skill_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, String>(5)?,
                ))
            })
            .optional()
            .map_err(|source| local_state_query(&self.path, "load pin record", source))?;

        row.map(|record| decode_pin_record(&self.path, record))
            .transpose()
    }

    /// Insert or update the current pin for a managed skill.
    pub fn upsert_pin_record(&mut self, record: &PinRecord) -> Result<(), AppError> {
        upsert_pin_record_in(&self.connection, &self.path, record)
    }

    /// Delete the mutable install, pin, and projection rows for one managed skill.
    pub fn delete_current_skill_state(&mut self, skill: &ManagedSkillRef) -> Result<(), AppError> {
        validate_skill_ref(&self.path, skill)?;

        self.with_transaction("delete current skill state", |connection, path| {
            connection
                .execute(
                    "DELETE FROM projection_records WHERE scope = ?1 AND skill_id = ?2",
                    params![skill.scope.as_str(), skill.skill_id],
                )
                .map_err(|source| {
                    local_state_query(path, "delete projection records for skill", source)
                })?;
            connection
                .execute(
                    "DELETE FROM pins WHERE scope = ?1 AND skill_id = ?2",
                    params![skill.scope.as_str(), skill.skill_id],
                )
                .map_err(|source| local_state_query(path, "delete pin record", source))?;
            connection
                .execute(
                    "DELETE FROM install_records WHERE scope = ?1 AND skill_id = ?2",
                    params![skill.scope.as_str(), skill.skill_id],
                )
                .map_err(|source| local_state_query(path, "delete install record", source))?;

            Ok(())
        })
    }

    /// List rollback records ordered newest first.
    pub fn rollback_records(
        &self,
        skill: Option<&ManagedSkillRef>,
    ) -> Result<Vec<RollbackRecord>, AppError> {
        let sql_all = concat!(
            "SELECT id, scope, skill_id, rolled_back_at, from_reference, to_reference ",
            "FROM rollback_records ORDER BY rolled_back_at DESC, id DESC"
        );
        let sql_filtered = concat!(
            "SELECT id, scope, skill_id, rolled_back_at, from_reference, to_reference ",
            "FROM rollback_records WHERE scope = ?1 AND skill_id = ?2 ",
            "ORDER BY rolled_back_at DESC, id DESC"
        );

        let mut statement = self
            .connection
            .prepare(if skill.is_some() {
                sql_filtered
            } else {
                sql_all
            })
            .map_err(|source| {
                local_state_query(&self.path, "prepare rollback records query", source)
            })?;

        let mut records = Vec::new();
        if let Some(skill) = skill {
            validate_skill_ref(&self.path, skill)?;
            let rows = statement
                .query_map(params![skill.scope.as_str(), skill.skill_id], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                    ))
                })
                .map_err(|source| {
                    local_state_query(&self.path, "query rollback records", source)
                })?;

            for row in rows {
                let row = row.map_err(|source| {
                    local_state_query(&self.path, "read rollback record", source)
                })?;
                records.push(decode_rollback_record(&self.path, row)?);
            }
        } else {
            let rows = statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                    ))
                })
                .map_err(|source| {
                    local_state_query(&self.path, "query rollback records", source)
                })?;

            for row in rows {
                let row = row.map_err(|source| {
                    local_state_query(&self.path, "read rollback record", source)
                })?;
                records.push(decode_rollback_record(&self.path, row)?);
            }
        }

        Ok(records)
    }

    /// Insert one immutable rollback record.
    pub fn record_rollback(&mut self, record: &RollbackRecord) -> Result<i64, AppError> {
        insert_rollback_record_in(&self.connection, &self.path, record)
    }

    /// Load the current telemetry settings row, if one has been persisted.
    pub fn telemetry_settings(&self) -> Result<Option<TelemetrySettings>, AppError> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT consent, notice_seen_at, updated_at FROM telemetry_settings \
                 WHERE singleton_id = 1",
            )
            .map_err(|source| {
                local_state_query(&self.path, "prepare telemetry settings query", source)
            })?;

        let row = statement
            .query_row([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .optional()
            .map_err(|source| local_state_query(&self.path, "load telemetry settings", source))?;

        row.map(|record| decode_telemetry_settings(&self.path, record))
            .transpose()
    }

    /// Insert or update the singleton telemetry settings row.
    pub fn upsert_telemetry_settings(
        &mut self,
        settings: &TelemetrySettings,
    ) -> Result<(), AppError> {
        upsert_telemetry_settings_in(&self.connection, &self.path, settings)
    }

    /// Append one immutable history entry to the local ledger.
    pub fn append_history_entry(&mut self, entry: &HistoryEntry) -> Result<i64, AppError> {
        insert_history_entry_in(&self.connection, &self.path, entry)
    }

    /// Remove every current projection record across all scopes.
    pub fn clear_projection_records(&mut self) -> Result<(), AppError> {
        self.connection
            .execute("DELETE FROM projection_records", [])
            .map_err(|source| {
                local_state_query(&self.path, "delete all projection records", source)
            })?;
        Ok(())
    }

    /// Query history entries in deterministic newest-first order.
    pub fn history_entries(&self, query: &HistoryQuery) -> Result<Vec<HistoryEntry>, AppError> {
        let sql_all = concat!(
            "SELECT id, kind, scope, skill_id, target, occurred_at, summary, details_json ",
            "FROM history_events ORDER BY occurred_at DESC, id DESC"
        );
        let sql_filtered = concat!(
            "SELECT id, kind, scope, skill_id, target, occurred_at, summary, details_json ",
            "FROM history_events WHERE scope = ?1 AND skill_id = ?2 ",
            "ORDER BY occurred_at DESC, id DESC"
        );

        let mut statement = self
            .connection
            .prepare(if query.skill.is_some() {
                sql_filtered
            } else {
                sql_all
            })
            .map_err(|source| local_state_query(&self.path, "prepare history query", source))?;

        let mut entries = Vec::new();
        if let Some(skill) = &query.skill {
            validate_skill_ref(&self.path, skill)?;
            let rows = statement
                .query_map(params![skill.scope.as_str(), skill.skill_id], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, String>(6)?,
                        row.get::<_, Option<String>>(7)?,
                    ))
                })
                .map_err(|source| local_state_query(&self.path, "query history entries", source))?;

            for row in rows {
                let row = row.map_err(|source| {
                    local_state_query(&self.path, "read history entry", source)
                })?;
                entries.push(decode_history_entry(&self.path, row)?);
            }
        } else {
            let rows = statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, String>(6)?,
                        row.get::<_, Option<String>>(7)?,
                    ))
                })
                .map_err(|source| local_state_query(&self.path, "query history entries", source))?;

            for row in rows {
                let row = row.map_err(|source| {
                    local_state_query(&self.path, "read history entry", source)
                })?;
                entries.push(decode_history_entry(&self.path, row)?);
            }
        }

        if let Some(limit) = query.limit {
            entries.truncate(limit);
        }

        Ok(entries)
    }

    /// Build an aggregated current-state snapshot for one managed skill.
    pub fn skill_snapshot(&self, skill: &ManagedSkillRef) -> Result<SkillStateSnapshot, AppError> {
        validate_skill_ref(&self.path, skill)?;

        Ok(SkillStateSnapshot {
            skill: skill.clone(),
            install: self.install_record(skill)?,
            pin: self.pin_record(skill)?,
            projections: self.projection_records(Some(skill))?,
            latest_update_check: self.latest_update_check(skill)?,
            local_modifications: self.local_modifications(Some(skill))?,
            rollbacks: self.rollback_records(Some(skill))?,
        })
    }

    /// Execute a multi-write operation in one SQLite transaction.
    pub(crate) fn with_transaction<T>(
        &mut self,
        operation: &'static str,
        work: impl FnOnce(&Connection, &Path) -> Result<T, AppError>,
    ) -> Result<T, AppError> {
        let transaction = self
            .connection
            .transaction()
            .map_err(|source| local_state_query(&self.path, operation, source))?;
        let result = work(&transaction, &self.path)?;
        transaction
            .commit()
            .map_err(|source| local_state_query(&self.path, operation, source))?;
        Ok(result)
    }
}

/// Resolve the default local state root under the current home directory.
pub fn default_state_root() -> Result<PathBuf, AppError> {
    resolve_home_directory()
        .map(|home| home.join(DEFAULT_LOCAL_STATE_DIR))
        .ok_or(AppError::HomeDirectoryUnavailable)
}

/// Resolve the default `~/.skillctl/state.db` path.
pub fn default_state_database_path() -> Result<PathBuf, AppError> {
    Ok(default_state_root()?.join(DEFAULT_LOCAL_STATE_DATABASE_FILE))
}

pub(crate) fn upsert_install_record_in(
    connection: &Connection,
    path: &Path,
    record: &InstallRecord,
) -> Result<(), AppError> {
    validate_install_record(path, record)?;
    connection
        .execute(
            "INSERT INTO install_records (scope, skill_id, source_kind, source_url, \
             source_subpath, resolved_revision, upstream_revision, content_hash, overlay_hash, \
             effective_version_hash, installed_at, updated_at, detached, forked) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14) \
             ON CONFLICT(scope, skill_id) DO UPDATE SET \
             source_kind = excluded.source_kind, \
             source_url = excluded.source_url, \
             source_subpath = excluded.source_subpath, \
             resolved_revision = excluded.resolved_revision, \
             upstream_revision = excluded.upstream_revision, \
             content_hash = excluded.content_hash, \
             overlay_hash = excluded.overlay_hash, \
             effective_version_hash = excluded.effective_version_hash, \
             installed_at = excluded.installed_at, \
             updated_at = excluded.updated_at, \
             detached = excluded.detached, \
             forked = excluded.forked",
            params![
                record.skill.scope.as_str(),
                record.skill.skill_id,
                source_kind_as_str(record.source_kind),
                record.source_url,
                record.source_subpath,
                record.resolved_revision,
                record.upstream_revision,
                record.content_hash,
                record.overlay_hash,
                record.effective_version_hash,
                record.installed_at,
                record.updated_at,
                bool_to_int(record.detached),
                bool_to_int(record.forked),
            ],
        )
        .map_err(|source| local_state_query(path, "write install record", source))?;

    Ok(())
}

pub(crate) fn upsert_projection_record_in(
    connection: &Connection,
    path: &Path,
    record: &ProjectionRecord,
) -> Result<(), AppError> {
    validate_projection_record(path, record)?;
    connection
        .execute(
            "INSERT INTO projection_records (scope, skill_id, target, generation_mode, \
             physical_root, projected_path, effective_version_hash, generated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
             ON CONFLICT(scope, skill_id, target, physical_root) DO UPDATE SET \
             generation_mode = excluded.generation_mode, \
             projected_path = excluded.projected_path, \
             effective_version_hash = excluded.effective_version_hash, \
             generated_at = excluded.generated_at",
            params![
                record.skill.scope.as_str(),
                record.skill.skill_id,
                record.target.as_str(),
                record.generation_mode.as_str(),
                record.physical_root,
                record.projected_path,
                record.effective_version_hash,
                record.generated_at,
            ],
        )
        .map_err(|source| local_state_query(path, "write projection record", source))?;

    Ok(())
}

pub(crate) fn insert_update_check_record_in(
    connection: &Connection,
    path: &Path,
    record: &UpdateCheckRecord,
) -> Result<i64, AppError> {
    validate_update_check_record(path, record)?;
    connection
        .execute(
            "INSERT INTO update_checks (scope, skill_id, checked_at, pinned_revision, \
             latest_revision, outcome, overlay_detected, local_modification_detected, notes) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                record.skill.scope.as_str(),
                record.skill.skill_id,
                record.checked_at,
                record.pinned_revision,
                record.latest_revision,
                record.outcome.as_str(),
                bool_to_int(record.overlay_detected),
                bool_to_int(record.local_modification_detected),
                record.notes,
            ],
        )
        .map_err(|source| local_state_query(path, "write update check record", source))?;

    Ok(connection.last_insert_rowid())
}

pub(crate) fn insert_local_modification_record_in(
    connection: &Connection,
    path: &Path,
    record: &LocalModificationRecord,
) -> Result<i64, AppError> {
    validate_local_modification_record(path, record)?;
    connection
        .execute(
            "INSERT INTO local_modifications (scope, skill_id, detected_at, kind, path, details) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                record.skill.scope.as_str(),
                record.skill.skill_id,
                record.detected_at,
                record.kind.as_str(),
                record.path,
                record.details,
            ],
        )
        .map_err(|source| local_state_query(path, "write local modification record", source))?;

    Ok(connection.last_insert_rowid())
}

pub(crate) fn upsert_pin_record_in(
    connection: &Connection,
    path: &Path,
    record: &PinRecord,
) -> Result<(), AppError> {
    validate_pin_record(path, record)?;
    connection
        .execute(
            "INSERT INTO pins (scope, skill_id, requested_reference, resolved_revision, \
             effective_version_hash, pinned_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
             ON CONFLICT(scope, skill_id) DO UPDATE SET \
             requested_reference = excluded.requested_reference, \
             resolved_revision = excluded.resolved_revision, \
             effective_version_hash = excluded.effective_version_hash, \
             pinned_at = excluded.pinned_at",
            params![
                record.skill.scope.as_str(),
                record.skill.skill_id,
                record.requested_reference,
                record.resolved_revision,
                record.effective_version_hash,
                record.pinned_at,
            ],
        )
        .map_err(|source| local_state_query(path, "write pin record", source))?;

    Ok(())
}

pub(crate) fn insert_rollback_record_in(
    connection: &Connection,
    path: &Path,
    record: &RollbackRecord,
) -> Result<i64, AppError> {
    validate_rollback_record(path, record)?;
    connection
        .execute(
            "INSERT INTO rollback_records (scope, skill_id, rolled_back_at, from_reference, \
             to_reference) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                record.skill.scope.as_str(),
                record.skill.skill_id,
                record.rolled_back_at,
                record.from_reference,
                record.to_reference,
            ],
        )
        .map_err(|source| local_state_query(path, "write rollback record", source))?;

    Ok(connection.last_insert_rowid())
}

pub(crate) fn upsert_telemetry_settings_in(
    connection: &Connection,
    path: &Path,
    settings: &TelemetrySettings,
) -> Result<(), AppError> {
    validate_telemetry_settings(path, settings)?;
    connection
        .execute(
            "INSERT INTO telemetry_settings (singleton_id, consent, notice_seen_at, updated_at) \
             VALUES (1, ?1, ?2, ?3) ON CONFLICT(singleton_id) DO UPDATE SET \
             consent = excluded.consent, \
             notice_seen_at = excluded.notice_seen_at, \
             updated_at = excluded.updated_at",
            params![
                settings.consent.as_str(),
                settings.notice_seen_at,
                settings.updated_at,
            ],
        )
        .map_err(|source| local_state_query(path, "write telemetry settings", source))?;

    Ok(())
}

pub(crate) fn insert_history_entry_in(
    connection: &Connection,
    path: &Path,
    entry: &HistoryEntry,
) -> Result<i64, AppError> {
    validate_history_entry(path, entry)?;
    let details_json = encode_history_details(path, &entry.details)?;

    connection
        .execute(
            "INSERT INTO history_events (kind, scope, skill_id, target, occurred_at, summary, \
             details_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                entry.kind.as_str(),
                entry.scope.map(ManagedScope::as_str),
                entry.skill_id,
                entry.target.map(TargetRuntime::as_str),
                entry.occurred_at,
                entry.summary,
                details_json,
            ],
        )
        .map_err(|source| local_state_query(path, "write history entry", source))?;

    Ok(connection.last_insert_rowid())
}

fn bootstrap_connection(connection: &Connection, path: &Path) -> Result<(), AppError> {
    connection
        .execute_batch("PRAGMA foreign_keys = ON;")
        .map_err(|source| local_state_query(path, "enable foreign keys", source))?;
    initialize_or_validate_schema(connection, path)
}

fn initialize_or_validate_schema(connection: &Connection, path: &Path) -> Result<(), AppError> {
    let found = schema_version(connection, path)?;
    if found == 0 {
        if database_has_user_tables(connection, path)? {
            return Err(local_state_validation(
                path,
                "schema version is missing but user tables already exist",
            ));
        }

        connection
            .execute_batch(&local_state_schema_sql())
            .map_err(|source| local_state_query(path, "initialize local state schema", source))?;
        return Ok(());
    }

    match LOCAL_STATE_SCHEMA_POLICY.classify(found) {
        VersionDisposition::Current => validate_required_tables(connection, path),
        VersionDisposition::NeedsMigration { from, to } => Err(local_state_validation(
            path,
            format!("schema version {from} requires migration to {to}"),
        )),
        VersionDisposition::Unsupported {
            found,
            minimum_supported,
            current,
        } => {
            let message = if minimum_supported == current {
                format!("schema version must be {current}, found {found}")
            } else {
                format!(
                    "schema version supports {minimum_supported} through {current}, found {found}"
                )
            };
            Err(local_state_validation(path, message))
        }
    }
}

fn schema_version(connection: &Connection, path: &Path) -> Result<u32, AppError> {
    let found: i64 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(|source| local_state_query(path, "read schema version", source))?;

    u32::try_from(found).map_err(|_| {
        local_state_validation(
            path,
            format!("schema version must be a non-negative integer, found {found}"),
        )
    })
}

fn database_has_user_tables(connection: &Connection, path: &Path) -> Result<bool, AppError> {
    let count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
            [],
            |row| row.get(0),
        )
        .map_err(|source| local_state_query(path, "inspect existing tables", source))?;

    Ok(count > 0)
}

fn validate_required_tables(connection: &Connection, path: &Path) -> Result<(), AppError> {
    let mut statement = connection
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'")
        .map_err(|source| local_state_query(path, "prepare required tables query", source))?;
    let rows = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|source| local_state_query(path, "query required tables", source))?;

    let mut tables = BTreeSet::new();
    for row in rows {
        tables.insert(row.map_err(|source| local_state_query(path, "read table name", source))?);
    }

    let missing: Vec<&str> = REQUIRED_TABLES
        .iter()
        .copied()
        .filter(|table| !tables.contains(*table))
        .collect();
    if !missing.is_empty() {
        return Err(local_state_validation(
            path,
            format!("schema is missing required tables: {}", missing.join(", ")),
        ));
    }

    Ok(())
}

fn local_state_schema_sql() -> String {
    format!(
        r#"
CREATE TABLE install_records (
    scope TEXT NOT NULL CHECK (scope IN ('workspace', 'user')),
    skill_id TEXT NOT NULL,
    source_kind TEXT NOT NULL CHECK (source_kind IN ('git', 'local-path', 'archive')),
    source_url TEXT NOT NULL,
    source_subpath TEXT NOT NULL,
    resolved_revision TEXT NOT NULL,
    upstream_revision TEXT,
    content_hash TEXT NOT NULL,
    overlay_hash TEXT NOT NULL,
    effective_version_hash TEXT NOT NULL,
    installed_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    detached INTEGER NOT NULL CHECK (detached IN (0, 1)),
    forked INTEGER NOT NULL CHECK (forked IN (0, 1)),
    PRIMARY KEY (scope, skill_id)
);

CREATE TABLE projection_records (
    scope TEXT NOT NULL CHECK (scope IN ('workspace', 'user')),
    skill_id TEXT NOT NULL,
    target TEXT NOT NULL CHECK (
        target IN ('codex', 'claude-code', 'github-copilot', 'gemini-cli', 'amp', 'opencode')
    ),
    generation_mode TEXT NOT NULL CHECK (generation_mode IN ('copy', 'symlink')),
    physical_root TEXT NOT NULL,
    projected_path TEXT NOT NULL,
    effective_version_hash TEXT NOT NULL,
    generated_at TEXT NOT NULL,
    PRIMARY KEY (scope, skill_id, target, physical_root)
);

CREATE TABLE update_checks (
    id INTEGER PRIMARY KEY,
    scope TEXT NOT NULL CHECK (scope IN ('workspace', 'user')),
    skill_id TEXT NOT NULL,
    checked_at TEXT NOT NULL,
    pinned_revision TEXT NOT NULL,
    latest_revision TEXT,
    outcome TEXT NOT NULL CHECK (
        outcome IN ('up-to-date', 'update-available', 'blocked', 'detached', 'local-source', 'failed')
    ),
    overlay_detected INTEGER NOT NULL CHECK (overlay_detected IN (0, 1)),
    local_modification_detected INTEGER NOT NULL CHECK (local_modification_detected IN (0, 1)),
    notes TEXT
);

CREATE INDEX idx_update_checks_skill_time
    ON update_checks (scope, skill_id, checked_at DESC, id DESC);

CREATE TABLE local_modifications (
    id INTEGER PRIMARY KEY,
    scope TEXT NOT NULL CHECK (scope IN ('workspace', 'user')),
    skill_id TEXT NOT NULL,
    detected_at TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('overlay', 'projected-copy', 'detached-fork')),
    path TEXT,
    details TEXT
);

CREATE INDEX idx_local_modifications_skill_time
    ON local_modifications (scope, skill_id, detected_at DESC, id DESC);

CREATE TABLE pins (
    scope TEXT NOT NULL CHECK (scope IN ('workspace', 'user')),
    skill_id TEXT NOT NULL,
    requested_reference TEXT NOT NULL,
    resolved_revision TEXT NOT NULL,
    effective_version_hash TEXT,
    pinned_at TEXT NOT NULL,
    PRIMARY KEY (scope, skill_id)
);

CREATE TABLE rollback_records (
    id INTEGER PRIMARY KEY,
    scope TEXT NOT NULL CHECK (scope IN ('workspace', 'user')),
    skill_id TEXT NOT NULL,
    rolled_back_at TEXT NOT NULL,
    from_reference TEXT NOT NULL,
    to_reference TEXT NOT NULL
);

CREATE INDEX idx_rollback_records_skill_time
    ON rollback_records (scope, skill_id, rolled_back_at DESC, id DESC);

CREATE TABLE telemetry_settings (
    singleton_id INTEGER PRIMARY KEY CHECK (singleton_id = 1),
    consent TEXT NOT NULL CHECK (consent IN ('unknown', 'enabled', 'disabled')),
    notice_seen_at TEXT,
    updated_at TEXT NOT NULL
);

CREATE TABLE history_events (
    id INTEGER PRIMARY KEY,
    kind TEXT NOT NULL CHECK (
        kind IN (
            'install',
            'update-check',
            'update-applied',
            'projection',
            'pin',
            'rollback',
            'overlay-created',
            'direct-modification-detected',
            'detach',
            'fork',
            'cleanup',
            'prune',
            'telemetry-consent-changed'
        )
    ),
    scope TEXT CHECK (scope IN ('workspace', 'user')),
    skill_id TEXT,
    target TEXT CHECK (
        target IS NULL OR
        target IN ('codex', 'claude-code', 'github-copilot', 'gemini-cli', 'amp', 'opencode')
    ),
    occurred_at TEXT NOT NULL,
    summary TEXT NOT NULL,
    details_json TEXT
);

CREATE INDEX idx_history_events_skill_time
    ON history_events (scope, skill_id, occurred_at DESC, id DESC);

PRAGMA user_version = {version};
"#,
        version = CURRENT_LOCAL_STATE_VERSION
    )
}

fn resolve_home_directory() -> Option<PathBuf> {
    let home = env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty());
    if home.is_some() {
        return home;
    }

    let user_profile = env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty());
    if user_profile.is_some() {
        return user_profile;
    }

    match (env::var_os("HOMEDRIVE"), env::var_os("HOMEPATH")) {
        (Some(drive), Some(path)) => {
            let mut combined = PathBuf::from(drive);
            combined.push(path);
            if combined.as_os_str().is_empty() {
                None
            } else {
                Some(combined)
            }
        }
        _ => None,
    }
}

type InstallRow = (
    String,
    String,
    String,
    String,
    String,
    String,
    Option<String>,
    String,
    String,
    String,
    String,
    String,
    i64,
    i64,
);

type ProjectionRow = (
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    String,
);

type UpdateCheckRow = (
    i64,
    String,
    String,
    String,
    String,
    Option<String>,
    String,
    i64,
    i64,
    Option<String>,
);

type LocalModificationRow = (
    i64,
    String,
    String,
    String,
    String,
    Option<String>,
    Option<String>,
);

type PinRow = (String, String, String, String, Option<String>, String);

type RollbackRow = (i64, String, String, String, String, String);

type TelemetrySettingsRow = (String, Option<String>, String);

type HistoryEntryRow = (
    i64,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    String,
    String,
    Option<String>,
);

fn decode_install_record(path: &Path, row: InstallRow) -> Result<InstallRecord, AppError> {
    Ok(InstallRecord {
        skill: ManagedSkillRef::new(parse_scope(path, "install_records.scope", &row.0)?, row.1),
        source_kind: parse_source_kind(path, "install_records.source_kind", &row.2)?,
        source_url: row.3,
        source_subpath: row.4,
        resolved_revision: row.5,
        upstream_revision: row.6,
        content_hash: row.7,
        overlay_hash: row.8,
        effective_version_hash: row.9,
        installed_at: row.10,
        updated_at: row.11,
        detached: int_to_bool(path, "install_records.detached", row.12)?,
        forked: int_to_bool(path, "install_records.forked", row.13)?,
    })
}

fn decode_projection_record(path: &Path, row: ProjectionRow) -> Result<ProjectionRecord, AppError> {
    Ok(ProjectionRecord {
        skill: ManagedSkillRef::new(
            parse_scope(path, "projection_records.scope", &row.0)?,
            row.1,
        ),
        target: parse_target_runtime(path, "projection_records.target", &row.2)?,
        generation_mode: parse_projection_mode(path, "projection_records.generation_mode", &row.3)?,
        physical_root: row.4,
        projected_path: row.5,
        effective_version_hash: row.6,
        generated_at: row.7,
    })
}

fn decode_update_check_record(
    path: &Path,
    row: UpdateCheckRow,
) -> Result<UpdateCheckRecord, AppError> {
    Ok(UpdateCheckRecord {
        id: Some(row.0),
        skill: ManagedSkillRef::new(parse_scope(path, "update_checks.scope", &row.1)?, row.2),
        checked_at: row.3,
        pinned_revision: row.4,
        latest_revision: row.5,
        outcome: parse_update_check_outcome(path, "update_checks.outcome", &row.6)?,
        overlay_detected: int_to_bool(path, "update_checks.overlay_detected", row.7)?,
        local_modification_detected: int_to_bool(
            path,
            "update_checks.local_modification_detected",
            row.8,
        )?,
        notes: row.9,
    })
}

fn decode_local_modification_record(
    path: &Path,
    row: LocalModificationRow,
) -> Result<LocalModificationRecord, AppError> {
    Ok(LocalModificationRecord {
        id: Some(row.0),
        skill: ManagedSkillRef::new(
            parse_scope(path, "local_modifications.scope", &row.1)?,
            row.2,
        ),
        detected_at: row.3,
        kind: parse_local_modification_kind(path, "local_modifications.kind", &row.4)?,
        path: row.5,
        details: row.6,
    })
}

fn decode_pin_record(path: &Path, row: PinRow) -> Result<PinRecord, AppError> {
    Ok(PinRecord {
        skill: ManagedSkillRef::new(parse_scope(path, "pins.scope", &row.0)?, row.1),
        requested_reference: row.2,
        resolved_revision: row.3,
        effective_version_hash: row.4,
        pinned_at: row.5,
    })
}

fn decode_rollback_record(path: &Path, row: RollbackRow) -> Result<RollbackRecord, AppError> {
    Ok(RollbackRecord {
        id: Some(row.0),
        skill: ManagedSkillRef::new(parse_scope(path, "rollback_records.scope", &row.1)?, row.2),
        rolled_back_at: row.3,
        from_reference: row.4,
        to_reference: row.5,
    })
}

fn decode_telemetry_settings(
    path: &Path,
    row: TelemetrySettingsRow,
) -> Result<TelemetrySettings, AppError> {
    Ok(TelemetrySettings {
        consent: parse_telemetry_consent(path, "telemetry_settings.consent", &row.0)?,
        notice_seen_at: row.1,
        updated_at: row.2,
    })
}

fn decode_history_entry(path: &Path, row: HistoryEntryRow) -> Result<HistoryEntry, AppError> {
    let scope = match row.2 {
        Some(scope) => Some(parse_scope(path, "history_events.scope", &scope)?),
        None => None,
    };
    let target = match row.4 {
        Some(target) => Some(parse_target_runtime(
            path,
            "history_events.target",
            &target,
        )?),
        None => None,
    };

    Ok(HistoryEntry {
        id: Some(row.0),
        kind: HistoryEventKind::parse(&row.1).ok_or_else(|| {
            local_state_validation(
                path,
                format!("history_events.kind has unsupported value '{}'", row.1),
            )
        })?,
        scope,
        skill_id: row.3,
        target,
        occurred_at: row.5,
        summary: row.6,
        details: decode_history_details(path, row.7)?,
    })
}

fn parse_scope(path: &Path, field: &str, value: &str) -> Result<ManagedScope, AppError> {
    match value {
        "workspace" => Ok(ManagedScope::Workspace),
        "user" => Ok(ManagedScope::User),
        _ => Err(local_state_validation(
            path,
            format!("{field} has unsupported scope '{value}'"),
        )),
    }
}

fn parse_projection_mode(
    path: &Path,
    field: &str,
    value: &str,
) -> Result<ProjectionMode, AppError> {
    match value {
        "copy" => Ok(ProjectionMode::Copy),
        "symlink" => Ok(ProjectionMode::Symlink),
        _ => Err(local_state_validation(
            path,
            format!("{field} has unsupported projection mode '{value}'"),
        )),
    }
}

fn parse_update_check_outcome(
    path: &Path,
    field: &str,
    value: &str,
) -> Result<UpdateCheckOutcome, AppError> {
    match value {
        "up-to-date" => Ok(UpdateCheckOutcome::UpToDate),
        "update-available" => Ok(UpdateCheckOutcome::UpdateAvailable),
        "blocked" => Ok(UpdateCheckOutcome::Blocked),
        "detached" => Ok(UpdateCheckOutcome::Detached),
        "local-source" => Ok(UpdateCheckOutcome::LocalSource),
        "failed" => Ok(UpdateCheckOutcome::Failed),
        _ => Err(local_state_validation(
            path,
            format!("{field} has unsupported update outcome '{value}'"),
        )),
    }
}

fn parse_local_modification_kind(
    path: &Path,
    field: &str,
    value: &str,
) -> Result<LocalModificationKind, AppError> {
    match value {
        "overlay" => Ok(LocalModificationKind::Overlay),
        "projected-copy" => Ok(LocalModificationKind::ProjectedCopy),
        "detached-fork" => Ok(LocalModificationKind::DetachedFork),
        _ => Err(local_state_validation(
            path,
            format!("{field} has unsupported local modification kind '{value}'"),
        )),
    }
}

fn parse_telemetry_consent(
    path: &Path,
    field: &str,
    value: &str,
) -> Result<TelemetryConsent, AppError> {
    match value {
        "unknown" => Ok(TelemetryConsent::Unknown),
        "enabled" => Ok(TelemetryConsent::Enabled),
        "disabled" => Ok(TelemetryConsent::Disabled),
        _ => Err(local_state_validation(
            path,
            format!("{field} has unsupported telemetry consent '{value}'"),
        )),
    }
}

fn parse_source_kind(path: &Path, field: &str, value: &str) -> Result<SourceKind, AppError> {
    match value {
        "git" => Ok(SourceKind::Git),
        "local-path" => Ok(SourceKind::LocalPath),
        "archive" => Ok(SourceKind::Archive),
        _ => Err(local_state_validation(
            path,
            format!("{field} has unsupported source kind '{value}'"),
        )),
    }
}

fn parse_target_runtime(path: &Path, field: &str, value: &str) -> Result<TargetRuntime, AppError> {
    match value {
        "codex" => Ok(TargetRuntime::Codex),
        "claude-code" => Ok(TargetRuntime::ClaudeCode),
        "github-copilot" => Ok(TargetRuntime::GithubCopilot),
        "gemini-cli" => Ok(TargetRuntime::GeminiCli),
        "amp" => Ok(TargetRuntime::Amp),
        "opencode" => Ok(TargetRuntime::Opencode),
        _ => Err(local_state_validation(
            path,
            format!("{field} has unsupported target runtime '{value}'"),
        )),
    }
}

fn source_kind_as_str(kind: SourceKind) -> &'static str {
    match kind {
        SourceKind::Git => "git",
        SourceKind::LocalPath => "local-path",
        SourceKind::Archive => "archive",
    }
}

fn encode_history_details(
    path: &Path,
    details: &HistoryDetails,
) -> Result<Option<String>, AppError> {
    if details.is_empty() {
        return Ok(None);
    }

    serde_json::to_string(details).map(Some).map_err(|source| {
        local_state_validation(
            path,
            format!("history details must be valid JSON: {source}"),
        )
    })
}

fn decode_history_details(path: &Path, raw: Option<String>) -> Result<HistoryDetails, AppError> {
    match raw {
        Some(raw) => serde_json::from_str(&raw).map_err(|source| {
            local_state_validation(
                path,
                format!("history details must decode from JSON: {source}"),
            )
        }),
        None => Ok(BTreeMap::new()),
    }
}

fn bool_to_int(value: bool) -> i64 {
    if value { 1 } else { 0 }
}

fn int_to_bool(path: &Path, field: &str, value: i64) -> Result<bool, AppError> {
    match value {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(local_state_validation(
            path,
            format!("{field} must be 0 or 1, found {value}"),
        )),
    }
}

fn validate_install_record(path: &Path, record: &InstallRecord) -> Result<(), AppError> {
    validate_skill_ref(path, &record.skill)?;
    validate_non_empty_trimmed(path, "install.source_url", &record.source_url)?;
    validate_relative_path(path, "install.source_subpath", &record.source_subpath)?;
    validate_token(path, "install.resolved_revision", &record.resolved_revision)?;
    if let Some(upstream) = &record.upstream_revision {
        validate_token(path, "install.upstream_revision", upstream)?;
    }
    validate_token(path, "install.content_hash", &record.content_hash)?;
    validate_token(path, "install.overlay_hash", &record.overlay_hash)?;
    validate_token(
        path,
        "install.effective_version_hash",
        &record.effective_version_hash,
    )?;
    validate_timestamp(path, "install.installed_at", &record.installed_at)?;
    validate_timestamp(path, "install.updated_at", &record.updated_at)?;
    Ok(())
}

fn validate_projection_record(path: &Path, record: &ProjectionRecord) -> Result<(), AppError> {
    validate_skill_ref(path, &record.skill)?;
    validate_non_empty_trimmed(path, "projection.physical_root", &record.physical_root)?;
    validate_relative_path(path, "projection.projected_path", &record.projected_path)?;
    validate_token(
        path,
        "projection.effective_version_hash",
        &record.effective_version_hash,
    )?;
    validate_timestamp(path, "projection.generated_at", &record.generated_at)?;
    Ok(())
}

fn validate_update_check_record(path: &Path, record: &UpdateCheckRecord) -> Result<(), AppError> {
    validate_skill_ref(path, &record.skill)?;
    validate_timestamp(path, "update_check.checked_at", &record.checked_at)?;
    validate_token(
        path,
        "update_check.pinned_revision",
        &record.pinned_revision,
    )?;
    if let Some(latest) = &record.latest_revision {
        validate_token(path, "update_check.latest_revision", latest)?;
    }
    if let Some(notes) = &record.notes {
        validate_non_empty_trimmed(path, "update_check.notes", notes)?;
    }
    Ok(())
}

fn validate_local_modification_record(
    path: &Path,
    record: &LocalModificationRecord,
) -> Result<(), AppError> {
    validate_skill_ref(path, &record.skill)?;
    validate_timestamp(path, "local_modification.detected_at", &record.detected_at)?;
    if let Some(modified_path) = &record.path {
        validate_non_empty_trimmed(path, "local_modification.path", modified_path)?;
    }
    if let Some(details) = &record.details {
        validate_non_empty_trimmed(path, "local_modification.details", details)?;
    }
    Ok(())
}

fn validate_pin_record(path: &Path, record: &PinRecord) -> Result<(), AppError> {
    validate_skill_ref(path, &record.skill)?;
    validate_non_empty_trimmed(path, "pin.requested_reference", &record.requested_reference)?;
    validate_token(path, "pin.resolved_revision", &record.resolved_revision)?;
    if let Some(hash) = &record.effective_version_hash {
        validate_token(path, "pin.effective_version_hash", hash)?;
    }
    validate_timestamp(path, "pin.pinned_at", &record.pinned_at)?;
    Ok(())
}

fn validate_rollback_record(path: &Path, record: &RollbackRecord) -> Result<(), AppError> {
    validate_skill_ref(path, &record.skill)?;
    validate_timestamp(path, "rollback.rolled_back_at", &record.rolled_back_at)?;
    validate_non_empty_trimmed(path, "rollback.from_reference", &record.from_reference)?;
    validate_non_empty_trimmed(path, "rollback.to_reference", &record.to_reference)?;
    Ok(())
}

fn validate_telemetry_settings(path: &Path, settings: &TelemetrySettings) -> Result<(), AppError> {
    if let Some(notice_seen_at) = &settings.notice_seen_at {
        validate_timestamp(path, "telemetry.notice_seen_at", notice_seen_at)?;
    }
    validate_timestamp(path, "telemetry.updated_at", &settings.updated_at)?;
    Ok(())
}

fn validate_history_entry(path: &Path, entry: &HistoryEntry) -> Result<(), AppError> {
    if entry.skill_id.is_some() && entry.scope.is_none() {
        return Err(local_state_validation(
            path,
            "history entry must include a scope when skill_id is present",
        ));
    }
    if let Some(skill_id) = &entry.skill_id {
        validate_skill_id(path, "history.skill_id", skill_id)?;
    }
    validate_timestamp(path, "history.occurred_at", &entry.occurred_at)?;
    validate_non_empty_trimmed(path, "history.summary", &entry.summary)?;
    for key in entry.details.keys() {
        validate_non_empty_trimmed(path, "history.details key", key)?;
    }
    Ok(())
}

fn validate_skill_ref(path: &Path, skill: &ManagedSkillRef) -> Result<(), AppError> {
    validate_skill_id(path, "skill_id", &skill.skill_id)
}

fn validate_skill_id(path: &Path, field: &str, value: &str) -> Result<(), AppError> {
    if value.is_empty() {
        return Err(local_state_validation(
            path,
            format!("{field} must not be empty"),
        ));
    }
    if value.len() > 64 {
        return Err(local_state_validation(
            path,
            format!(
                "{field} must be at most 64 characters, found {}",
                value.len()
            ),
        ));
    }
    if value.starts_with('-') || value.ends_with('-') || value.contains("--") {
        return Err(local_state_validation(
            path,
            format!("{field} must use lowercase letters, digits, and single hyphens: '{value}'"),
        ));
    }
    if !value
        .bytes()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(local_state_validation(
            path,
            format!("{field} must use lowercase letters, digits, and hyphens only: '{value}'"),
        ));
    }

    Ok(())
}

fn validate_non_empty_trimmed(path: &Path, field: &str, value: &str) -> Result<(), AppError> {
    if value.trim().is_empty() {
        return Err(local_state_validation(
            path,
            format!("{field} must not be empty"),
        ));
    }
    if value.trim() != value {
        return Err(local_state_validation(
            path,
            format!("{field} must not contain leading or trailing whitespace"),
        ));
    }
    if value.chars().any(char::is_control) {
        return Err(local_state_validation(
            path,
            format!("{field} must not contain control characters"),
        ));
    }

    Ok(())
}

fn validate_token(path: &Path, field: &str, value: &str) -> Result<(), AppError> {
    validate_non_empty_trimmed(path, field, value)?;
    if value.chars().any(char::is_whitespace) {
        return Err(local_state_validation(
            path,
            format!("{field} must not contain whitespace: '{value}'"),
        ));
    }
    Ok(())
}

fn validate_relative_path(path: &Path, field: &str, value: &str) -> Result<(), AppError> {
    validate_non_empty_trimmed(path, field, value)?;

    if value.starts_with('/') {
        return Err(local_state_validation(
            path,
            format!("{field} must be relative, found absolute path '{value}'"),
        ));
    }
    if value.contains('\\') {
        return Err(local_state_validation(
            path,
            format!("{field} must use '/' separators: '{value}'"),
        ));
    }
    if value.contains(':') {
        return Err(local_state_validation(
            path,
            format!("{field} must not contain ':' so it remains portable: '{value}'"),
        ));
    }
    if value.ends_with('/') {
        return Err(local_state_validation(
            path,
            format!("{field} must not end with '/': '{value}'"),
        ));
    }

    for segment in value.split('/') {
        if segment.is_empty() {
            return Err(local_state_validation(
                path,
                format!("{field} must not contain empty path segments: '{value}'"),
            ));
        }
        if matches!(segment, "." | "..") {
            return Err(local_state_validation(
                path,
                format!("{field} must not contain '.' or '..' segments: '{value}'"),
            ));
        }
    }

    Ok(())
}

fn validate_timestamp(path: &Path, field: &str, value: &str) -> Result<(), AppError> {
    validate_non_empty_trimmed(path, field, value)?;

    let bytes = value.as_bytes();
    let matches_shape = bytes.len() == 20
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[10] == b'T'
        && bytes[13] == b':'
        && bytes[16] == b':'
        && bytes[19] == b'Z'
        && bytes.iter().enumerate().all(|(index, byte)| match index {
            4 | 7 | 10 | 13 | 16 | 19 => true,
            _ => byte.is_ascii_digit(),
        });

    if !matches_shape {
        return Err(local_state_validation(
            path,
            format!("{field} must use deterministic UTC RFC3339 timestamps, found '{value}'"),
        ));
    }

    Ok(())
}

fn local_state_query(path: &Path, operation: &'static str, source: rusqlite::Error) -> AppError {
    AppError::LocalStateQuery {
        path: path.to_path_buf(),
        operation,
        source,
    }
}

fn local_state_validation(path: &Path, message: impl Into<String>) -> AppError {
    AppError::LocalStateValidation {
        path: path.to_path_buf(),
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    #[test]
    fn local_state_bootstraps_required_tables_and_schema_version() {
        let temp = tempdir().expect("tempdir exists");
        let path = temp.path().join("state.db");
        let store = LocalStateStore::open_at(&path).expect("state store opens");

        assert_eq!(
            store.schema_version().expect("schema version is readable"),
            CURRENT_LOCAL_STATE_VERSION
        );

        let mut statement = store
            .connection
            .prepare(
                "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
            )
            .expect("table query prepares");
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))
            .expect("table query runs");
        let mut tables = BTreeSet::new();
        for row in rows {
            tables.insert(row.expect("table name is readable"));
        }

        for table in REQUIRED_TABLES {
            assert!(tables.contains(*table), "missing required table {table}");
        }
    }

    #[test]
    fn local_state_roundtrips_records_and_snapshot_queries() {
        let mut store = LocalStateStore::open_in_memory().expect("state store opens");
        let skill = ManagedSkillRef::new(ManagedScope::Workspace, "ai-sdk");

        let install = InstallRecord {
            skill: skill.clone(),
            source_kind: SourceKind::Git,
            source_url: "https://github.com/vercel/ai.git".to_string(),
            source_subpath: "skills/ai-sdk".to_string(),
            resolved_revision: "0123456789abcdef".to_string(),
            upstream_revision: Some("fedcba9876543210".to_string()),
            content_hash: "sha256:content".to_string(),
            overlay_hash: "sha256:overlay".to_string(),
            effective_version_hash: "sha256:effective".to_string(),
            installed_at: "2026-03-19T11:00:00Z".to_string(),
            updated_at: "2026-03-19T11:05:00Z".to_string(),
            detached: false,
            forked: false,
        };
        let projection = ProjectionRecord {
            skill: skill.clone(),
            target: TargetRuntime::ClaudeCode,
            generation_mode: ProjectionMode::Copy,
            physical_root: ".claude/skills".to_string(),
            projected_path: "ai-sdk".to_string(),
            effective_version_hash: "sha256:effective".to_string(),
            generated_at: "2026-03-19T11:06:00Z".to_string(),
        };
        let update_check = UpdateCheckRecord {
            id: None,
            skill: skill.clone(),
            checked_at: "2026-03-19T11:07:00Z".to_string(),
            pinned_revision: "0123456789abcdef".to_string(),
            latest_revision: Some("1111111111111111".to_string()),
            outcome: UpdateCheckOutcome::UpdateAvailable,
            overlay_detected: true,
            local_modification_detected: false,
            notes: Some("upstream advanced by one commit".to_string()),
        };
        let modification = LocalModificationRecord {
            id: None,
            skill: skill.clone(),
            detected_at: "2026-03-19T11:08:00Z".to_string(),
            kind: LocalModificationKind::Overlay,
            path: Some(".agents/overlays/ai-sdk/SKILL.md".to_string()),
            details: Some("overlay differs from pinned upstream".to_string()),
        };
        let pin = PinRecord {
            skill: skill.clone(),
            requested_reference: "refs/tags/v1.0.0".to_string(),
            resolved_revision: "0123456789abcdef".to_string(),
            effective_version_hash: Some("sha256:effective".to_string()),
            pinned_at: "2026-03-19T11:09:00Z".to_string(),
        };
        let rollback = RollbackRecord {
            id: None,
            skill: skill.clone(),
            rolled_back_at: "2026-03-19T11:10:00Z".to_string(),
            from_reference: "sha256:next".to_string(),
            to_reference: "sha256:effective".to_string(),
        };
        let telemetry = TelemetrySettings {
            consent: TelemetryConsent::Enabled,
            notice_seen_at: Some("2026-03-19T11:11:00Z".to_string()),
            updated_at: "2026-03-19T11:11:00Z".to_string(),
        };
        let mut details = BTreeMap::new();
        details.insert("revision".to_string(), json!("0123456789abcdef"));
        let history = HistoryEntry {
            id: None,
            kind: HistoryEventKind::Install,
            scope: Some(ManagedScope::Workspace),
            skill_id: Some("ai-sdk".to_string()),
            target: None,
            occurred_at: "2026-03-19T11:12:00Z".to_string(),
            summary: "Installed ai-sdk at 0123456789abcdef".to_string(),
            details,
        };

        store
            .upsert_install_record(&install)
            .expect("install record writes");
        store
            .upsert_projection_record(&projection)
            .expect("projection record writes");
        store
            .record_update_check(&update_check)
            .expect("update check writes");
        store
            .record_local_modification(&modification)
            .expect("local modification writes");
        store.upsert_pin_record(&pin).expect("pin writes");
        store.record_rollback(&rollback).expect("rollback writes");
        store
            .upsert_telemetry_settings(&telemetry)
            .expect("telemetry settings write");
        store
            .append_history_entry(&history)
            .expect("history entry writes");

        assert_eq!(
            store
                .install_record(&skill)
                .expect("install record loads")
                .expect("install record exists"),
            install
        );
        assert_eq!(
            store
                .projection_records(Some(&skill))
                .expect("projection record loads"),
            vec![projection.clone()]
        );
        assert_eq!(
            store
                .latest_update_check(&skill)
                .expect("latest update check loads")
                .expect("latest update check exists")
                .outcome,
            UpdateCheckOutcome::UpdateAvailable
        );
        assert_eq!(
            store
                .local_modifications(Some(&skill))
                .expect("local modification loads")
                .len(),
            1
        );
        assert_eq!(
            store
                .pin_record(&skill)
                .expect("pin record loads")
                .expect("pin record exists"),
            pin
        );
        assert_eq!(
            store
                .rollback_records(Some(&skill))
                .expect("rollback records load")
                .len(),
            1
        );
        assert_eq!(
            store
                .telemetry_settings()
                .expect("telemetry settings load")
                .expect("telemetry settings exist"),
            telemetry
        );
        assert_eq!(
            store
                .history_entries(&HistoryQuery::for_skill(skill.clone()))
                .expect("history loads")
                .len(),
            1
        );

        let snapshot = store.skill_snapshot(&skill).expect("snapshot loads");
        assert_eq!(snapshot.install, Some(install));
        assert_eq!(snapshot.pin, Some(pin));
        assert_eq!(snapshot.projections, vec![projection]);
        assert_eq!(snapshot.local_modifications.len(), 1);
        assert_eq!(snapshot.rollbacks.len(), 1);
    }

    #[test]
    fn history_queries_filter_by_skill_and_limit_results() {
        let mut store = LocalStateStore::open_in_memory().expect("state store opens");

        store
            .append_history_entry(&HistoryEntry {
                id: None,
                kind: HistoryEventKind::Install,
                scope: Some(ManagedScope::Workspace),
                skill_id: Some("ai-sdk".to_string()),
                target: None,
                occurred_at: "2026-03-19T11:00:00Z".to_string(),
                summary: "Installed ai-sdk".to_string(),
                details: BTreeMap::new(),
            })
            .expect("history entry writes");
        store
            .append_history_entry(&HistoryEntry {
                id: None,
                kind: HistoryEventKind::Projection,
                scope: Some(ManagedScope::Workspace),
                skill_id: Some("ai-sdk".to_string()),
                target: Some(TargetRuntime::ClaudeCode),
                occurred_at: "2026-03-19T11:01:00Z".to_string(),
                summary: "Projected ai-sdk into claude-code".to_string(),
                details: BTreeMap::new(),
            })
            .expect("history entry writes");
        store
            .append_history_entry(&HistoryEntry {
                id: None,
                kind: HistoryEventKind::Cleanup,
                scope: Some(ManagedScope::Workspace),
                skill_id: Some("release-notes".to_string()),
                target: None,
                occurred_at: "2026-03-19T11:02:00Z".to_string(),
                summary: "Cleaned stale projections for release-notes".to_string(),
                details: BTreeMap::new(),
            })
            .expect("history entry writes");

        let filtered = store
            .history_entries(&HistoryQuery {
                skill: Some(ManagedSkillRef::new(ManagedScope::Workspace, "ai-sdk")),
                limit: Some(1),
            })
            .expect("history loads");

        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].kind, HistoryEventKind::Projection);
        assert_eq!(filtered[0].skill_id.as_deref(), Some("ai-sdk"));
    }

    #[test]
    fn opening_rejects_unsupported_existing_schema_versions() {
        let temp = tempdir().expect("tempdir exists");
        let path = temp.path().join("state.db");
        let connection = Connection::open(&path).expect("sqlite database opens");
        connection
            .execute_batch("PRAGMA user_version = 99;")
            .expect("schema version can be set");
        drop(connection);

        let error = LocalStateStore::open_at(&path).expect_err("unsupported schema is rejected");
        assert!(
            error.to_string().contains("schema version"),
            "unexpected error: {error}"
        );
    }
}
