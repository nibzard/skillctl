use super::*;

pub(super) fn bootstrap_connection(
    connection: &Connection,
    path: &Path,
    workspace_key: &str,
) -> Result<(), AppError> {
    connection
        .execute_batch("PRAGMA foreign_keys = ON;")
        .map_err(|source| local_state_query(path, "enable foreign keys", source))?;
    initialize_or_validate_schema(connection, path, workspace_key)
}

fn initialize_or_validate_schema(
    connection: &Connection,
    path: &Path,
    workspace_key: &str,
) -> Result<(), AppError> {
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
        VersionDisposition::NeedsMigration { from, to } => {
            migrate_schema(connection, path, workspace_key, from, to)?;
            validate_required_tables(connection, path)
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

fn migrate_schema(
    connection: &Connection,
    path: &Path,
    workspace_key: &str,
    from: u32,
    to: u32,
) -> Result<(), AppError> {
    if from != 1 || to != CURRENT_LOCAL_STATE_VERSION {
        return Err(local_state_validation(
            path,
            format!("schema version {from} requires migration to {to}"),
        ));
    }

    for (legacy_name, migrated_name) in [
        ("install_records", "install_records_v1"),
        ("projection_records", "projection_records_v1"),
        ("update_checks", "update_checks_v1"),
        ("local_modifications", "local_modifications_v1"),
        ("pins", "pins_v1"),
        ("rollback_records", "rollback_records_v1"),
        ("history_events", "history_events_v1"),
    ] {
        rename_legacy_table_if_needed(connection, path, legacy_name, migrated_name)?;
    }

    connection
        .execute_batch(&local_state_schema_sql())
        .map_err(|source| {
            local_state_query(path, "initialize migrated local state schema", source)
        })?;

    connection
        .execute(
            "INSERT INTO install_records (scope, workspace_key, skill_id, source_kind, source_url, \
             source_subpath, resolved_revision, upstream_revision, content_hash, overlay_hash, \
             effective_version_hash, installed_at, updated_at, detached, forked) \
             SELECT scope, CASE WHEN scope = 'workspace' THEN ?1 ELSE ?2 END, skill_id, source_kind, \
             source_url, source_subpath, resolved_revision, upstream_revision, content_hash, overlay_hash, \
             effective_version_hash, installed_at, updated_at, detached, forked FROM install_records_v1",
            params![workspace_key, GLOBAL_WORKSPACE_KEY],
        )
        .map_err(|source| local_state_query(path, "migrate install records", source))?;
    connection
        .execute(
            "INSERT INTO projection_records (scope, workspace_key, skill_id, target, generation_mode, \
             physical_root, projected_path, effective_version_hash, generated_at) \
             SELECT scope, CASE WHEN scope = 'workspace' THEN ?1 ELSE ?2 END, skill_id, target, generation_mode, \
             physical_root, projected_path, effective_version_hash, generated_at FROM projection_records_v1",
            params![workspace_key, GLOBAL_WORKSPACE_KEY],
        )
        .map_err(|source| local_state_query(path, "migrate projection records", source))?;
    connection
        .execute(
            "INSERT INTO update_checks (scope, workspace_key, skill_id, checked_at, pinned_revision, \
             latest_revision, outcome, overlay_detected, local_modification_detected, notes) \
             SELECT scope, CASE WHEN scope = 'workspace' THEN ?1 ELSE ?2 END, skill_id, checked_at, pinned_revision, \
             latest_revision, outcome, overlay_detected, local_modification_detected, notes FROM update_checks_v1",
            params![workspace_key, GLOBAL_WORKSPACE_KEY],
        )
        .map_err(|source| local_state_query(path, "migrate update checks", source))?;
    connection
        .execute(
            "INSERT INTO local_modifications (scope, workspace_key, skill_id, detected_at, kind, path, details) \
             SELECT scope, CASE WHEN scope = 'workspace' THEN ?1 ELSE ?2 END, skill_id, detected_at, kind, path, details \
             FROM local_modifications_v1",
            params![workspace_key, GLOBAL_WORKSPACE_KEY],
        )
        .map_err(|source| local_state_query(path, "migrate local modifications", source))?;
    connection
        .execute(
            "INSERT INTO pins (scope, workspace_key, skill_id, requested_reference, resolved_revision, \
             effective_version_hash, pinned_at) \
             SELECT scope, CASE WHEN scope = 'workspace' THEN ?1 ELSE ?2 END, skill_id, requested_reference, \
             resolved_revision, effective_version_hash, pinned_at FROM pins_v1",
            params![workspace_key, GLOBAL_WORKSPACE_KEY],
        )
        .map_err(|source| local_state_query(path, "migrate pin records", source))?;
    connection
        .execute(
            "INSERT INTO rollback_records (scope, workspace_key, skill_id, rolled_back_at, from_reference, to_reference) \
             SELECT scope, CASE WHEN scope = 'workspace' THEN ?1 ELSE ?2 END, skill_id, rolled_back_at, from_reference, \
             to_reference FROM rollback_records_v1",
            params![workspace_key, GLOBAL_WORKSPACE_KEY],
        )
        .map_err(|source| local_state_query(path, "migrate rollback records", source))?;
    connection
        .execute(
            "INSERT INTO history_events (workspace_key, kind, scope, skill_id, target, occurred_at, summary, details_json) \
             SELECT CASE WHEN scope = 'workspace' THEN ?1 ELSE ?2 END, kind, scope, skill_id, target, occurred_at, summary, details_json \
             FROM history_events_v1",
            params![workspace_key, GLOBAL_WORKSPACE_KEY],
        )
        .map_err(|source| local_state_query(path, "migrate history entries", source))?;

    connection
        .execute_batch(
            "
DROP TABLE install_records_v1;
DROP TABLE projection_records_v1;
DROP TABLE update_checks_v1;
DROP TABLE local_modifications_v1;
DROP TABLE pins_v1;
DROP TABLE rollback_records_v1;
DROP TABLE history_events_v1;
",
        )
        .map_err(|source| local_state_query(path, "drop migrated legacy tables", source))?;

    Ok(())
}

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
    if missing.is_empty() {
        Ok(())
    } else {
        Err(local_state_validation(
            path,
            format!("missing required tables: {}", missing.join(", ")),
        ))
    }
}

fn local_state_schema_sql() -> String {
    format!(
        r"
CREATE TABLE IF NOT EXISTS install_records (
    scope TEXT NOT NULL CHECK (scope IN ('workspace', 'user')),
    workspace_key TEXT NOT NULL,
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
            'failed'
        )
    ),
    overlay_detected INTEGER NOT NULL CHECK (overlay_detected IN (0, 1)),
    local_modification_detected INTEGER NOT NULL CHECK (local_modification_detected IN (0, 1)),
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

CREATE TABLE IF NOT EXISTS telemetry_settings (
    singleton_id INTEGER PRIMARY KEY CHECK (singleton_id = 1),
    consent TEXT NOT NULL CHECK (consent IN ('unknown', 'enabled', 'disabled')),
    notice_seen_at TEXT,
    updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS history_events (
    id INTEGER PRIMARY KEY,
    workspace_key TEXT NOT NULL,
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

CREATE INDEX IF NOT EXISTS idx_history_events_skill_time
    ON history_events (scope, workspace_key, skill_id, occurred_at DESC, id DESC);

PRAGMA user_version = {version};
",
        version = CURRENT_LOCAL_STATE_VERSION
    )
}

pub(super) fn resolve_home_directory() -> Option<PathBuf> {
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
