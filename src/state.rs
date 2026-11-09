//! Shared schema-version policy plus the versioned local SQLite state store.
//!
//! The store in this module is the authoritative local system of record for
//! installs, projections, update checks, local modifications, pins, rollbacks,
//! telemetry consent, and the append-only history ledger.

use std::{
    collections::BTreeMap,
    env, fs,
    path::{Path, PathBuf},
    time::Duration,
};

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{
    adapter::{TargetRuntime, TargetScope},
    error::AppError,
    history::HistoryEventKind,
    source::SourceKind,
};

mod codec;
mod journal;
mod lock;
mod schema;
#[cfg(test)]
mod tests;

use self::{codec::*, schema::*};

pub use self::journal::{
    RecoveryOutcome, journal_root, migration_backup_root, migration_backups, pending_operations,
    recover_pending_operations, write_file_atomically,
};

pub(crate) use self::journal::{JournalState, OperationJournal, recover_interrupted_operations};
pub(crate) use self::lock::CoordinationGuard;
pub(crate) use self::schema::resolve_home_directory;

/// Current schema version for `.agents/skillctl.yaml`.
pub const CURRENT_MANIFEST_VERSION: u32 = 1;
/// Current schema version for `.agents/skillctl.lock`.
pub const CURRENT_LOCKFILE_VERSION: u32 = 2;
/// Current schema version for `~/.skillctl/state.db`.
pub const CURRENT_LOCAL_STATE_VERSION: u32 = 6;

/// Default directory created under the home directory for local state.
pub const DEFAULT_LOCAL_STATE_DIR: &str = ".skillctl";
/// Default filename for the SQLite state database.
pub const DEFAULT_LOCAL_STATE_DATABASE_FILE: &str = "state.db";

/// Version policy for workspace manifests.
pub const MANIFEST_SCHEMA_POLICY: SchemaVersionPolicy =
    SchemaVersionPolicy::new(CURRENT_MANIFEST_VERSION, CURRENT_MANIFEST_VERSION);
/// Version policy for workspace lockfiles.
///
/// Lockfiles written as version 1 remain loadable through the compatibility
/// loader in `src/lockfile.rs`. They are upgraded in memory and written back as
/// the current version by the next command that persists the lockfile.
pub const LOCKFILE_SCHEMA_POLICY: SchemaVersionPolicy =
    SchemaVersionPolicy::new(CURRENT_LOCKFILE_VERSION, 1);
/// Version policy for the local state store.
pub const LOCAL_STATE_SCHEMA_POLICY: SchemaVersionPolicy =
    SchemaVersionPolicy::new(CURRENT_LOCAL_STATE_VERSION, 1);

const GLOBAL_WORKSPACE_KEY: &str = "__global__";

/// Environment variable that overrides the SQLite busy timeout of state
/// connections.
const STATE_BUSY_TIMEOUT_ENV: &str = "SKILLCTL_STATE_BUSY_TIMEOUT_MS";
/// Default time one state connection waits for a competing SQLite lock.
///
/// Read commands such as `list` and `history` open the store without the
/// coordination lock, so they depend on this wait while another process
/// commits. `rusqlite` sets five seconds by default, and a starved writer under
/// CPU saturation held its commit longer than that, so the read failed with
/// `database is locked`. Sixty seconds covers the worst observed commit under
/// load with headroom. See notes/state-lock-test-timing.md.
const DEFAULT_STATE_BUSY_TIMEOUT: Duration = Duration::from_secs(60);

const REQUIRED_TABLES: &[&str] = &[
    "history_events",
    "install_records",
    "local_modifications",
    "pins",
    "projection_records",
    "rollback_records",
    "skill_reviews",
    "telemetry_settings",
    "update_checks",
];

/// Mapping between one schema-v1 table and its current counterpart.
pub(crate) struct LegacyTableMigration {
    /// Name of the schema-v1 table after the rename step.
    pub legacy: &'static str,
    /// Name of the current table.
    pub current: &'static str,
    /// Current-table columns the legacy projection produces, in order.
    pub current_columns: &'static [&'static str],
    /// One expression per current column, evaluated over the legacy table.
    ///
    /// Most expressions repeat the column name. The `workspace_key` column
    /// derives its value from the legacy scope, so its expression is a `CASE`
    /// bound to the `?1` workspace key and the `?2` global key.
    pub legacy_expressions: &'static [&'static str],
    /// Columns that identify one row of the current table.
    pub identity_columns: &'static [&'static str],
}

/// Expression that derives a workspace key from the legacy scope column.
const WORKSPACE_KEY_EXPRESSION: &str = "CASE WHEN scope = 'workspace' THEN ?1 ELSE ?2 END";

/// Row mappings used by the schema-v1 to schema-v2 migration.
pub(crate) const LEGACY_TABLE_MIGRATIONS: &[LegacyTableMigration] = &[
    LegacyTableMigration {
        legacy: "install_records_v1",
        current: "install_records",
        current_columns: &[
            "scope",
            "workspace_key",
            "skill_id",
            "source_kind",
            "source_url",
            "source_subpath",
            "resolved_revision",
            "upstream_revision",
            "content_hash",
            "overlay_hash",
            "effective_version_hash",
            "installed_at",
            "updated_at",
            "detached",
            "forked",
        ],
        legacy_expressions: &[
            "scope",
            WORKSPACE_KEY_EXPRESSION,
            "skill_id",
            "source_kind",
            "source_url",
            "source_subpath",
            "resolved_revision",
            "upstream_revision",
            "content_hash",
            "overlay_hash",
            "effective_version_hash",
            "installed_at",
            "updated_at",
            "detached",
            "forked",
        ],
        identity_columns: &["scope", "workspace_key", "skill_id"],
    },
    LegacyTableMigration {
        legacy: "projection_records_v1",
        current: "projection_records",
        current_columns: &[
            "scope",
            "workspace_key",
            "skill_id",
            "target",
            "generation_mode",
            "physical_root",
            "projected_path",
            "effective_version_hash",
            "generated_at",
        ],
        legacy_expressions: &[
            "scope",
            WORKSPACE_KEY_EXPRESSION,
            "skill_id",
            "target",
            "generation_mode",
            "physical_root",
            "projected_path",
            "effective_version_hash",
            "generated_at",
        ],
        identity_columns: &[
            "scope",
            "workspace_key",
            "skill_id",
            "target",
            "physical_root",
        ],
    },
    LegacyTableMigration {
        legacy: "update_checks_v1",
        current: "update_checks",
        current_columns: &[
            "scope",
            "workspace_key",
            "skill_id",
            "checked_at",
            "pinned_revision",
            "latest_revision",
            "outcome",
            "overlay_detected",
            "local_modification_detected",
            "notes",
        ],
        legacy_expressions: &[
            "scope",
            WORKSPACE_KEY_EXPRESSION,
            "skill_id",
            "checked_at",
            "pinned_revision",
            "latest_revision",
            "outcome",
            "overlay_detected",
            "local_modification_detected",
            "notes",
        ],
        identity_columns: &["scope", "workspace_key", "skill_id", "checked_at"],
    },
    LegacyTableMigration {
        legacy: "local_modifications_v1",
        current: "local_modifications",
        current_columns: &[
            "scope",
            "workspace_key",
            "skill_id",
            "detected_at",
            "kind",
            "path",
            "details",
        ],
        legacy_expressions: &[
            "scope",
            WORKSPACE_KEY_EXPRESSION,
            "skill_id",
            "detected_at",
            "kind",
            "path",
            "details",
        ],
        identity_columns: &[
            "scope",
            "workspace_key",
            "skill_id",
            "detected_at",
            "kind",
            "path",
        ],
    },
    LegacyTableMigration {
        legacy: "pins_v1",
        current: "pins",
        current_columns: &[
            "scope",
            "workspace_key",
            "skill_id",
            "requested_reference",
            "resolved_revision",
            "effective_version_hash",
            "pinned_at",
        ],
        legacy_expressions: &[
            "scope",
            WORKSPACE_KEY_EXPRESSION,
            "skill_id",
            "requested_reference",
            "resolved_revision",
            "effective_version_hash",
            "pinned_at",
        ],
        identity_columns: &["scope", "workspace_key", "skill_id"],
    },
    LegacyTableMigration {
        legacy: "rollback_records_v1",
        current: "rollback_records",
        current_columns: &[
            "scope",
            "workspace_key",
            "skill_id",
            "rolled_back_at",
            "from_reference",
            "to_reference",
        ],
        legacy_expressions: &[
            "scope",
            WORKSPACE_KEY_EXPRESSION,
            "skill_id",
            "rolled_back_at",
            "from_reference",
            "to_reference",
        ],
        identity_columns: &[
            "scope",
            "workspace_key",
            "skill_id",
            "rolled_back_at",
            "from_reference",
            "to_reference",
        ],
    },
    LegacyTableMigration {
        legacy: "history_events_v1",
        current: "history_events",
        current_columns: &[
            "workspace_key",
            "kind",
            "scope",
            "skill_id",
            "target",
            "occurred_at",
            "summary",
            "details_json",
        ],
        legacy_expressions: &[
            WORKSPACE_KEY_EXPRESSION,
            "kind",
            "scope",
            "skill_id",
            "target",
            "occurred_at",
            "summary",
            "details_json",
        ],
        identity_columns: &["workspace_key", "kind", "scope", "skill_id", "occurred_at"],
    },
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
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
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

impl From<ManagedScope> for TargetScope {
    fn from(scope: ManagedScope) -> Self {
        match scope {
            ManagedScope::Workspace => Self::Workspace,
            ManagedScope::User => Self::User,
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
    /// The import is pinned to an immutable commit, which names no upstream
    /// ref an update check could observe.
    CommitPinned,
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
            Self::CommitPinned => "commit-pinned",
            Self::Failed => "failed",
        }
    }

    /// Parse the stable persisted identifier.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "up-to-date" => Some(Self::UpToDate),
            "update-available" => Some(Self::UpdateAvailable),
            "blocked" => Some(Self::Blocked),
            "detached" => Some(Self::Detached),
            "local-source" => Some(Self::LocalSource),
            "commit-pinned" => Some(Self::CommitPinned),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }
}

/// Why one update check held back the apply of the update it observed.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum UpdateCheckBlockReason {
    /// The installed import holds a top-level `scripts/` tree no recorded
    /// review covers.
    InstalledScripts,
    /// The revision waiting upstream adds a top-level `scripts/` tree no
    /// recorded review covers.
    IncomingScripts,
}

impl UpdateCheckBlockReason {
    /// Return the stable persisted identifier.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InstalledScripts => "installed-scripts",
            Self::IncomingScripts => "incoming-scripts",
        }
    }

    /// Parse the stable persisted identifier.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "installed-scripts" => Some(Self::InstalledScripts),
            "incoming-scripts" => Some(Self::IncomingScripts),
            _ => None,
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
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
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
    /// Why the check's trust gate held back the apply of the update it observed.
    ///
    /// The row is the only place the check's look at the upstream tree
    /// survives, because no later reader can see the revision it saw. The
    /// reason describes what the check observed, so a review recorded after
    /// the check can still license the move it held back.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub apply_blocked_by: Option<UpdateCheckBlockReason>,
    /// Optional explanatory note captured with the check.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

impl UpdateCheckRecord {
    /// Fit one recorded check to the revision an install now holds.
    ///
    /// A check observes one moment: it compared the upstream it saw with the
    /// revision pinned then, and the store keeps that observation. A later
    /// apply, `pin`, or `rollback` can move the import onto the very revision
    /// the check named, which leaves the row claiming an update that no longer
    /// waits. The mirror move leaves the import on a revision the check never
    /// certified, which strands an `up-to-date` certification on a revision the
    /// install left. Surfaces that report current state, such as `skillctl
    /// list` and the dashboard, carry this fitted view instead of replaying the
    /// row, so neither direction can contradict the install line. Checks that
    /// still describe the installed revision pass through unchanged.
    pub fn fitted_to(mut self, install: &InstallRecord) -> Self {
        let Some(latest) = self.latest_revision.clone() else {
            // A check that observed no upstream revision cannot name one that
            // arrived or one that waits again.
            return self;
        };
        let install_holds_latest = latest == install.resolved_revision;
        let claims_waiting_update = matches!(
            self.outcome,
            UpdateCheckOutcome::UpdateAvailable | UpdateCheckOutcome::Blocked
        );
        if install_holds_latest && claims_waiting_update {
            self.outcome = UpdateCheckOutcome::UpToDate;
            self.pinned_revision = install.resolved_revision.clone();
            // The revision the gate held back is the revision the install now holds,
            // so the block no longer describes anything an apply could move to.
            self.apply_blocked_by = None;
            self.notes = Some(format!(
                "recorded check named {} as the newest upstream revision; the install already holds it",
                install.resolved_revision
            ));
            return self;
        }

        // The mirror case: the row certified its pinned revision as current,
        // but the install no longer holds that revision, because a `rollback`
        // or a `pin` moved it after the check ran. The certification no longer
        // covers the installed revision, while the row stays the only record
        // of what upstream held. That record names a revision the install does
        // not hold, so it waits again instead of the install reading as up to
        // date for a revision it no longer holds.
        if self.outcome == UpdateCheckOutcome::UpToDate
            && self.pinned_revision != install.resolved_revision
            && !install_holds_latest
        {
            self.outcome = UpdateCheckOutcome::UpdateAvailable;
            self.pinned_revision = install.resolved_revision.clone();
            self.notes = Some(format!(
                "recorded check named {latest} as the newest upstream revision; \
                 the install holds {} instead",
                install.resolved_revision
            ));
        }

        self
    }
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

/// Outcome of one recorded local review decision.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReviewDecision {
    /// The reviewed content is accepted.
    Approved,
    /// A previously recorded approval was withdrawn.
    Revoked,
}

impl ReviewDecision {
    /// Return the stable persisted identifier.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Approved => "approved",
            Self::Revoked => "revoked",
        }
    }
}

/// Current local review decision for one managed imported skill.
///
/// A decision covers exactly one effective version hash, so it stops counting
/// as soon as the import moves to different content.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct TrustReviewRecord {
    /// Managed skill identity.
    pub skill: ManagedSkillRef,
    /// Recorded decision.
    pub decision: ReviewDecision,
    /// Revision that was current when the decision was recorded.
    pub resolved_revision: String,
    /// Effective version hash the decision covers.
    pub effective_version_hash: String,
    /// Whether the reviewed content holds a top-level `scripts/` directory.
    pub contains_scripts: bool,
    /// Timestamp of the decision.
    pub reviewed_at: String,
    /// Optional reviewer note.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
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
    ///
    /// Opening coordinates with other processes because bootstrapping may
    /// create or migrate the shared database.
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

        let mut connection =
            Connection::open(&path).map_err(|source| AppError::LocalStateOpen {
                path: path.clone(),
                source,
            })?;
        configure_connection(&connection, &path)?;
        // Bootstrap decides for itself whether it may write: the unlocked pass
        // validates read-only and reports the schema work it found instead of
        // performing it, so `list`, `history`, `doctor`, and every other read
        // stay available while a lifecycle writer holds the lock, and a
        // database that changed underneath the pass — for example through a
        // concurrent crash-recovery restore — is migrated by a coordinated
        // pass instead of an unlocked one. The SQLite busy timeout covers the
        // unlocked reads.
        match bootstrap_connection(&mut connection, &path, &workspace_key, false)
            .map_err(|error| unreadable_state_database(&path, error))?
        {
            BootstrapOutcome::NeedsCoordination => {
                let _guard = self::lock::acquire(&path, "bootstrap local state")?;
                if let Err(error) =
                    bootstrap_connection(&mut connection, &path, &workspace_key, true)
                {
                    return Err(unreadable_state_database(&path, error));
                }
            }
            BootstrapOutcome::Validated | BootstrapOutcome::Wrote => {}
        }

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
        let mut connection =
            Connection::open_in_memory().map_err(|source| AppError::LocalStateOpen {
                path: path.clone(),
                source,
            })?;
        configure_connection(&connection, &path)?;
        bootstrap_connection(&mut connection, &path, &workspace_key, true)?;

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

    /// Acquire the coordination lock that guards this store.
    ///
    /// In-memory stores have no shared state and coordinate nothing. Acquiring
    /// the lock again inside one process, for example from a lifecycle
    /// transaction that already holds it, returns immediately.
    fn coordinate(&self, purpose: &'static str) -> Result<lock::CoordinationGuard, AppError> {
        lock::acquire(&self.path, purpose)
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
        let _guard = self.coordinate("write install record")?;
        upsert_install_record_in(
            &self.connection,
            &self.path,
            self.workspace_key_for_scope(record.skill.scope),
            record,
        )
    }

    /// List every recorded review decision visible to this workspace.
    pub fn trust_reviews(&self) -> Result<Vec<TrustReviewRecord>, AppError> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT workspace_key, scope, skill_id, decision, resolved_revision, \
                 effective_version_hash, contains_scripts, reviewed_at, note FROM skill_reviews \
                 WHERE (scope = 'workspace' AND workspace_key = ?1) \
                    OR (scope = 'user' AND workspace_key = ?2) \
                 ORDER BY CASE scope WHEN 'workspace' THEN 0 ELSE 1 END, skill_id",
            )
            .map_err(|source| {
                local_state_query(&self.path, "prepare trust review query", source)
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
                        row.get::<_, i64>(6)?,
                        row.get::<_, String>(7)?,
                        row.get::<_, Option<String>>(8)?,
                    ))
                },
            )
            .map_err(|source| local_state_query(&self.path, "query trust reviews", source))?;

        let mut records = Vec::new();
        for row in rows {
            let row =
                row.map_err(|source| local_state_query(&self.path, "read trust review", source))?;
            records.push(decode_trust_review_record(&self.path, row)?);
        }

        Ok(records)
    }

    /// Load the recorded review decision for one managed skill.
    pub fn trust_review(
        &self,
        skill: &ManagedSkillRef,
    ) -> Result<Option<TrustReviewRecord>, AppError> {
        validate_skill_ref(&self.path, skill)?;

        let mut statement = self
            .connection
            .prepare(
                "SELECT workspace_key, scope, skill_id, decision, resolved_revision, \
                 effective_version_hash, contains_scripts, reviewed_at, note FROM skill_reviews \
                 WHERE scope = ?1 AND workspace_key = ?2 AND skill_id = ?3",
            )
            .map_err(|source| {
                local_state_query(&self.path, "prepare trust review query", source)
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
                        row.get::<_, i64>(6)?,
                        row.get::<_, String>(7)?,
                        row.get::<_, Option<String>>(8)?,
                    ))
                },
            )
            .optional()
            .map_err(|source| local_state_query(&self.path, "load trust review", source))?;

        row.map(|record| decode_trust_review_record(&self.path, record))
            .transpose()
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

        if let Some(skill) = skill {
            validate_skill_ref(&self.path, skill)?;
            query_record_list(
                &self.path,
                &mut statement,
                "query projection records",
                "read projection record",
                params![
                    skill.scope.as_str(),
                    self.workspace_key_for_scope(skill.scope),
                    skill.skill_id
                ],
                map_projection_row,
                decode_projection_record,
            )
        } else {
            query_record_list(
                &self.path,
                &mut statement,
                "query projection records",
                "read projection record",
                params![self.workspace_key.as_str(), GLOBAL_WORKSPACE_KEY],
                map_projection_row,
                decode_projection_record,
            )
        }
    }

    /// Insert or update the current projection record for a managed skill.
    pub fn upsert_projection_record(&mut self, record: &ProjectionRecord) -> Result<(), AppError> {
        let _guard = self.coordinate("write projection record")?;
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
        let _guard = self.coordinate("replace projection records")?;
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
            "outcome, overlay_detected, local_modification_detected, apply_blocked_by, notes ",
            "FROM update_checks ",
            "WHERE (scope = 'workspace' AND workspace_key = ?1) ",
            "   OR (scope = 'user' AND workspace_key = ?2) ",
            "ORDER BY checked_at DESC, id DESC"
        );
        let sql_filtered = concat!(
            "SELECT id, workspace_key, scope, skill_id, checked_at, pinned_revision, latest_revision, ",
            "outcome, overlay_detected, local_modification_detected, apply_blocked_by, notes ",
            "FROM update_checks ",
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

        if let Some(skill) = skill {
            validate_skill_ref(&self.path, skill)?;
            query_record_list(
                &self.path,
                &mut statement,
                "query update checks",
                "read update check",
                params![
                    skill.scope.as_str(),
                    self.workspace_key_for_scope(skill.scope),
                    skill.skill_id
                ],
                map_update_check_row,
                decode_update_check_record,
            )
        } else {
            query_record_list(
                &self.path,
                &mut statement,
                "query update checks",
                "read update check",
                params![self.workspace_key.as_str(), GLOBAL_WORKSPACE_KEY],
                map_update_check_row,
                decode_update_check_record,
            )
        }
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
                 outcome, overlay_detected, local_modification_detected, apply_blocked_by, notes \
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
                map_update_check_row,
            )
            .optional()
            .map_err(|source| local_state_query(&self.path, "load latest update check", source))?;

        row.map(|record| decode_update_check_record(&self.path, record))
            .transpose()
    }

    /// Insert one immutable update check row.
    pub fn record_update_check(&mut self, record: &UpdateCheckRecord) -> Result<i64, AppError> {
        let _guard = self.coordinate("write update check record")?;
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

        if let Some(skill) = skill {
            validate_skill_ref(&self.path, skill)?;
            query_record_list(
                &self.path,
                &mut statement,
                "query local modifications",
                "read local modification",
                params![
                    skill.scope.as_str(),
                    self.workspace_key_for_scope(skill.scope),
                    skill.skill_id
                ],
                map_local_modification_row,
                decode_local_modification_record,
            )
        } else {
            query_record_list(
                &self.path,
                &mut statement,
                "query local modifications",
                "read local modification",
                params![self.workspace_key.as_str(), GLOBAL_WORKSPACE_KEY],
                map_local_modification_row,
                decode_local_modification_record,
            )
        }
    }

    /// Insert one immutable local modification record.
    pub fn record_local_modification(
        &mut self,
        record: &LocalModificationRecord,
    ) -> Result<i64, AppError> {
        let _guard = self.coordinate("write local modification record")?;
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
        let _guard = self.coordinate("write pin record")?;
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
        let _guard = self.coordinate("delete current skill state")?;

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
            // The recorded review covers one install's content, so it leaves
            // with that install. The history entry stays as the audit trail.
            connection
                .execute(
                    "DELETE FROM skill_reviews WHERE scope = ?1 AND workspace_key = ?2 AND \
                     skill_id = ?3",
                    params![skill.scope.as_str(), workspace_key.as_str(), skill.skill_id],
                )
                .map_err(|source| local_state_query(path, "delete trust review", source))?;
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

        if let Some(skill) = skill {
            validate_skill_ref(&self.path, skill)?;
            query_record_list(
                &self.path,
                &mut statement,
                "query rollback records",
                "read rollback record",
                params![
                    skill.scope.as_str(),
                    self.workspace_key_for_scope(skill.scope),
                    skill.skill_id
                ],
                map_rollback_row,
                decode_rollback_record,
            )
        } else {
            query_record_list(
                &self.path,
                &mut statement,
                "query rollback records",
                "read rollback record",
                params![self.workspace_key.as_str(), GLOBAL_WORKSPACE_KEY],
                map_rollback_row,
                decode_rollback_record,
            )
        }
    }

    /// Insert one immutable rollback record.
    pub fn record_rollback(&mut self, record: &RollbackRecord) -> Result<i64, AppError> {
        let _guard = self.coordinate("write rollback record")?;
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
        let _guard = self.coordinate("write telemetry settings")?;
        upsert_telemetry_settings_in(&self.connection, &self.path, settings)
    }

    /// Append one immutable history entry to the local ledger.
    pub fn append_history_entry(&mut self, entry: &HistoryEntry) -> Result<i64, AppError> {
        let _guard = self.coordinate("write history entry")?;
        insert_history_entry_in(&self.connection, &self.path, &self.workspace_key, entry)
    }

    /// Remove every current projection record across all scopes.
    pub fn clear_projection_records(&mut self) -> Result<(), AppError> {
        let _guard = self.coordinate("delete projection records")?;
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

        let mut entries = if let Some(skill) = &query.skill {
            validate_skill_ref(&self.path, skill)?;
            query_record_list(
                &self.path,
                &mut statement,
                "query history entries",
                "read history entry",
                params![
                    skill.scope.as_str(),
                    self.workspace_key_for_scope(skill.scope),
                    skill.skill_id
                ],
                map_history_entry_row,
                decode_history_entry,
            )
        } else {
            query_record_list(
                &self.path,
                &mut statement,
                "query history entries",
                "read history entry",
                params![self.workspace_key.as_str(), GLOBAL_WORKSPACE_KEY],
                map_history_entry_row,
                decode_history_entry,
            )
        }?;

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
    ///
    /// The write runs while the coordination lock is held, the way every other
    /// store mutation does, so a check-mode update that records its rows
    /// outside a lifecycle transaction still coordinates with a guarded
    /// writer. A lifecycle transaction that already holds the lock reenters
    /// it, so callers inside `run_guarded` keep working unchanged.
    pub(crate) fn with_transaction<T>(
        &mut self,
        operation: &'static str,
        work: impl FnOnce(&Connection, &Path) -> Result<T, AppError>,
    ) -> Result<T, AppError> {
        let _guard = self.coordinate(operation)?;
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

/// Run one prepared record-list query and decode every row it returns.
///
/// Every list method shares this loop for its all-rows and filtered
/// statements, so the two statements of one list map and decode rows through
/// the same functions instead of drifting copies of one closure.
fn query_record_list<Row, Record>(
    path: &Path,
    statement: &mut rusqlite::Statement<'_>,
    query_action: &'static str,
    read_action: &'static str,
    params: &[&dyn rusqlite::ToSql],
    map_row: fn(&rusqlite::Row<'_>) -> rusqlite::Result<Row>,
    decode: fn(&Path, Row) -> Result<Record, AppError>,
) -> Result<Vec<Record>, AppError> {
    let rows = statement
        .query_map(params, map_row)
        .map_err(|source| local_state_query(path, query_action, source))?;

    let mut records = Vec::new();
    for row in rows {
        let row = row.map_err(|source| local_state_query(path, read_action, source))?;
        records.push(decode(path, row)?);
    }

    Ok(records)
}

/// Rewrite a store failure caused by an unreadable database file.
///
/// SQLite reports a corrupt or foreign file as `file is not a database` or a
/// malformed disk image. Those failures have one documented recovery, so they
/// carry the actionable message instead of the raw SQLite text.
fn unreadable_state_database(path: &Path, error: AppError) -> AppError {
    let (AppError::LocalStateOpen {
        source: sqlite_error,
        ..
    }
    | AppError::LocalStateQuery {
        source: sqlite_error,
        ..
    }) = &error
    else {
        return error;
    };

    if schema::is_unreadable_database_error(sqlite_error) {
        AppError::LocalStateDatabaseUnreadable {
            path: path.to_path_buf(),
            reason: sqlite_error.to_string(),
        }
    } else {
        error
    }
}

/// Apply the connection settings every state connection shares.
///
/// The busy timeout is the one wait that keeps uncoordinated connections
/// working while a coordinated writer commits. It covers readers such as
/// `list` and `history`, which take no coordination lock, and writers blocked
/// by an external reader that holds a shared lock.
pub(crate) fn configure_connection(connection: &Connection, path: &Path) -> Result<(), AppError> {
    connection
        .busy_timeout(state_busy_timeout())
        .map_err(|source| AppError::LocalStateQuery {
            path: path.to_path_buf(),
            operation: "set state busy timeout",
            source,
        })
}

/// Resolve the configured SQLite busy timeout for state connections.
fn state_busy_timeout() -> Duration {
    state_busy_timeout_from_env(env::var(STATE_BUSY_TIMEOUT_ENV).ok().as_deref())
}

/// Resolve one `SKILLCTL_STATE_BUSY_TIMEOUT_MS` value.
///
/// A missing or invalid value falls back to the default, matching how the
/// coordination lock treats `SKILLCTL_LOCK_TIMEOUT_MS`.
fn state_busy_timeout_from_env(value: Option<&str>) -> Duration {
    env_millisecond_timeout(value, DEFAULT_STATE_BUSY_TIMEOUT)
}

/// Resolve one environment-sourced millisecond timeout.
///
/// A missing or invalid value falls back to `default`. A configured zero is
/// clamped to one millisecond, so a zero timeout always means one retry
/// instead of a wait that ends before it starts. Every millisecond timeout
/// the crate reads from the environment goes through this parser.
pub(crate) fn env_millisecond_timeout(value: Option<&str>, default: Duration) -> Duration {
    value
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map_or(default, |milliseconds| {
            Duration::from_millis(milliseconds.max(1))
        })
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

/// Acquire the coordination lock that guards one optional state database.
///
/// A missing database path coordinates nothing.
pub(crate) fn acquire_coordination_lock(
    database_path: Option<&Path>,
    purpose: &'static str,
) -> Result<CoordinationGuard, AppError> {
    lock::acquire(
        database_path.unwrap_or_else(|| Path::new(":memory:")),
        purpose,
    )
}

/// Resolve the root that holds lifecycle journals for the default store.
///
/// A missing home directory falls back to a temporary root so lifecycle
/// operations still journal their work instead of running unrecorded.
pub fn default_journal_root() -> PathBuf {
    default_state_root().unwrap_or_else(|_| env::temp_dir().join("skillctl-journal"))
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
                record.source_kind.label(),
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
             latest_revision, outcome, overlay_detected, local_modification_detected, \
             apply_blocked_by, notes) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
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
                record.apply_blocked_by.map(UpdateCheckBlockReason::as_str),
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

pub(crate) fn upsert_trust_review_in(
    connection: &Connection,
    path: &Path,
    workspace_key: &str,
    record: &TrustReviewRecord,
) -> Result<(), AppError> {
    validate_trust_review_record(path, record)?;
    connection
        .execute(
            "INSERT INTO skill_reviews (scope, workspace_key, skill_id, decision, \
             resolved_revision, effective_version_hash, contains_scripts, reviewed_at, note) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) \
             ON CONFLICT(scope, workspace_key, skill_id) DO UPDATE SET \
             decision = excluded.decision, \
             resolved_revision = excluded.resolved_revision, \
             effective_version_hash = excluded.effective_version_hash, \
             contains_scripts = excluded.contains_scripts, \
             reviewed_at = excluded.reviewed_at, \
             note = excluded.note",
            params![
                record.skill.scope.as_str(),
                workspace_key,
                record.skill.skill_id,
                record.decision.as_str(),
                record.resolved_revision,
                record.effective_version_hash,
                bool_to_int(record.contains_scripts),
                record.reviewed_at,
                record.note,
            ],
        )
        .map_err(|source| local_state_query(path, "write trust review", source))?;

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

/// Preview of one journal that never reached a terminal state.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PendingOperationView {
    /// Journal identifier.
    pub id: String,
    /// Lifecycle operation that created the journal.
    pub operation: String,
    /// Journal state: `active` or `needs-review`.
    pub state: String,
    /// Absolute workspace path recorded when the journal was created.
    pub workspace_path: String,
    /// Process identifier that created the journal.
    pub pid: u32,
    /// Creation timestamp.
    pub started_at: String,
    /// Journal boundaries reached before the process stopped.
    pub steps: Vec<String>,
    /// Paths the journal can restore.
    pub tracked_paths: Vec<String>,
}

/// Preview of one legacy table that still exists beside the current schema.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct LegacyTableView {
    /// Legacy table name.
    pub legacy: String,
    /// Current table name.
    pub current: String,
    /// Rows held by the legacy table.
    pub legacy_rows: i64,
    /// Rows held by the current table.
    pub current_rows: i64,
    /// `copy-and-drop`, `drop-only`, or `ambiguous`.
    pub action: String,
    /// Why the table cannot be recovered automatically, when it cannot.
    pub detail: String,
}

/// Result of inspecting or repairing recoverable local state.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct StateRecoveryReport {
    /// Schema version of the state database, `0` when it does not exist.
    pub schema_version: u32,
    /// Whether the state database file exists.
    pub state_database_exists: bool,
    /// Why SQLite cannot read the state database, when it cannot.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state_database_unreadable: Option<String>,
    /// Journals that never reached a terminal state.
    pub pending_operations: Vec<PendingOperationView>,
    /// Legacy tables that still exist beside the current schema.
    pub legacy_tables: Vec<LegacyTableView>,
    /// Retained state database backups.
    pub migration_backups: Vec<String>,
    /// Repairs that were applied.
    pub repairs: Vec<String>,
    /// Findings that need a human decision.
    pub unresolved: Vec<String>,
}

impl StateRecoveryReport {
    /// Whether anything was found that a repair could act on or report.
    pub fn has_findings(&self) -> bool {
        self.state_database_unreadable.is_some()
            || !self.pending_operations.is_empty()
            || !self.legacy_tables.is_empty()
    }
}

/// Inspect recoverable local state without changing anything.
pub fn inspect_state_recovery(working_directory: &Path) -> Result<StateRecoveryReport, AppError> {
    build_recovery_report(working_directory, StateRecoveryReport::default(), false)
}

/// Preview or apply local state recovery.
///
/// With `apply` set, the report describes the repairs that were performed.
/// Repairs only act where row ownership and reconciliation are unambiguous;
/// every other finding is reported in `unresolved`.
pub fn repair_state(
    working_directory: &Path,
    apply: bool,
) -> Result<StateRecoveryReport, AppError> {
    if !apply {
        return inspect_state_recovery(working_directory);
    }

    let mut report = StateRecoveryReport::default();
    let database_path = default_state_database_path()?;
    let state_root = default_state_root()?;

    // An unreadable database is moved aside before journal recovery. A journal
    // that carries a database snapshot can then restore its rows into the
    // rebuilt store, while recovering first would fail on the corrupt file and
    // leave the journal, and its snapshot, stranded in `needs-review`.
    if let schema::DatabaseHealth::Unreadable { reason } = schema::database_health(&database_path) {
        report.state_database_unreadable = Some(reason);
        quarantine_unreadable_state_database(&database_path, working_directory, &mut report)?;
    }

    // Restore interrupted operations so later schema work sees the state those
    // operations started from.
    {
        let _guard = lock::acquire(&database_path, "recover local state")?;
        for outcome in journal::recover_pending_operations(&state_root)? {
            match outcome {
                RecoveryOutcome::Recovered(operation) => report.repairs.push(format!(
                    "restored the pre-operation state of interrupted '{}' (journal {})",
                    operation.operation, operation.id
                )),
                RecoveryOutcome::NeedsReview { id, reason } => {
                    report.unresolved.push(format!(
                        "journal {id} could not be restored automatically: {reason}"
                    ));
                }
                RecoveryOutcome::Unreadable { directory, reason } => {
                    report.unresolved.push(format!(
                        "journal {} is unreadable: {reason}",
                        crate::planner::portable_path(&directory)
                    ));
                }
            }
        }
    }

    build_recovery_report(working_directory, report, true)
}

/// Journal state of an operation that has not finished or failed yet.
///
/// Mirrors `JournalState::Active.as_str()`, which stays private to the journal
/// module.
const ACTIVE_JOURNAL_STATE: &str = "active";

fn build_recovery_report(
    working_directory: &Path,
    mut report: StateRecoveryReport,
    apply: bool,
) -> Result<StateRecoveryReport, AppError> {
    let database_path = default_state_database_path()?;
    let state_root = default_state_root()?;
    let workspace_key = workspace_key_for_path(working_directory)?;

    report.state_database_exists = database_path.exists();
    report.migration_backups = journal::migration_backups(&state_root);

    // This inspection holds no coordination lock, so an active journal may
    // belong to a live lifecycle transaction: a journal exists on disk only
    // while its owner holds the coordination lock. The lock is asked after the
    // journals were read, so an operation that starts between the two steps is
    // seen holding the lock. An active journal under a held lock is an
    // operation in progress, not an interrupted one, and stays unreported;
    // needs-review journals always belong to an operation that finished, so
    // they stay reported.
    let pending = journal::pending_operations(&state_root)?;
    let operation_in_progress = pending
        .iter()
        .any(|operation| operation.state == ACTIVE_JOURNAL_STATE)
        && lock::is_held(&database_path);
    report.pending_operations = pending
        .into_iter()
        .filter(|operation| !operation_in_progress || operation.state != ACTIVE_JOURNAL_STATE)
        .map(|operation| PendingOperationView {
            id: operation.id,
            operation: operation.operation,
            state: operation.state,
            workspace_path: operation.workspace_path,
            pid: operation.pid,
            started_at: operation.started_at,
            steps: operation.steps,
            tracked_paths: operation.tracked_paths,
        })
        .collect();

    if report.state_database_exists {
        // A file that is not a readable database has one recovery: move it
        // aside and let the next command rebuild local state. It is reported
        // as its own finding instead of failing the inspection, so the
        // recovery entry point still answers.
        let health = schema::database_health(&database_path);
        if let schema::DatabaseHealth::Unreadable { reason } = &health {
            report.state_database_unreadable = Some(reason.clone());
        }

        match health {
            schema::DatabaseHealth::Unreadable { .. } => {
                if apply {
                    quarantine_unreadable_state_database(
                        &database_path,
                        working_directory,
                        &mut report,
                    )?;
                }
            }
            schema::DatabaseHealth::Missing => {}
            schema::DatabaseHealth::Readable => {
                inspect_and_repair_database(
                    &database_path,
                    &workspace_key,
                    working_directory,
                    apply,
                    &mut report,
                )?;
            }
        }
    } else if apply {
        report
            .repairs
            .push("state database does not exist; the next command creates it".to_string());
    }

    report.unresolved.extend(
        report
            .legacy_tables
            .iter()
            .filter(|table| table.action == "ambiguous")
            .map(|table| {
                format!(
                    "legacy table {} cannot be attributed to one workspace: {}",
                    table.legacy, table.detail
                )
            }),
    );

    Ok(report)
}

/// Inspect a readable database and, with `apply`, run the ordinary repairs.
fn inspect_and_repair_database(
    database_path: &Path,
    workspace_key: &str,
    working_directory: &Path,
    apply: bool,
    report: &mut StateRecoveryReport,
) -> Result<(), AppError> {
    let inspection = schema::inspect_database(database_path, workspace_key)?;
    report.schema_version = inspection.version;
    for disposition in inspection.dispositions {
        let (action, detail) = match &disposition.action {
            LegacyTableAction::CopyAndDrop { missing, identical } => (
                "copy-and-drop".to_string(),
                format!("{missing} row(s) to copy, {identical} already present"),
            ),
            LegacyTableAction::DropOnly { identical } => (
                "drop-only".to_string(),
                format!("{identical} row(s) already migrated"),
            ),
            LegacyTableAction::Ambiguous { reason } => ("ambiguous".to_string(), reason.clone()),
        };
        report.legacy_tables.push(LegacyTableView {
            legacy: disposition.legacy.to_string(),
            current: disposition.current.to_string(),
            legacy_rows: disposition.legacy_rows,
            current_rows: disposition.current_rows,
            action,
            detail,
        });
    }

    if apply {
        apply_database_repairs(database_path, working_directory, report)?;
    }

    Ok(())
}

/// Move an unreadable state database aside and rebuild an empty one.
///
/// Nothing is deleted. The corrupt file is renamed beside the database so it
/// stays available for inspection, and its write-ahead sidecars move with it
/// because SQLite refuses a fresh database that still carries a stale sidecar.
fn quarantine_unreadable_state_database(
    database_path: &Path,
    working_directory: &Path,
    report: &mut StateRecoveryReport,
) -> Result<(), AppError> {
    let _guard = lock::acquire(database_path, "quarantine unreadable state database")?;
    let state_root = match database_path.parent() {
        Some(parent) => parent.to_path_buf(),
        None => default_state_root()?,
    };

    let stamp = journal::coarse_timestamp().replace('.', "-");
    let mut quarantined = state_root.join(format!("state.db.corrupt.{stamp}"));
    let mut attempt = 1;
    while quarantined.exists() {
        attempt += 1;
        quarantined = state_root.join(format!("state.db.corrupt.{stamp}-{attempt}"));
    }

    fs::rename(database_path, &quarantined).map_err(|source| AppError::FilesystemOperation {
        action: "move the unreadable state database aside",
        path: database_path.to_path_buf(),
        source,
    })?;
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut sidecar = database_path.as_os_str().to_os_string();
        sidecar.push(suffix);
        let sidecar = PathBuf::from(sidecar);
        if !sidecar.exists() {
            continue;
        }
        let mut moved = quarantined.as_os_str().to_os_string();
        moved.push(suffix);
        fs::rename(&sidecar, moved).map_err(|source| AppError::FilesystemOperation {
            action: "move a sidecar of the unreadable state database aside",
            path: sidecar,
            source,
        })?;
    }

    let store = LocalStateStore::open_at_for(database_path.to_path_buf(), working_directory)?;
    report.schema_version = store.schema_version()?;
    report.repairs.push(format!(
        "moved the unreadable state database to '{}' and rebuilt an empty local state store; run \
         'skillctl restore --frozen' to rebuild the import records from .agents/skillctl.yaml \
         and .agents/skillctl.lock",
        crate::planner::portable_path(&quarantined)
    ));

    Ok(())
}

/// Run the ordinary bootstrap path under the coordination lock.
///
/// This performs the same migration and stranded-state recovery as a normal
/// store open. Failures are recorded as unresolved findings instead of
/// aborting the report so a partially damaged database still produces guidance.
fn apply_database_repairs(
    database_path: &Path,
    working_directory: &Path,
    report: &mut StateRecoveryReport,
) -> Result<(), AppError> {
    let _guard = lock::acquire(database_path, "repair local state")?;
    match LocalStateStore::open_at_for(database_path.to_path_buf(), working_directory) {
        Ok(_) => {
            report
                .repairs
                .push("verified the state database opens with the current schema".to_string());
            report.legacy_tables.clear();
        }
        Err(error) => report
            .unresolved
            .push(format!("state database could not be opened: {error}")),
    }

    Ok(())
}

fn local_state_query(path: &Path, operation: &'static str, source: rusqlite::Error) -> AppError {
    if schema::is_busy_timeout_error(&source) {
        return state_busy_timeout_failure(path, operation, source);
    }

    AppError::LocalStateQuery {
        path: path.to_path_buf(),
        operation,
        source,
    }
}

/// Build the failure a connection meets when a lock outlasts the busy timeout.
///
/// The timeout reported here is resolved again from the environment, which is
/// the same value `configure_connection` applied to the connection, so the
/// message states the wait the command really used.
fn state_busy_timeout_failure(
    path: &Path,
    operation: &'static str,
    source: rusqlite::Error,
) -> AppError {
    AppError::LocalStateBusyTimeout {
        path: path.to_path_buf(),
        operation,
        timeout_ms: state_busy_timeout().as_millis() as u64,
        reason: source.to_string(),
    }
}

fn local_state_validation(path: &Path, message: impl Into<String>) -> AppError {
    AppError::LocalStateValidation {
        path: path.to_path_buf(),
        message: message.into(),
    }
}

#[cfg(test)]
mod bootstrap_coordination_tests {
    use super::*;

    #[test]
    fn a_validated_current_schema_bootstraps_without_coordination() {
        let temp = tempfile::tempdir().expect("tempdir exists");
        let database = temp.path().join("state.db");
        // Opening the store bootstraps the schema to the current version with
        // no stranded legacy tables, which is the state every reader meets.
        LocalStateStore::open_at(&database).expect("store opens");

        let mut connection = Connection::open(&database).expect("database opens");
        assert_eq!(
            bootstrap_connection(&mut connection, &database, "workspace", false)
                .expect("unlocked bootstrap succeeds"),
            BootstrapOutcome::Validated,
            "a current schema with no stranded tables bootstraps read-only"
        );
    }

    #[test]
    fn a_missing_schema_defers_to_a_coordinated_bootstrap() {
        let temp = tempfile::tempdir().expect("tempdir exists");
        let database = temp.path().join("state.db");
        let mut connection = Connection::open(&database).expect("empty database opens");

        assert_eq!(
            bootstrap_connection(&mut connection, &database, "workspace", false)
                .expect("unlocked bootstrap succeeds"),
            BootstrapOutcome::NeedsCoordination,
            "a database without a version marker reports the schema creation it \
             may not perform unlocked"
        );
        assert_eq!(
            bootstrap_connection(&mut connection, &database, "workspace", true)
                .expect("coordinated bootstrap succeeds"),
            BootstrapOutcome::Wrote,
            "a coordinated bootstrap creates the schema"
        );
    }

    #[test]
    fn an_outdated_schema_defers_instead_of_migrating_unlocked() {
        let temp = tempfile::tempdir().expect("tempdir exists");
        let database = temp.path().join("state.db");
        LocalStateStore::open_at(&database).expect("store opens");

        // The window the unlock probe used to miss: a concurrent
        // crash-recovery restore can rewrite the committed schema version
        // between two readings, so the unlocked pass must find the older
        // version and refuse to write it, not migrate it.
        let mut connection = Connection::open(&database).expect("database opens");
        connection
            .pragma_update(None, "user_version", 3u32)
            .expect("schema version drops");

        assert_eq!(
            bootstrap_connection(&mut connection, &database, "workspace", false)
                .expect("unlocked bootstrap succeeds"),
            BootstrapOutcome::NeedsCoordination,
            "an outdated schema reports the migration it may not perform unlocked"
        );
        let found = schema_version(&connection, &database).expect("version reads");
        assert_eq!(
            found, 3,
            "the unlocked bootstrap leaves the older version untouched"
        );

        assert_eq!(
            bootstrap_connection(&mut connection, &database, "workspace", true)
                .expect("coordinated bootstrap succeeds"),
            BootstrapOutcome::Wrote,
            "a coordinated bootstrap migrates the schema"
        );
    }

    #[test]
    fn stranded_legacy_tables_defer_to_a_coordinated_bootstrap() {
        let temp = tempfile::tempdir().expect("tempdir exists");
        let database = temp.path().join("state.db");
        LocalStateStore::open_at(&database).expect("store opens");

        let mut connection = Connection::open(&database).expect("database opens");
        connection
            .execute("CREATE TABLE install_records_v1 (placeholder TEXT)", [])
            .expect("stranded legacy table appears");

        assert_eq!(
            bootstrap_connection(&mut connection, &database, "workspace", false)
                .expect("unlocked bootstrap succeeds"),
            BootstrapOutcome::NeedsCoordination,
            "stranded legacy tables report the recovery they need"
        );
        let stranded: bool = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'install_records_v1')",
                [],
                |row| row.get(0),
            )
            .expect("table presence reads");
        assert!(
            stranded,
            "the unlocked bootstrap leaves the stranded table in place"
        );
    }

    #[test]
    fn an_unsupported_schema_is_refused_read_only() {
        let temp = tempfile::tempdir().expect("tempdir exists");
        let database = temp.path().join("state.db");
        let mut connection = Connection::open(&database).expect("database opens");
        connection
            .execute_batch(
                "CREATE TABLE placeholder (placeholder TEXT); \
                 PRAGMA user_version = 99;",
            )
            .expect("unsupported database is prepared");

        let error = bootstrap_connection(&mut connection, &database, "workspace", false)
            .expect_err("an unsupported version is refused by validation");
        assert!(
            matches!(error, AppError::LocalStateValidation { .. }),
            "the refusal is a validation failure: {error}"
        );
    }
}
