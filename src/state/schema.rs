//! Schema initialization, migration, and stranded-state reconciliation.
//!
//! Every schema change runs inside one SQLite transaction: the version marker
//! advances in the same transaction that renames legacy tables, creates the new
//! schema, copies rows, and removes the legacy copies. An interrupted migration
//! therefore leaves the previous schema and its rows untouched instead of a
//! database that looks current but holds no data.

use std::{
    collections::BTreeSet,
    env,
    path::{Path, PathBuf},
};

use rusqlite::{Connection, params};

use super::{
    GLOBAL_WORKSPACE_KEY, LEGACY_TABLE_MIGRATIONS, LOCAL_STATE_SCHEMA_POLICY, LegacyTableMigration,
    REQUIRED_TABLES, VersionDisposition, journal, local_state_query, local_state_validation,
};
use crate::error::AppError;

/// Environment variable that injects a failure at one migration boundary.
pub(crate) const MIGRATION_FAILPOINT_ENV: &str = "SKILLCTL_MIGRATION_FAILPOINT";

/// Boundaries of a schema migration, in execution order.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MigrationStage {
    /// Before any migration statement runs.
    Start,
    /// After legacy tables were renamed to their `_v1` names.
    AfterRenames,
    /// After the current schema objects were created without a version bump.
    AfterSchemaCreation,
    /// After legacy rows were copied into the current tables.
    AfterDataCopies,
    /// After the legacy `_v1` tables were dropped.
    AfterLegacyCleanup,
    /// After the schema version marker advanced, before the commit.
    AfterVersionAdvance,
}

impl MigrationStage {
    /// Stable identifier used by the failpoint environment variable.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::AfterRenames => "after-renames",
            Self::AfterSchemaCreation => "after-schema-creation",
            Self::AfterDataCopies => "after-data-copies",
            Self::AfterLegacyCleanup => "after-legacy-cleanup",
            Self::AfterVersionAdvance => "after-version-advance",
        }
    }
}

/// Every migration boundary in execution order.
#[cfg_attr(not(test), expect(dead_code, reason = "used by migration tests"))]
pub(crate) const MIGRATION_STAGES: &[MigrationStage] = &[
    MigrationStage::Start,
    MigrationStage::AfterRenames,
    MigrationStage::AfterSchemaCreation,
    MigrationStage::AfterDataCopies,
    MigrationStage::AfterLegacyCleanup,
    MigrationStage::AfterVersionAdvance,
];

/// Hook invoked at each migration boundary.
///
/// Interruption tests use the hook to fail at one boundary and prove that the
/// previous state survives. The default hook reads
/// `SKILLCTL_MIGRATION_FAILPOINT` so integration tests can drive the same
/// boundaries in a child process.
pub(crate) type MigrationHook<'a> = &'a dyn Fn(MigrationStage) -> Result<(), AppError>;

/// What one bootstrap pass did about schema work.
///
/// The outcome is the coordination contract between a store and its caller: a
/// pass that only reads reports [`Validated`](Self::Validated), a pass that
/// wrote reports [`Wrote`](Self::Wrote), and a pass that found work it may not
/// perform unlocked reports [`NeedsCoordination`](Self::NeedsCoordination) so
/// its caller retries while holding the coordination lock.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum BootstrapOutcome {
    /// The pass validated the schema without writing.
    Validated,
    /// The pass created, migrated, or recovered the schema.
    Wrote,
    /// The pass found schema work only a coordinated pass may perform.
    NeedsCoordination,
}

/// Bootstrap one connection's schema.
///
/// The pass classifies the schema it finds and acts on that one reading: with
/// `allow_writes` clear it refuses to create, migrate, or recover anything and
/// reports [`BootstrapOutcome::NeedsCoordination`] instead, so an unlocked
/// bootstrap can never write no matter how the shared database changed around
/// it. Callers that hold the coordination lock pass `allow_writes` set.
pub(super) fn bootstrap_connection(
    connection: &mut Connection,
    path: &Path,
    workspace_key: &str,
    allow_writes: bool,
) -> Result<BootstrapOutcome, AppError> {
    connection
        .execute_batch("PRAGMA foreign_keys = ON;")
        .map_err(|source| local_state_query(path, "enable foreign keys", source))?;
    initialize_or_validate_schema(connection, path, workspace_key, allow_writes)
}

fn initialize_or_validate_schema(
    connection: &mut Connection,
    path: &Path,
    workspace_key: &str,
    allow_writes: bool,
) -> Result<BootstrapOutcome, AppError> {
    let found = schema_version(connection, path)?;
    if found == 0 {
        if database_has_user_tables(connection, path)? {
            return Err(local_state_validation(
                path,
                "schema version is missing but user tables already exist",
            ));
        }

        if !allow_writes {
            return Ok(BootstrapOutcome::NeedsCoordination);
        }
        create_current_schema(connection, path)?;
        return Ok(BootstrapOutcome::Wrote);
    }

    match LOCAL_STATE_SCHEMA_POLICY.classify(found) {
        VersionDisposition::Current => {
            // The stranded-table recovery writes only when a legacy table
            // exists beside the current schema, so its presence decides
            // whether this pass may stay read-only.
            let stranded = !present_legacy_tables(connection, path)?.is_empty();
            if stranded && !allow_writes {
                return Ok(BootstrapOutcome::NeedsCoordination);
            }
            recover_stranded_legacy_tables(connection, path, workspace_key, found)?;
            validate_required_tables(connection, path)?;
            if stranded {
                Ok(BootstrapOutcome::Wrote)
            } else {
                Ok(BootstrapOutcome::Validated)
            }
        }
        VersionDisposition::NeedsMigration { from, to } => {
            if !allow_writes {
                return Ok(BootstrapOutcome::NeedsCoordination);
            }
            migrate_schema(
                connection,
                path,
                workspace_key,
                from,
                to,
                &env_migration_hook,
            )?;
            validate_required_tables(connection, path)?;
            Ok(BootstrapOutcome::Wrote)
        }
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

/// Create the current schema and its version marker in one transaction.
fn create_current_schema(connection: &mut Connection, path: &Path) -> Result<(), AppError> {
    let transaction = connection
        .transaction()
        .map_err(|source| local_state_query(path, "begin schema creation", source))?;
    transaction
        .execute_batch(&local_state_tables_sql())
        .map_err(|source| local_state_query(path, "initialize local state schema", source))?;
    advance_schema_version(&transaction, path, LOCAL_STATE_SCHEMA_POLICY.current())?;
    transaction
        .commit()
        .map_err(|source| local_state_query(path, "commit schema creation", source))
}

/// Run the `from` to `to` migration inside one SQLite transaction.
///
/// The hook receives every boundary in [`MIGRATION_STAGES`] order. A hook error
/// aborts the migration, the transaction rolls back, and the database keeps the
/// `from` schema with its rows.
///
/// Version 1 predates workspace namespacing, so its tables are renamed, copied
/// into the current tables, and dropped. Later versions are already namespaced,
/// so they migrate additively: missing tables are created and tables whose
/// constraints gained values are rebuilt.
pub(crate) fn migrate_schema(
    connection: &mut Connection,
    path: &Path,
    workspace_key: &str,
    from: u32,
    to: u32,
    hook: MigrationHook<'_>,
) -> Result<(), AppError> {
    if from == 0
        || from >= to
        || to != LOCAL_STATE_SCHEMA_POLICY.current()
        || from > 1 && !ADDITIVE_MIGRATION_SUPPORTED_VERSIONS.contains(&from)
    {
        return Err(local_state_validation(
            path,
            format!("schema version {from} requires migration to {to}"),
        ));
    }

    hook(MigrationStage::Start)?;
    journal::record_migration_backup(path, from, to)?;

    let transaction = connection
        .transaction()
        .map_err(|source| local_state_query(path, "begin schema migration", source))?;

    if from == 1 {
        for (legacy_name, migrated_name) in LEGACY_RENAMES {
            rename_legacy_table_if_needed(&transaction, path, legacy_name, migrated_name)?;
        }
        // Renaming a table keeps its indexes attached under their original
        // names, so the `IF NOT EXISTS` creates below would skip those names
        // and `drop_legacy_tables` would remove the indexes with the `_v1`
        // tables. Drop the names first so the current schema recreates every
        // index.
        let drop_legacy_indexes = LEGACY_SECONDARY_INDEXES
            .iter()
            .map(|index| format!("DROP INDEX IF EXISTS {index};"))
            .collect::<Vec<_>>()
            .concat();
        transaction
            .execute_batch(&drop_legacy_indexes)
            .map_err(|source| local_state_query(path, "drop legacy schema indexes", source))?;
        hook(MigrationStage::AfterRenames)?;
    }

    transaction
        .execute_batch(&local_state_tables_sql())
        .map_err(|source| {
            local_state_query(path, "initialize migrated local state schema", source)
        })?;
    if from > 1 {
        rebuild_widened_constraint_tables(&transaction, path, from)?;
    }
    hook(MigrationStage::AfterSchemaCreation)?;

    if from == 1 {
        let reconciliation = reconcile_legacy_tables(&transaction, path, workspace_key)?;
        if !reconciliation.ambiguous.is_empty() {
            return Err(local_state_validation(
                path,
                format!(
                    "legacy tables {} hold rows whose owning workspace cannot be determined; \
                     run 'skillctl doctor --repair-state' to preview recovery",
                    reconciliation.ambiguous.join(", ")
                ),
            ));
        }
        hook(MigrationStage::AfterDataCopies)?;

        drop_legacy_tables(&transaction, path)?;
        hook(MigrationStage::AfterLegacyCleanup)?;
    }

    advance_schema_version(&transaction, path, to)?;
    hook(MigrationStage::AfterVersionAdvance)?;

    transaction
        .commit()
        .map_err(|source| local_state_query(path, "commit schema migration", source))
}

/// Oldest schema version that already carries the workspace key columns.
///
/// Every version from here to the current one migrates additively instead of
/// through the schema-v1 rename and copy flow.
const ADDITIVE_MIGRATION_SUPPORTED_VERSIONS: &[u32] = &[2, 3, 4, 5];

/// One table the additive migration rebuilds from the canonical schema.
struct WidenedConstraintTable {
    /// Table name in the current schema.
    table: &'static str,
    /// Secondary index the canonical schema attaches to the table.
    index: Option<&'static str>,
    /// Columns the supported older versions carry, each paired with the first
    /// schema version whose table carries it, so the copy can skip a column
    /// the migrated-from version does not have yet.
    columns: &'static [(&'static str, u32)],
}

/// Tables the additive migration rebuilds.
///
/// A rebuild delivers each shape change an older table cannot hold in place: a
/// `CHECK` constraint list that gained values and a column that was added.
/// The copy names only the columns the older table carries, so a column the
/// canonical schema added keeps its default instead of losing rows.
///
/// `install_records` carries no secondary index of its own, so its rebuild has
/// none to drop.
const WIDENED_CONSTRAINT_TABLES: &[WidenedConstraintTable] = &[
    WidenedConstraintTable {
        table: "history_events",
        index: Some("idx_history_events_skill_time"),
        columns: &[
            ("id", 2),
            ("workspace_key", 2),
            ("kind", 2),
            ("scope", 2),
            ("skill_id", 2),
            ("target", 2),
            ("occurred_at", 2),
            ("summary", 2),
            ("details_json", 2),
        ],
    },
    WidenedConstraintTable {
        table: "update_checks",
        index: Some("idx_update_checks_skill_time"),
        columns: &[
            ("id", 2),
            ("scope", 2),
            ("workspace_key", 2),
            ("skill_id", 2),
            ("checked_at", 2),
            ("pinned_revision", 2),
            ("latest_revision", 2),
            ("outcome", 2),
            ("overlay_detected", 2),
            ("local_modification_detected", 2),
            ("apply_blocked_by", 5),
            ("notes", 2),
        ],
    },
    WidenedConstraintTable {
        table: "install_records",
        index: None,
        columns: &[
            ("scope", 2),
            ("workspace_key", 2),
            ("skill_id", 2),
            ("source_kind", 2),
            ("source_url", 2),
            ("source_subpath", 2),
            ("resolved_revision", 2),
            ("upstream_revision", 2),
            ("content_hash", 2),
            ("overlay_hash", 2),
            ("effective_version_hash", 2),
            ("installed_at", 2),
            ("updated_at", 2),
            ("detached", 2),
            ("forked", 2),
        ],
    },
];

/// Rebuild every table whose canonical shape changed since the version being
/// migrated from.
///
/// SQLite cannot widen a `CHECK` constraint in place, so the additive migration
/// moves the rows into a fresh table created from the canonical schema text,
/// which keeps the rebuilt shape and the created shape from drifting. The copy
/// names only the columns the older table carries, so a column the canonical
/// schema added keeps its default instead of losing rows.
fn rebuild_widened_constraint_tables(
    connection: &Connection,
    path: &Path,
    from: u32,
) -> Result<(), AppError> {
    for widened in WIDENED_CONSTRAINT_TABLES {
        let table = widened.table;
        let column_list = widened
            .columns
            .iter()
            .filter(|(_, since)| *since <= from)
            .map(|(column, _)| *column)
            .collect::<Vec<_>>()
            .join(", ");
        let drop_index = widened
            .index
            .map(|index| format!("DROP INDEX IF EXISTS {index};\n"))
            .unwrap_or_default();
        connection
            .execute_batch(&format!(
                "{drop_index}\
                 DROP TABLE IF EXISTS {table}_migrated;
                 ALTER TABLE {table} RENAME TO {table}_migrated;"
            ))
            .map_err(|source| local_state_query(path, "stage constraint table rebuild", source))?;
        connection
            .execute_batch(&local_state_tables_sql())
            .map_err(|source| local_state_query(path, "recreate constraint tables", source))?;
        connection
            .execute_batch(&format!(
                "INSERT INTO {table} ({column_list})
                 SELECT {column_list}
                   FROM {table}_migrated;
                 DROP TABLE {table}_migrated;"
            ))
            .map_err(|source| {
                local_state_query(path, "rebuild a widened constraint table", source)
            })?;
    }

    Ok(())
}

/// Pairs of schema-v1 table names and their migration holding names.
pub(crate) const LEGACY_RENAMES: &[(&str, &str)] = &[
    ("install_records", "install_records_v1"),
    ("projection_records", "projection_records_v1"),
    ("update_checks", "update_checks_v1"),
    ("local_modifications", "local_modifications_v1"),
    ("pins", "pins_v1"),
    ("rollback_records", "rollback_records_v1"),
    ("history_events", "history_events_v1"),
];

/// Secondary index names the schema-v1 tables carried.
///
/// A schema-v1 database created every secondary index the current schema
/// declares, under the same names. The v1 migration drops them after the
/// renames so the recreated schema attaches fresh indexes to the current
/// tables instead of losing the names with the dropped `_v1` tables.
const LEGACY_SECONDARY_INDEXES: &[&str] = &[
    "idx_update_checks_skill_time",
    "idx_local_modifications_skill_time",
    "idx_rollback_records_skill_time",
    "idx_history_events_skill_time",
];

fn rename_legacy_table_if_needed(
    connection: &Connection,
    path: &Path,
    legacy_name: &str,
    migrated_name: &str,
) -> Result<(), AppError> {
    if sqlite_table_exists(connection, path, migrated_name)? {
        return Ok(());
    }
    if !sqlite_table_exists(connection, path, legacy_name)? {
        return Ok(());
    }

    connection
        .execute(
            &format!("ALTER TABLE {legacy_name} RENAME TO {migrated_name}"),
            [],
        )
        .map_err(|source| local_state_query(path, "rename legacy schema table", source))?;

    Ok(())
}

fn advance_schema_version(
    connection: &Connection,
    path: &Path,
    version: u32,
) -> Result<(), AppError> {
    connection
        .pragma_update(None, "user_version", version)
        .map_err(|source| local_state_query(path, "advance schema version", source))
}

/// Result of comparing legacy `_v1` tables with the current tables.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct LegacyReconciliation {
    /// Legacy tables that were left untouched because ownership is ambiguous.
    pub ambiguous: Vec<String>,
}

/// Safe action for one legacy table.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum LegacyTableAction {
    /// The legacy rows are unclaimed and can be copied, then dropped.
    CopyAndDrop {
        /// Rows still missing from the current table.
        missing: i64,
        /// Rows already present in identical form.
        identical: i64,
    },
    /// Every legacy row already lives in the current table.
    DropOnly {
        /// Rows already present in identical form.
        identical: i64,
    },
    /// A human decision is required before the rows can be moved.
    Ambiguous {
        /// Why the rows cannot be attributed to one workspace.
        reason: String,
    },
}

/// Classification of one legacy table.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LegacyTableDisposition {
    /// Legacy table name.
    pub legacy: &'static str,
    /// Current table name.
    pub current: &'static str,
    /// Rows held by the legacy table.
    pub legacy_rows: i64,
    /// Rows held by the current table.
    pub current_rows: i64,
    /// Safe action for this table.
    pub action: LegacyTableAction,
}

/// Detect and recover databases that hold both legacy and current tables.
///
/// Recovery copies or drops legacy rows only when row ownership is
/// unambiguous. Ambiguous tables are left untouched and reported so the
/// recovery entry point can preview them.
fn recover_stranded_legacy_tables(
    connection: &mut Connection,
    path: &Path,
    workspace_key: &str,
    version: u32,
) -> Result<(), AppError> {
    if present_legacy_tables(connection, path)?.is_empty() {
        return Ok(());
    }

    journal::record_migration_backup(path, version, version)?;
    let transaction = connection
        .transaction()
        .map_err(|source| local_state_query(path, "begin stranded state recovery", source))?;
    let reconciliation = reconcile_legacy_tables(&transaction, path, workspace_key)?;
    transaction
        .commit()
        .map_err(|source| local_state_query(path, "commit stranded state recovery", source))?;

    if reconciliation.ambiguous.is_empty() {
        Ok(())
    } else {
        Err(local_state_validation(
            path,
            format!(
                "legacy tables {} hold rows whose owning workspace cannot be determined; \
                 run 'skillctl doctor --repair-state' to preview recovery",
                reconciliation.ambiguous.join(", ")
            ),
        ))
    }
}

/// Reconcile legacy `_v1` tables with the current tables.
///
/// The caller owns the transaction so migration and repair share one atomic
/// boundary. Ambiguous tables are never changed.
pub(crate) fn reconcile_legacy_tables(
    connection: &Connection,
    path: &Path,
    workspace_key: &str,
) -> Result<LegacyReconciliation, AppError> {
    let mut reconciliation = LegacyReconciliation::default();

    for disposition in classify_legacy_tables(connection, path, workspace_key)? {
        let migration = migration_for(disposition.legacy).expect("legacy table is registered");
        match disposition.action {
            LegacyTableAction::CopyAndDrop { .. } => {
                copy_missing_legacy_rows(connection, path, migration, workspace_key)?;
                drop_legacy_table(connection, path, migration.legacy)?;
            }
            LegacyTableAction::DropOnly { .. } => {
                drop_legacy_table(connection, path, migration.legacy)?;
            }
            LegacyTableAction::Ambiguous { reason } => {
                reconciliation
                    .ambiguous
                    .push(format!("{} ({reason})", disposition.legacy));
            }
        }
    }

    Ok(reconciliation)
}

/// Classify every present legacy table without changing anything.
pub(crate) fn classify_legacy_tables(
    connection: &Connection,
    path: &Path,
    workspace_key: &str,
) -> Result<Vec<LegacyTableDisposition>, AppError> {
    let present = present_legacy_tables(connection, path)?;
    let mut dispositions = Vec::new();

    for migration in LEGACY_TABLE_MIGRATIONS {
        if !present.contains(&migration.legacy) {
            continue;
        }

        let legacy_rows = table_row_count(connection, path, migration.legacy)?;
        let current_rows = table_row_count(connection, path, migration.current)?;
        let action = if legacy_rows == 0 {
            LegacyTableAction::DropOnly { identical: 0 }
        } else {
            classify_rows(connection, path, migration, workspace_key, legacy_rows)?
        };

        dispositions.push(LegacyTableDisposition {
            legacy: migration.legacy,
            current: migration.current,
            legacy_rows,
            current_rows,
            action,
        });
    }

    Ok(dispositions)
}

fn classify_rows(
    connection: &Connection,
    path: &Path,
    migration: &LegacyTableMigration,
    workspace_key: &str,
    legacy_rows: i64,
) -> Result<LegacyTableAction, AppError> {
    let identical =
        count_projection_intersection(connection, path, migration, workspace_key, false)?;
    let shared_identity =
        count_projection_intersection(connection, path, migration, workspace_key, true)?;
    let conflicts = shared_identity - identical;

    if conflicts > 0 {
        return Ok(LegacyTableAction::Ambiguous {
            reason: format!(
                "{conflicts} row(s) already exist in {} with the same identity but different \
                 content",
                migration.current
            ),
        });
    }

    let missing = legacy_rows - shared_identity;
    if missing == 0 {
        return Ok(LegacyTableAction::DropOnly { identical });
    }

    let foreign_owners = connection
        .query_row(
            &format!(
                "SELECT COUNT(*) FROM {current} WHERE workspace_key NOT IN (?1, ?2)",
                current = migration.current
            ),
            params![workspace_key, GLOBAL_WORKSPACE_KEY],
            |row| row.get::<_, i64>(0),
        )
        .map_err(|source| local_state_query(path, "inspect legacy row ownership", source))?;

    if foreign_owners > 0 {
        return Ok(LegacyTableAction::Ambiguous {
            reason: format!(
                "{missing} row(s) are not recorded in {} while {foreign_owners} row(s) already \
                 belong to another workspace",
                migration.current
            ),
        });
    }

    Ok(LegacyTableAction::CopyAndDrop { missing, identical })
}

/// Count legacy rows that already exist in the current table.
///
/// With `identities_only`, rows are compared by identity columns only, which
/// detects rows that were copied once and then changed.
fn count_projection_intersection(
    connection: &Connection,
    path: &Path,
    migration: &LegacyTableMigration,
    workspace_key: &str,
    identities_only: bool,
) -> Result<i64, AppError> {
    let columns = if identities_only {
        migration.identity_columns
    } else {
        migration.current_columns
    };
    let sql = format!(
        "SELECT COUNT(*) FROM (\
            SELECT {columns} FROM ({projection}) \
            INTERSECT \
            SELECT {columns} FROM {current}\
         )",
        columns = columns.join(", "),
        projection = legacy_projection(migration),
        current = migration.current
    );

    connection
        .query_row(&sql, params![workspace_key, GLOBAL_WORKSPACE_KEY], |row| {
            row.get::<_, i64>(0)
        })
        .map_err(|source| local_state_query(path, "compare legacy rows", source))
}

/// The legacy table projected onto the current column names.
fn legacy_projection(migration: &LegacyTableMigration) -> String {
    let selection = migration
        .current_columns
        .iter()
        .zip(migration.legacy_expressions)
        .map(|(name, expression)| format!("{expression} AS {name}"))
        .collect::<Vec<_>>()
        .join(", ");

    format!("SELECT {selection} FROM {}", migration.legacy)
}

fn copy_missing_legacy_rows(
    connection: &Connection,
    path: &Path,
    migration: &LegacyTableMigration,
    workspace_key: &str,
) -> Result<(), AppError> {
    let columns = migration.current_columns.join(", ");
    let sql = format!(
        "INSERT INTO {current} ({columns}) \
         SELECT {columns} FROM (\
            {projection} \
            EXCEPT \
            SELECT {columns} FROM {current}\
         )",
        current = migration.current,
        columns = columns,
        projection = legacy_projection(migration),
    );

    connection
        .execute(&sql, params![workspace_key, GLOBAL_WORKSPACE_KEY])
        .map_err(|source| local_state_query(path, "migrate legacy rows", source))
        .map(|_| ())
}

fn drop_legacy_table(connection: &Connection, path: &Path, legacy: &str) -> Result<(), AppError> {
    connection
        .execute(&format!("DROP TABLE {legacy}"), [])
        .map_err(|source| local_state_query(path, "drop migrated legacy table", source))
        .map(|_| ())
}

/// Drop every present legacy `_v1` table.
pub(crate) fn drop_legacy_tables(
    connection: &Connection,
    path: &Path,
) -> Result<Vec<&'static str>, AppError> {
    let mut dropped = Vec::new();
    for migration in LEGACY_TABLE_MIGRATIONS {
        if !sqlite_table_exists(connection, path, migration.legacy)? {
            continue;
        }
        drop_legacy_table(connection, path, migration.legacy)?;
        dropped.push(migration.legacy);
    }

    Ok(dropped)
}

fn present_legacy_tables(
    connection: &Connection,
    path: &Path,
) -> Result<BTreeSet<&'static str>, AppError> {
    let mut present = BTreeSet::new();
    for migration in LEGACY_TABLE_MIGRATIONS {
        if sqlite_table_exists(connection, path, migration.legacy)? {
            present.insert(migration.legacy);
        }
    }
    Ok(present)
}

fn migration_for(legacy: &str) -> Option<&'static LegacyTableMigration> {
    LEGACY_TABLE_MIGRATIONS
        .iter()
        .find(|migration| migration.legacy == legacy)
}

pub(super) fn schema_version(connection: &Connection, path: &Path) -> Result<u32, AppError> {
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

fn sqlite_table_exists(
    connection: &Connection,
    path: &Path,
    table: &str,
) -> Result<bool, AppError> {
    let exists = connection
        .query_row(
            "SELECT EXISTS(
                SELECT 1
                  FROM sqlite_master
                 WHERE type = 'table' AND name = ?1
            )",
            [table],
            |row| row.get::<_, bool>(0),
        )
        .map_err(|source| local_state_query(path, "inspect table presence", source))?;

    Ok(exists)
}

fn table_row_count(connection: &Connection, path: &Path, table: &str) -> Result<i64, AppError> {
    connection
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get::<_, i64>(0)
        })
        .map_err(|source| local_state_query(path, "count table rows", source))
}

pub(super) fn validate_required_tables(
    connection: &Connection,
    path: &Path,
) -> Result<(), AppError> {
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
    if missing.is_empty() {
        Ok(())
    } else {
        Err(local_state_validation(
            path,
            format!("missing required tables: {}", missing.join(", ")),
        ))
    }
}

/// Read-only classification of one state database without migrating it.
pub(crate) struct SchemaInspection {
    /// Schema version found in the database, `0` when no schema exists.
    pub version: u32,
    /// Classification of every present legacy table.
    pub dispositions: Vec<LegacyTableDisposition>,
}

/// Whether SQLite can read one state database file.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum DatabaseHealth {
    /// The file does not exist.
    Missing,
    /// SQLite opens the file and its integrity check passes.
    Readable,
    /// The file exists, but SQLite cannot read it as a database.
    Unreadable {
        /// Why SQLite refused the file.
        reason: String,
    },
}

/// Classify one state database file without migrating or changing it.
///
/// Opening a file that is not a database fails only at the first read, so the
/// probe reads the version marker and then runs SQLite's own integrity check.
pub(crate) fn database_health(path: &Path) -> DatabaseHealth {
    if !path.exists() {
        return DatabaseHealth::Missing;
    }

    let connection = match Connection::open(path) {
        Ok(connection) => connection,
        Err(source) => {
            return DatabaseHealth::Unreadable {
                reason: source.to_string(),
            };
        }
    };
    if let Err(source) = super::configure_connection(&connection, path) {
        return DatabaseHealth::Unreadable {
            reason: source.to_string(),
        };
    }

    // Reading `user_version` is what turns a foreign file into
    // `file is not a database`.
    if let Err(source) =
        connection.pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
    {
        return DatabaseHealth::Unreadable {
            reason: source.to_string(),
        };
    }

    match connection.query_row("PRAGMA quick_check;", [], |row| row.get::<_, String>(0)) {
        Ok(check) if check == "ok" => DatabaseHealth::Readable,
        Ok(check) => DatabaseHealth::Unreadable {
            reason: format!("integrity check reported {check}"),
        },
        Err(source) => DatabaseHealth::Unreadable {
            reason: source.to_string(),
        },
    }
}

/// Whether one SQLite error means the file cannot be read as a database.
///
/// Both codes describe a file no command can open, so both carry the same
/// documented recovery instead of the raw SQLite text.
pub(crate) fn is_unreadable_database_error(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(failure, _)
            if matches!(
                failure.code,
                rusqlite::ErrorCode::NotADatabase | rusqlite::ErrorCode::DatabaseCorrupt
            )
    )
}

/// Whether one SQLite error means a competing lock outlasted the busy timeout.
///
/// Only `DatabaseBusy` counts. The busy timeout governs that code alone, so a
/// shared-cache lock inside one connection must not be reported as a timeout a
/// longer wait could have survived.
pub(crate) fn is_busy_timeout_error(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(failure, _)
            if matches!(failure.code, rusqlite::ErrorCode::DatabaseBusy)
    )
}

/// Inspect one database without opening or migrating it.
pub(crate) fn inspect_database(
    path: &Path,
    workspace_key: &str,
) -> Result<SchemaInspection, AppError> {
    let connection = Connection::open(path).map_err(|source| AppError::LocalStateOpen {
        path: path.to_path_buf(),
        source,
    })?;
    super::configure_connection(&connection, path)?;
    let version = schema_version(&connection, path)?;
    let dispositions = classify_legacy_tables(&connection, path, workspace_key)?;

    Ok(SchemaInspection {
        version,
        dispositions,
    })
}

/// SQL that creates the current schema objects without the version marker.
fn local_state_tables_sql() -> String {
    format!(
        "
CREATE TABLE IF NOT EXISTS install_records (
    scope TEXT NOT NULL CHECK (scope IN ('workspace', 'user')),
    workspace_key TEXT NOT NULL,
    skill_id TEXT NOT NULL,
    source_kind TEXT NOT NULL CHECK (
        source_kind IN ('git', 'local-path', 'archive', 'well-known')
    ),
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
    PRIMARY KEY (scope, workspace_key, skill_id)
);

CREATE TABLE IF NOT EXISTS projection_records (
    scope TEXT NOT NULL CHECK (scope IN ('workspace', 'user')),
    workspace_key TEXT NOT NULL,
    skill_id TEXT NOT NULL,
    target TEXT NOT NULL CHECK (
        target IN ('codex', 'claude-code', 'github-copilot', 'gemini-cli', 'amp', 'opencode')
    ),
    generation_mode TEXT NOT NULL CHECK (generation_mode IN ('copy', 'symlink')),
    physical_root TEXT NOT NULL,
    projected_path TEXT NOT NULL,
    effective_version_hash TEXT NOT NULL,
    generated_at TEXT NOT NULL,
    PRIMARY KEY (scope, workspace_key, skill_id, target, physical_root)
);

CREATE TABLE IF NOT EXISTS update_checks (
    id INTEGER PRIMARY KEY,
    scope TEXT NOT NULL CHECK (scope IN ('workspace', 'user')),
    workspace_key TEXT NOT NULL,
    skill_id TEXT NOT NULL,
    checked_at TEXT NOT NULL,
    pinned_revision TEXT NOT NULL,
    latest_revision TEXT,
    outcome TEXT NOT NULL CHECK (
        outcome IN (
            'up-to-date',
            'update-available',
            'blocked',
            'detached',
            'local-source',
            'commit-pinned',
            'failed'
        )
    ),
    overlay_detected INTEGER NOT NULL CHECK (overlay_detected IN (0, 1)),
    local_modification_detected INTEGER NOT NULL CHECK (local_modification_detected IN (0, 1)),
    apply_blocked_by TEXT CHECK (
        apply_blocked_by IN ('installed-scripts', 'incoming-scripts')
    ),
    notes TEXT
);

CREATE INDEX IF NOT EXISTS idx_update_checks_skill_time
    ON update_checks (scope, workspace_key, skill_id, checked_at DESC, id DESC);

CREATE TABLE IF NOT EXISTS local_modifications (
    id INTEGER PRIMARY KEY,
    scope TEXT NOT NULL CHECK (scope IN ('workspace', 'user')),
    workspace_key TEXT NOT NULL,
    skill_id TEXT NOT NULL,
    detected_at TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('overlay', 'projected-copy', 'detached-fork')),
    path TEXT,
    details TEXT
);

CREATE INDEX IF NOT EXISTS idx_local_modifications_skill_time
    ON local_modifications (scope, workspace_key, skill_id, detected_at DESC, id DESC);

CREATE TABLE IF NOT EXISTS pins (
    scope TEXT NOT NULL CHECK (scope IN ('workspace', 'user')),
    workspace_key TEXT NOT NULL,
    skill_id TEXT NOT NULL,
    requested_reference TEXT NOT NULL,
    resolved_revision TEXT NOT NULL,
    effective_version_hash TEXT,
    pinned_at TEXT NOT NULL,
    PRIMARY KEY (scope, workspace_key, skill_id)
);

CREATE TABLE IF NOT EXISTS rollback_records (
    id INTEGER PRIMARY KEY,
    scope TEXT NOT NULL CHECK (scope IN ('workspace', 'user')),
    workspace_key TEXT NOT NULL,
    skill_id TEXT NOT NULL,
    rolled_back_at TEXT NOT NULL,
    from_reference TEXT NOT NULL,
    to_reference TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_rollback_records_skill_time
    ON rollback_records (scope, workspace_key, skill_id, rolled_back_at DESC, id DESC);

CREATE TABLE IF NOT EXISTS skill_reviews (
    scope TEXT NOT NULL CHECK (scope IN ('workspace', 'user')),
    workspace_key TEXT NOT NULL,
    skill_id TEXT NOT NULL,
    decision TEXT NOT NULL CHECK (decision IN ('approved', 'revoked')),
    resolved_revision TEXT NOT NULL,
    effective_version_hash TEXT NOT NULL,
    contains_scripts INTEGER NOT NULL CHECK (contains_scripts IN (0, 1)),
    reviewed_at TEXT NOT NULL,
    note TEXT,
    PRIMARY KEY (scope, workspace_key, skill_id)
);

CREATE TABLE IF NOT EXISTS telemetry_settings (
    singleton_id INTEGER PRIMARY KEY CHECK (singleton_id = 1),
    consent TEXT NOT NULL CHECK (consent IN ('unknown', 'enabled', 'disabled')),
    notice_seen_at TEXT,
    updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS history_events (
    id INTEGER PRIMARY KEY,
    workspace_key TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ({history_event_kinds})),
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

CREATE INDEX IF NOT EXISTS idx_history_events_skill_time
    ON history_events (scope, workspace_key, skill_id, occurred_at DESC, id DESC);
",
        history_event_kinds = HISTORY_EVENT_KINDS_SQL,
    )
}

/// Every history event kind the current schema accepts, as SQL literals.
///
/// The list is shared by the schema that creates `history_events` and by the
/// additive migration that rebuilds it, so the two can never drift.
const HISTORY_EVENT_KINDS_SQL: &str = concat!(
    "'install', 'update-check', 'update-applied', 'projection', 'pin', 'rollback', ",
    "'overlay-created', 'direct-modification-detected', 'detach', 'fork', 'cleanup', ",
    "'prune', 'telemetry-consent-changed', 'trust-review'"
);

/// Default hook that injects a failure from the environment.
fn env_migration_hook(stage: MigrationStage) -> Result<(), AppError> {
    let Ok(expected) = env::var(MIGRATION_FAILPOINT_ENV) else {
        return Ok(());
    };
    if expected.trim() == stage.as_str() {
        return Err(AppError::FilesystemOperation {
            action: "execute schema migration failpoint",
            path: PathBuf::from(format!("{MIGRATION_FAILPOINT_ENV}:{}", stage.as_str())),
            source: std::io::Error::other("injected migration failure"),
        });
    }

    Ok(())
}

/// Resolve the current home directory.
///
/// Empty `HOME` and `USERPROFILE` values are ignored, and Windows
/// `HOMEDRIVE`+`HOMEPATH` pairs are honored, so every store and sync helper
/// that roots itself at the home directory agrees with the state database on
/// which directory that is.
pub(crate) fn resolve_home_directory() -> Option<PathBuf> {
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
