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
use sha2::{Digest, Sha256};

use crate::{
    adapter::TargetRuntime, error::AppError, history::HistoryEventKind, source::SourceKind,
};

mod codec;
mod schema;
#[cfg(test)]
mod tests;

use self::{codec::*, schema::*};

/// Current schema version for `.agents/skillctl.yaml`.
pub const CURRENT_MANIFEST_VERSION: u32 = 1;
/// Current schema version for `.agents/skillctl.lock`.
pub const CURRENT_LOCKFILE_VERSION: u32 = 1;
/// Current schema version for `~/.skillctl/state.db`.
pub const CURRENT_LOCAL_STATE_VERSION: u32 = 2;

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
    SchemaVersionPolicy::new(CURRENT_LOCAL_STATE_VERSION, 1);

const GLOBAL_WORKSPACE_KEY: &str = "__global__";

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
    workspace_key: String,
    connection: Connection,
}

impl std::fmt::Debug for LocalStateStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LocalStateStore")
            .field("path", &self.path)
            .field("workspace_key", &self.workspace_key)
            .finish_non_exhaustive()
    }
}

impl LocalStateStore {
    /// Open or create the default `~/.skillctl/state.db` database.
    pub fn open_default() -> Result<Self, AppError> {
        let working_directory =
            env::current_dir().map_err(|source| AppError::CurrentWorkingDirectory { source })?;
        Self::open_default_for(&working_directory)
    }

    /// Open or create the default `~/.skillctl/state.db` database for one workspace.
    pub fn open_default_for(working_directory: &Path) -> Result<Self, AppError> {
        Self::open_at_for(default_state_database_path()?, working_directory)
    }

    /// Open or create a store at an explicit database path.
    pub fn open_at(path: impl Into<PathBuf>) -> Result<Self, AppError> {
        let working_directory =
            env::current_dir().map_err(|source| AppError::CurrentWorkingDirectory { source })?;
        Self::open_at_for(path, &working_directory)
    }

    /// Open or create a store at an explicit database path for one workspace.
    pub fn open_at_for(
        path: impl Into<PathBuf>,
        working_directory: &Path,
    ) -> Result<Self, AppError> {
        let path = path.into();
        let workspace_key = workspace_key_for_path(working_directory)?;

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
        bootstrap_connection(&connection, &path, &workspace_key)?;

        Ok(Self {
            path,
            workspace_key,
            connection,
        })
    }

    /// Open an in-memory store. Intended for tests and pure logic callers.
    pub fn open_in_memory() -> Result<Self, AppError> {
        let working_directory =
            env::current_dir().map_err(|source| AppError::CurrentWorkingDirectory { source })?;
        Self::open_in_memory_for(&working_directory)
    }

    /// Open an in-memory store for one workspace. Intended for tests.
    pub fn open_in_memory_for(working_directory: &Path) -> Result<Self, AppError> {
        let path = PathBuf::from(":memory:");
        let workspace_key = workspace_key_for_path(working_directory)?;
        let connection =
            Connection::open_in_memory().map_err(|source| AppError::LocalStateOpen {
                path: path.clone(),
                source,
            })?;
        bootstrap_connection(&connection, &path, &workspace_key)?;

        Ok(Self {
            path,
            workspace_key,
            connection,
        })
    }

    /// Return the path to the backing SQLite database.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Return the current schema version persisted in the database.
    pub fn schema_version(&self) -> Result<u32, AppError> {
        schema_version(&self.connection, &self.path)
    }

    pub(crate) fn workspace_key_for_scope(&self, scope: ManagedScope) -> &str {
        match scope {
            ManagedScope::Workspace => self.workspace_key.as_str(),
            ManagedScope::User => GLOBAL_WORKSPACE_KEY,
        }
    }

    pub(crate) fn workspace_key_for_history_scope(&self, scope: Option<ManagedScope>) -> &str {
        match scope {
            Some(ManagedScope::Workspace) => self.workspace_key.as_str(),
            Some(ManagedScope::User) | None => GLOBAL_WORKSPACE_KEY,
        }
    }

    /// List every current install record in stable scope and skill order.
    pub fn list_install_records(&self) -> Result<Vec<InstallRecord>, AppError> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT workspace_key, scope, skill_id, source_kind, source_url, source_subpath, \
                 resolved_revision, upstream_revision, content_hash, overlay_hash, \
                 effective_version_hash, installed_at, updated_at, detached, forked \
                 FROM install_records \
                 WHERE (scope = 'workspace' AND workspace_key = ?1) \
                    OR (scope = 'user' AND workspace_key = ?2) \
                 ORDER BY CASE scope WHEN 'workspace' THEN 0 ELSE 1 END, skill_id",
            )
            .map_err(|source| {
                local_state_query(&self.path, "prepare install records query", source)
            })?;

        let rows = statement
            .query_map(
                params![self.workspace_key.as_str(), GLOBAL_WORKSPACE_KEY],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, String>(6)?,
                        row.get::<_, Option<String>>(7)?,
                        row.get::<_, String>(8)?,
                        row.get::<_, String>(9)?,
                        row.get::<_, String>(10)?,
                        row.get::<_, String>(11)?,
                        row.get::<_, String>(12)?,
                        row.get::<_, i64>(13)?,
                        row.get::<_, i64>(14)?,
                    ))
                },
            )
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
                "SELECT workspace_key, scope, skill_id, source_kind, source_url, source_subpath, \
                 resolved_revision, upstream_revision, content_hash, overlay_hash, \
                 effective_version_hash, installed_at, updated_at, detached, forked \
                 FROM install_records WHERE scope = ?1 AND workspace_key = ?2 AND skill_id = ?3",
            )
            .map_err(|source| {
                local_state_query(&self.path, "prepare install record query", source)
            })?;

        let row = statement
            .query_row(
                params![
                    skill.scope.as_str(),
                    self.workspace_key_for_scope(skill.scope),
                    skill.skill_id
                ],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, String>(6)?,
                        row.get::<_, Option<String>>(7)?,
                        row.get::<_, String>(8)?,
                        row.get::<_, String>(9)?,
                        row.get::<_, String>(10)?,
                        row.get::<_, String>(11)?,
                        row.get::<_, String>(12)?,
                        row.get::<_, i64>(13)?,
                        row.get::<_, i64>(14)?,
                    ))
                },
            )
            .optional()
            .map_err(|source| local_state_query(&self.path, "load install record", source))?;

        row.map(|record| decode_install_record(&self.path, record))
            .transpose()
    }

    /// Insert or update the current install record for a managed skill.
    pub fn upsert_install_record(&mut self, record: &InstallRecord) -> Result<(), AppError> {
        upsert_install_record_in(
            &self.connection,
            &self.path,
            self.workspace_key_for_scope(record.skill.scope),
            record,
        )
    }

    /// List projection records, optionally filtered to one managed skill.
    pub fn projection_records(
        &self,
        skill: Option<&ManagedSkillRef>,
    ) -> Result<Vec<ProjectionRecord>, AppError> {
        let sql_all = concat!(
            "SELECT workspace_key, scope, skill_id, target, generation_mode, physical_root, ",
            "projected_path, effective_version_hash, generated_at FROM projection_records ",
            "WHERE (scope = 'workspace' AND workspace_key = ?1) ",
            "   OR (scope = 'user' AND workspace_key = ?2) ",
            "ORDER BY scope, skill_id, target, physical_root"
        );
        let sql_filtered = concat!(
            "SELECT workspace_key, scope, skill_id, target, generation_mode, physical_root, ",
            "projected_path, effective_version_hash, generated_at FROM projection_records ",
            "WHERE scope = ?1 AND workspace_key = ?2 AND skill_id = ?3 ORDER BY target, physical_root"
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
                .query_map(
                    params![
                        skill.scope.as_str(),
                        self.workspace_key_for_scope(skill.scope),
                        skill.skill_id
                    ],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, String>(4)?,
                            row.get::<_, String>(5)?,
                            row.get::<_, String>(6)?,
                            row.get::<_, String>(7)?,
                            row.get::<_, String>(8)?,
                        ))
                    },
                )
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
                .query_map(
                    params![self.workspace_key.as_str(), GLOBAL_WORKSPACE_KEY],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, String>(4)?,
                            row.get::<_, String>(5)?,
                            row.get::<_, String>(6)?,
                            row.get::<_, String>(7)?,
                            row.get::<_, String>(8)?,
                        ))
                    },
                )
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
        upsert_projection_record_in(
            &self.connection,
            &self.path,
            self.workspace_key_for_scope(record.skill.scope),
            record,
        )
    }

    /// Replace the current projection records for one scope atomically.
    pub fn replace_projection_records_for_scope(
        &mut self,
        scope: ManagedScope,
        records: &[ProjectionRecord],
    ) -> Result<(), AppError> {
        let workspace_key = self.workspace_key_for_scope(scope).to_string();
        self.with_transaction("replace projection records", |connection, path| {
            connection
                .execute(
                    "DELETE FROM projection_records WHERE scope = ?1 AND workspace_key = ?2",
                    params![scope.as_str(), workspace_key.as_str()],
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
                upsert_projection_record_in(connection, path, workspace_key.as_str(), record)?;
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
            "SELECT id, workspace_key, scope, skill_id, checked_at, pinned_revision, latest_revision, ",
            "outcome, overlay_detected, local_modification_detected, notes FROM update_checks ",
            "WHERE (scope = 'workspace' AND workspace_key = ?1) ",
            "   OR (scope = 'user' AND workspace_key = ?2) ",
            "ORDER BY checked_at DESC, id DESC"
        );
        let sql_filtered = concat!(
            "SELECT id, workspace_key, scope, skill_id, checked_at, pinned_revision, latest_revision, ",
            "outcome, overlay_detected, local_modification_detected, notes FROM update_checks ",
            "WHERE scope = ?1 AND workspace_key = ?2 AND skill_id = ?3 ORDER BY checked_at DESC, id DESC"
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
                .query_map(
                    params![
                        skill.scope.as_str(),
                        self.workspace_key_for_scope(skill.scope),
                        skill.skill_id
                    ],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, String>(4)?,
                            row.get::<_, String>(5)?,
                            row.get::<_, Option<String>>(6)?,
                            row.get::<_, String>(7)?,
                            row.get::<_, i64>(8)?,
                            row.get::<_, i64>(9)?,
                            row.get::<_, Option<String>>(10)?,
                        ))
                    },
                )
                .map_err(|source| local_state_query(&self.path, "query update checks", source))?;

            for row in rows {
                let row = row
                    .map_err(|source| local_state_query(&self.path, "read update check", source))?;
                records.push(decode_update_check_record(&self.path, row)?);
            }
        } else {
            let rows = statement
                .query_map(
                    params![self.workspace_key.as_str(), GLOBAL_WORKSPACE_KEY],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, String>(4)?,
                            row.get::<_, String>(5)?,
                            row.get::<_, Option<String>>(6)?,
                            row.get::<_, String>(7)?,
                            row.get::<_, i64>(8)?,
                            row.get::<_, i64>(9)?,
                            row.get::<_, Option<String>>(10)?,
                        ))
                    },
                )
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
                "SELECT id, workspace_key, scope, skill_id, checked_at, pinned_revision, latest_revision, \
                 outcome, overlay_detected, local_modification_detected, notes \
                 FROM update_checks WHERE scope = ?1 AND workspace_key = ?2 AND skill_id = ?3 \
                 ORDER BY checked_at DESC, id DESC LIMIT 1",
            )
            .map_err(|source| {
                local_state_query(&self.path, "prepare latest update check query", source)
            })?;

        let row = statement
            .query_row(
                params![
                    skill.scope.as_str(),
                    self.workspace_key_for_scope(skill.scope),
                    skill.skill_id
                ],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, Option<String>>(6)?,
                        row.get::<_, String>(7)?,
                        row.get::<_, i64>(8)?,
                        row.get::<_, i64>(9)?,
                        row.get::<_, Option<String>>(10)?,
                    ))
                },
            )
            .optional()
            .map_err(|source| local_state_query(&self.path, "load latest update check", source))?;

        row.map(|record| decode_update_check_record(&self.path, record))
            .transpose()
    }

    /// Insert one immutable update check row.
    pub fn record_update_check(&mut self, record: &UpdateCheckRecord) -> Result<i64, AppError> {
        insert_update_check_record_in(
            &self.connection,
            &self.path,
            self.workspace_key_for_scope(record.skill.scope),
            record,
        )
    }

    /// List local modification records ordered newest first.
    pub fn local_modifications(
        &self,
        skill: Option<&ManagedSkillRef>,
    ) -> Result<Vec<LocalModificationRecord>, AppError> {
        let sql_all = concat!(
            "SELECT id, workspace_key, scope, skill_id, detected_at, kind, path, details ",
            "FROM local_modifications WHERE (scope = 'workspace' AND workspace_key = ?1) ",
            "   OR (scope = 'user' AND workspace_key = ?2) ORDER BY detected_at DESC, id DESC"
        );
        let sql_filtered = concat!(
            "SELECT id, workspace_key, scope, skill_id, detected_at, kind, path, details ",
            "FROM local_modifications WHERE scope = ?1 AND workspace_key = ?2 AND skill_id = ?3 ",
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
                .query_map(
                    params![
                        skill.scope.as_str(),
                        self.workspace_key_for_scope(skill.scope),
                        skill.skill_id
                    ],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, String>(4)?,
                            row.get::<_, String>(5)?,
                            row.get::<_, Option<String>>(6)?,
                            row.get::<_, Option<String>>(7)?,
                        ))
                    },
                )
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
                .query_map(
                    params![self.workspace_key.as_str(), GLOBAL_WORKSPACE_KEY],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, String>(4)?,
                            row.get::<_, String>(5)?,
                            row.get::<_, Option<String>>(6)?,
                            row.get::<_, Option<String>>(7)?,
                        ))
                    },
                )
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
        insert_local_modification_record_in(
            &self.connection,
            &self.path,
            self.workspace_key_for_scope(record.skill.scope),
            record,
        )
    }

    /// Load the current pin record for one managed skill.
    pub fn pin_record(&self, skill: &ManagedSkillRef) -> Result<Option<PinRecord>, AppError> {
        validate_skill_ref(&self.path, skill)?;

        let mut statement = self
            .connection
            .prepare(
                "SELECT workspace_key, scope, skill_id, requested_reference, resolved_revision, \
                 effective_version_hash, pinned_at FROM pins WHERE scope = ?1 AND workspace_key = ?2 AND skill_id = ?3",
            )
            .map_err(|source| local_state_query(&self.path, "prepare pin query", source))?;

        let row = statement
            .query_row(
                params![
                    skill.scope.as_str(),
                    self.workspace_key_for_scope(skill.scope),
                    skill.skill_id
                ],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, Option<String>>(5)?,
                        row.get::<_, String>(6)?,
                    ))
                },
            )
            .optional()
            .map_err(|source| local_state_query(&self.path, "load pin record", source))?;

        row.map(|record| decode_pin_record(&self.path, record))
            .transpose()
    }

    /// Insert or update the current pin for a managed skill.
    pub fn upsert_pin_record(&mut self, record: &PinRecord) -> Result<(), AppError> {
        upsert_pin_record_in(
            &self.connection,
            &self.path,
            self.workspace_key_for_scope(record.skill.scope),
            record,
        )
    }

    /// Delete the mutable install, pin, and projection rows for one managed skill.
    pub fn delete_current_skill_state(&mut self, skill: &ManagedSkillRef) -> Result<(), AppError> {
        validate_skill_ref(&self.path, skill)?;
        let workspace_key = self.workspace_key_for_scope(skill.scope).to_string();

        self.with_transaction("delete current skill state", |connection, path| {
            connection
                .execute(
                    "DELETE FROM projection_records WHERE scope = ?1 AND workspace_key = ?2 AND skill_id = ?3",
                    params![skill.scope.as_str(), workspace_key.as_str(), skill.skill_id],
                )
                .map_err(|source| {
                    local_state_query(path, "delete projection records for skill", source)
                })?;
            connection
                .execute(
                    "DELETE FROM pins WHERE scope = ?1 AND workspace_key = ?2 AND skill_id = ?3",
                    params![skill.scope.as_str(), workspace_key.as_str(), skill.skill_id],
                )
                .map_err(|source| local_state_query(path, "delete pin record", source))?;
            connection
                .execute(
                    "DELETE FROM install_records WHERE scope = ?1 AND workspace_key = ?2 AND skill_id = ?3",
                    params![skill.scope.as_str(), workspace_key.as_str(), skill.skill_id],
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
            "SELECT id, workspace_key, scope, skill_id, rolled_back_at, from_reference, to_reference ",
            "FROM rollback_records WHERE (scope = 'workspace' AND workspace_key = ?1) ",
            "   OR (scope = 'user' AND workspace_key = ?2) ORDER BY rolled_back_at DESC, id DESC"
        );
        let sql_filtered = concat!(
            "SELECT id, workspace_key, scope, skill_id, rolled_back_at, from_reference, to_reference ",
            "FROM rollback_records WHERE scope = ?1 AND workspace_key = ?2 AND skill_id = ?3 ",
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
                .query_map(
                    params![
                        skill.scope.as_str(),
                        self.workspace_key_for_scope(skill.scope),
                        skill.skill_id
                    ],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, String>(4)?,
                            row.get::<_, String>(5)?,
                            row.get::<_, String>(6)?,
                        ))
                    },
                )
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
                .query_map(
                    params![self.workspace_key.as_str(), GLOBAL_WORKSPACE_KEY],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, String>(4)?,
                            row.get::<_, String>(5)?,
                            row.get::<_, String>(6)?,
                        ))
                    },
                )
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
        insert_rollback_record_in(
            &self.connection,
            &self.path,
            self.workspace_key_for_scope(record.skill.scope),
            record,
        )
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
        insert_history_entry_in(&self.connection, &self.path, &self.workspace_key, entry)
    }

    /// Remove every current projection record across all scopes.
    pub fn clear_projection_records(&mut self) -> Result<(), AppError> {
        self.connection
            .execute(
                "DELETE FROM projection_records WHERE (scope = 'workspace' AND workspace_key = ?1) \
                 OR (scope = 'user' AND workspace_key = ?2)",
                params![self.workspace_key.as_str(), GLOBAL_WORKSPACE_KEY],
            )
            .map_err(|source| {
                local_state_query(&self.path, "delete all projection records", source)
            })?;
        Ok(())
    }

    /// Query history entries in deterministic newest-first order.
    pub fn history_entries(&self, query: &HistoryQuery) -> Result<Vec<HistoryEntry>, AppError> {
        let sql_all = concat!(
            "SELECT id, workspace_key, kind, scope, skill_id, target, occurred_at, summary, details_json ",
            "FROM history_events WHERE (scope = 'workspace' AND workspace_key = ?1) ",
            "   OR ((scope = 'user' OR scope IS NULL) AND workspace_key = ?2) ",
            "ORDER BY occurred_at DESC, id DESC"
        );
        let sql_filtered = concat!(
            "SELECT id, workspace_key, kind, scope, skill_id, target, occurred_at, summary, details_json ",
            "FROM history_events WHERE scope = ?1 AND workspace_key = ?2 AND skill_id = ?3 ",
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
                .query_map(
                    params![
                        skill.scope.as_str(),
                        self.workspace_key_for_scope(skill.scope),
                        skill.skill_id
                    ],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, Option<String>>(3)?,
                            row.get::<_, Option<String>>(4)?,
                            row.get::<_, Option<String>>(5)?,
                            row.get::<_, String>(6)?,
                            row.get::<_, String>(7)?,
                            row.get::<_, Option<String>>(8)?,
                        ))
                    },
                )
                .map_err(|source| local_state_query(&self.path, "query history entries", source))?;

            for row in rows {
                let row = row.map_err(|source| {
                    local_state_query(&self.path, "read history entry", source)
                })?;
                entries.push(decode_history_entry(&self.path, row)?);
            }
        } else {
            let rows = statement
                .query_map(
                    params![self.workspace_key.as_str(), GLOBAL_WORKSPACE_KEY],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, Option<String>>(3)?,
                            row.get::<_, Option<String>>(4)?,
                            row.get::<_, Option<String>>(5)?,
                            row.get::<_, String>(6)?,
                            row.get::<_, String>(7)?,
                            row.get::<_, Option<String>>(8)?,
                        ))
                    },
                )
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

/// Derive the stable workspace namespace key used for workspace-scoped state.
pub fn workspace_key_for_path(path: &Path) -> Result<String, AppError> {
    let canonical = fs::canonicalize(path).map_err(|source| AppError::FilesystemOperation {
        action: "canonicalize workspace path for state namespace",
        path: path.to_path_buf(),
        source,
    })?;
    let mut hasher = Sha256::new();
    hasher.update(canonical.to_string_lossy().as_bytes());
    Ok(format!("{:x}", hasher.finalize()))
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
    workspace_key: &str,
    record: &InstallRecord,
) -> Result<(), AppError> {
    validate_install_record(path, record)?;
    connection
        .execute(
            "INSERT INTO install_records (scope, workspace_key, skill_id, source_kind, source_url, \
             source_subpath, resolved_revision, upstream_revision, content_hash, overlay_hash, \
             effective_version_hash, installed_at, updated_at, detached, forked) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15) \
             ON CONFLICT(scope, workspace_key, skill_id) DO UPDATE SET \
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
                workspace_key,
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
    workspace_key: &str,
    record: &ProjectionRecord,
) -> Result<(), AppError> {
    validate_projection_record(path, record)?;
    connection
        .execute(
            "INSERT INTO projection_records (scope, workspace_key, skill_id, target, generation_mode, \
             physical_root, projected_path, effective_version_hash, generated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) \
             ON CONFLICT(scope, workspace_key, skill_id, target, physical_root) DO UPDATE SET \
             generation_mode = excluded.generation_mode, \
             projected_path = excluded.projected_path, \
             effective_version_hash = excluded.effective_version_hash, \
             generated_at = excluded.generated_at",
            params![
                record.skill.scope.as_str(),
                workspace_key,
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
    workspace_key: &str,
    record: &UpdateCheckRecord,
) -> Result<i64, AppError> {
    validate_update_check_record(path, record)?;
    connection
        .execute(
            "INSERT INTO update_checks (scope, workspace_key, skill_id, checked_at, pinned_revision, \
             latest_revision, outcome, overlay_detected, local_modification_detected, notes) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                record.skill.scope.as_str(),
                workspace_key,
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
    workspace_key: &str,
    record: &LocalModificationRecord,
) -> Result<i64, AppError> {
    validate_local_modification_record(path, record)?;
    connection
        .execute(
            "INSERT INTO local_modifications (scope, workspace_key, skill_id, detected_at, kind, path, details) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                record.skill.scope.as_str(),
                workspace_key,
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
    workspace_key: &str,
    record: &PinRecord,
) -> Result<(), AppError> {
    validate_pin_record(path, record)?;
    connection
        .execute(
            "INSERT INTO pins (scope, workspace_key, skill_id, requested_reference, resolved_revision, \
             effective_version_hash, pinned_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
             ON CONFLICT(scope, workspace_key, skill_id) DO UPDATE SET \
             requested_reference = excluded.requested_reference, \
             resolved_revision = excluded.resolved_revision, \
             effective_version_hash = excluded.effective_version_hash, \
             pinned_at = excluded.pinned_at",
            params![
                record.skill.scope.as_str(),
                workspace_key,
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
    workspace_key: &str,
    record: &RollbackRecord,
) -> Result<i64, AppError> {
    validate_rollback_record(path, record)?;
    connection
        .execute(
            "INSERT INTO rollback_records (scope, workspace_key, skill_id, rolled_back_at, from_reference, \
             to_reference) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                record.skill.scope.as_str(),
                workspace_key,
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
    workspace_key: &str,
    entry: &HistoryEntry,
) -> Result<i64, AppError> {
    validate_history_entry(path, entry)?;
    let details_json = encode_history_details(path, &entry.details)?;
    let scoped_workspace_key = match entry.scope {
        Some(ManagedScope::Workspace) => workspace_key,
        Some(ManagedScope::User) | None => GLOBAL_WORKSPACE_KEY,
    };

    connection
        .execute(
            "INSERT INTO history_events (workspace_key, kind, scope, skill_id, target, occurred_at, summary, \
             details_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                scoped_workspace_key,
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
