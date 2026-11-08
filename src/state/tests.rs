use super::*;
use crate::history::HistoryLedger;
use serde_json::json;
use std::collections::BTreeSet;
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
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'")
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
        apply_blocked_by: Some(UpdateCheckBlockReason::IncomingScripts),
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
    let recorded_check = store
        .latest_update_check(&skill)
        .expect("latest update check loads")
        .expect("latest update check exists");
    assert_eq!(recorded_check.outcome, UpdateCheckOutcome::UpdateAvailable);
    assert_eq!(
        recorded_check.apply_blocked_by,
        Some(UpdateCheckBlockReason::IncomingScripts),
        "the block reason the check recorded survives the roundtrip",
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
fn workspace_scoped_records_are_isolated_by_workspace_key() {
    let temp = tempdir().expect("tempdir exists");
    let path = temp.path().join("state.db");
    let workspace_a = temp.path().join("workspace-a");
    let workspace_b = temp.path().join("workspace-b");
    fs::create_dir_all(&workspace_a).expect("workspace a exists");
    fs::create_dir_all(&workspace_b).expect("workspace b exists");

    let mut store_a = LocalStateStore::open_at_for(&path, &workspace_a).expect("workspace a opens");
    let mut store_b = LocalStateStore::open_at_for(&path, &workspace_b).expect("workspace b opens");

    let workspace_skill = ManagedSkillRef::new(ManagedScope::Workspace, "release-notes");
    let user_skill = ManagedSkillRef::new(ManagedScope::User, "release-notes");

    let install_record =
        |skill: ManagedSkillRef, revision: &str, installed_at: &str| InstallRecord {
            skill,
            source_kind: SourceKind::Git,
            source_url: "https://example.com/release-notes.git".to_string(),
            source_subpath: ".agents/skills/release-notes".to_string(),
            resolved_revision: revision.to_string(),
            upstream_revision: Some(revision.to_string()),
            content_hash: format!("sha256:{revision}:content"),
            overlay_hash: "sha256:none".to_string(),
            effective_version_hash: format!("sha256:{revision}:effective"),
            installed_at: installed_at.to_string(),
            updated_at: installed_at.to_string(),
            detached: false,
            forked: false,
        };

    let workspace_a_install = install_record(
        workspace_skill.clone(),
        "aaaaaaaaaaaaaaaa",
        "2026-03-19T12:00:00Z",
    );
    let workspace_b_install = install_record(
        workspace_skill.clone(),
        "bbbbbbbbbbbbbbbb",
        "2026-03-19T12:05:00Z",
    );
    let user_install = install_record(
        user_skill.clone(),
        "cccccccccccccccc",
        "2026-03-19T12:10:00Z",
    );

    store_a
        .upsert_install_record(&workspace_a_install)
        .expect("workspace a install writes");
    store_b
        .upsert_install_record(&workspace_b_install)
        .expect("workspace b install writes");
    store_a
        .upsert_install_record(&user_install)
        .expect("user install writes");

    assert_eq!(
        store_a
            .install_record(&workspace_skill)
            .expect("workspace a install loads")
            .expect("workspace a install exists"),
        workspace_a_install
    );
    assert_eq!(
        store_b
            .install_record(&workspace_skill)
            .expect("workspace b install loads")
            .expect("workspace b install exists"),
        workspace_b_install
    );
    assert_eq!(
        store_a
            .install_record(&user_skill)
            .expect("user install loads in workspace a")
            .expect("user install exists in workspace a"),
        user_install
    );
    assert_eq!(
        store_b
            .install_record(&user_skill)
            .expect("user install loads in workspace b")
            .expect("user install exists in workspace b"),
        user_install
    );
    assert_eq!(
        store_a
            .list_install_records()
            .expect("workspace a records load")
            .into_iter()
            .filter(|record| record.skill.scope == ManagedScope::Workspace)
            .collect::<Vec<_>>(),
        vec![workspace_a_install]
    );
    assert_eq!(
        store_b
            .list_install_records()
            .expect("workspace b records load")
            .into_iter()
            .filter(|record| record.skill.scope == ManagedScope::Workspace)
            .collect::<Vec<_>>(),
        vec![workspace_b_install]
    );
    assert_eq!(
        store_a
            .list_install_records()
            .expect("user records load in workspace a")
            .into_iter()
            .filter(|record| record.skill.scope == ManagedScope::User)
            .collect::<Vec<_>>(),
        vec![user_install.clone()]
    );
    assert_eq!(
        store_b
            .list_install_records()
            .expect("user records load in workspace b")
            .into_iter()
            .filter(|record| record.skill.scope == ManagedScope::User)
            .collect::<Vec<_>>(),
        vec![user_install]
    );
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
    let connection = Connection::open(&path).expect("sqlite opens");
    connection
        .execute_batch(
            "
CREATE TABLE install_records (scope TEXT NOT NULL, skill_id TEXT NOT NULL, PRIMARY KEY (scope, skill_id));
PRAGMA user_version = 999;
",
        )
        .expect("legacy schema setup succeeds");

    let error = LocalStateStore::open_at(&path).expect_err("unsupported schema is rejected");

    assert!(
        error.to_string().contains(&format!(
            "schema version supports {} through {}, found 999",
            LOCAL_STATE_SCHEMA_POLICY.minimum_supported(),
            LOCAL_STATE_SCHEMA_POLICY.current()
        )),
        "unexpected error: {error}"
    );
}

#[test]
fn reopening_recovers_from_partially_migrated_v1_state() {
    let temp = tempdir().expect("tempdir exists");
    let path = temp.path().join("state.db");
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).expect("workspace exists");

    let connection = Connection::open(&path).expect("sqlite opens");
    connection
        .execute_batch(
            "
CREATE TABLE install_records_v1 (
    scope TEXT NOT NULL,
    skill_id TEXT NOT NULL,
    source_kind TEXT NOT NULL,
    source_url TEXT NOT NULL,
    source_subpath TEXT NOT NULL,
    resolved_revision TEXT NOT NULL,
    upstream_revision TEXT,
    content_hash TEXT NOT NULL,
    overlay_hash TEXT NOT NULL,
    effective_version_hash TEXT NOT NULL,
    installed_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    detached INTEGER NOT NULL,
    forked INTEGER NOT NULL,
    PRIMARY KEY (scope, skill_id)
);
CREATE TABLE install_records (
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
CREATE TABLE projection_records (
    scope TEXT NOT NULL,
    skill_id TEXT NOT NULL,
    target TEXT NOT NULL,
    generation_mode TEXT NOT NULL,
    physical_root TEXT NOT NULL,
    projected_path TEXT NOT NULL,
    effective_version_hash TEXT NOT NULL,
    generated_at TEXT NOT NULL,
    PRIMARY KEY (scope, skill_id, target, physical_root)
);
CREATE TABLE update_checks (
    id INTEGER PRIMARY KEY,
    scope TEXT NOT NULL,
    skill_id TEXT NOT NULL,
    checked_at TEXT NOT NULL,
    pinned_revision TEXT NOT NULL,
    latest_revision TEXT,
    outcome TEXT NOT NULL,
    overlay_detected INTEGER NOT NULL,
    local_modification_detected INTEGER NOT NULL,
    notes TEXT
);
CREATE TABLE local_modifications (
    id INTEGER PRIMARY KEY,
    scope TEXT NOT NULL,
    skill_id TEXT NOT NULL,
    detected_at TEXT NOT NULL,
    kind TEXT NOT NULL,
    path TEXT,
    details TEXT
);
CREATE TABLE pins (
    scope TEXT NOT NULL,
    skill_id TEXT NOT NULL,
    requested_reference TEXT NOT NULL,
    resolved_revision TEXT NOT NULL,
    effective_version_hash TEXT,
    pinned_at TEXT NOT NULL,
    PRIMARY KEY (scope, skill_id)
);
CREATE TABLE rollback_records (
    id INTEGER PRIMARY KEY,
    scope TEXT NOT NULL,
    skill_id TEXT NOT NULL,
    rolled_back_at TEXT NOT NULL,
    from_reference TEXT NOT NULL,
    to_reference TEXT NOT NULL
);
CREATE TABLE telemetry_settings (
    singleton_id INTEGER PRIMARY KEY CHECK (singleton_id = 1),
    consent TEXT NOT NULL,
    notice_seen_at TEXT,
    updated_at TEXT NOT NULL
);
CREATE TABLE history_events (
    id INTEGER PRIMARY KEY,
    kind TEXT NOT NULL,
    scope TEXT,
    skill_id TEXT,
    target TEXT,
    occurred_at TEXT NOT NULL,
    summary TEXT NOT NULL,
    details_json TEXT
);
PRAGMA user_version = 1;
",
        )
        .expect("partial legacy schema setup succeeds");

    connection
        .execute(
            "INSERT INTO install_records_v1 (
                scope, skill_id, source_kind, source_url, source_subpath, resolved_revision,
                upstream_revision, content_hash, overlay_hash, effective_version_hash,
                installed_at, updated_at, detached, forked
            ) VALUES (
                'workspace', 'release-notes', 'git', 'https://example.com/release-notes.git',
                '.agents/skills/release-notes', 'aaaaaaaaaaaaaaaa', 'bbbbbbbbbbbbbbbb',
                'sha256:content', 'sha256:overlay', 'sha256:effective',
                '2026-03-19T12:00:00Z', '2026-03-19T12:05:00Z', 0, 0
            )",
            [],
        )
        .expect("legacy install record writes");

    let store =
        LocalStateStore::open_at_for(&path, &workspace).expect("partial migration recovery works");

    assert_eq!(
        store.schema_version().expect("schema version is readable"),
        CURRENT_LOCAL_STATE_VERSION
    );

    let install = store
        .install_record(&ManagedSkillRef::new(
            ManagedScope::Workspace,
            "release-notes",
        ))
        .expect("install record loads")
        .expect("install record exists");
    assert_eq!(install.source_url, "https://example.com/release-notes.git");

    let legacy_backup_count: i64 = store
        .connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'install_records_v1'",
            [],
            |row| row.get(0),
        )
        .expect("legacy backup query succeeds");
    assert_eq!(legacy_backup_count, 0);
}

#[test]
fn coordination_lock_is_released_after_each_transaction_outcome() {
    let temp = tempdir().expect("tempdir exists");
    let database = temp.path().join("state.db");
    fs::write(&database, b"placeholder").expect("database exists");

    let lock_path = super::lock::lock_path_for(&database).expect("lock path resolves");
    let probe = || {
        use std::fs::File;
        let file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .expect("lock file opens");
        match file.try_lock() {
            Ok(()) => {
                let _ = file.unlock();
                true
            }
            Err(_) => false,
        }
    };

    crate::lifecycle::run_transaction("probe", |_transaction| Ok::<(), AppError>(()))
        .expect("successful transaction runs");
    assert!(probe(), "lock is free after a successful transaction");

    let failure = crate::lifecycle::run_transaction("probe", |_transaction| {
        Err::<(), AppError>(AppError::LocalStateValidation {
            path: database.clone(),
            message: "injected".to_string(),
        })
    })
    .expect_err("failing transaction reports its error");
    assert!(failure.to_string().contains("injected"));
    assert!(probe(), "lock is free after a rolled-back transaction");
}

/// Schema of a schema-v1 state database with one row in every table.
const LEGACY_V1_SCHEMA: &str = r"
CREATE TABLE install_records (
    scope TEXT NOT NULL, skill_id TEXT NOT NULL, source_kind TEXT NOT NULL,
    source_url TEXT NOT NULL, source_subpath TEXT NOT NULL, resolved_revision TEXT NOT NULL,
    upstream_revision TEXT, content_hash TEXT NOT NULL, overlay_hash TEXT NOT NULL,
    effective_version_hash TEXT NOT NULL, installed_at TEXT NOT NULL, updated_at TEXT NOT NULL,
    detached INTEGER NOT NULL, forked INTEGER NOT NULL, PRIMARY KEY (scope, skill_id)
);
CREATE TABLE projection_records (
    scope TEXT NOT NULL, skill_id TEXT NOT NULL, target TEXT NOT NULL,
    generation_mode TEXT NOT NULL, physical_root TEXT NOT NULL, projected_path TEXT NOT NULL,
    effective_version_hash TEXT NOT NULL, generated_at TEXT NOT NULL,
    PRIMARY KEY (scope, skill_id, target, physical_root)
);
CREATE TABLE update_checks (
    id INTEGER PRIMARY KEY, scope TEXT NOT NULL, skill_id TEXT NOT NULL, checked_at TEXT NOT NULL,
    pinned_revision TEXT NOT NULL, latest_revision TEXT, outcome TEXT NOT NULL,
    overlay_detected INTEGER NOT NULL, local_modification_detected INTEGER NOT NULL, notes TEXT
);
CREATE TABLE local_modifications (
    id INTEGER PRIMARY KEY, scope TEXT NOT NULL, skill_id TEXT NOT NULL,
    detected_at TEXT NOT NULL, kind TEXT NOT NULL, path TEXT, details TEXT
);
CREATE TABLE pins (
    scope TEXT NOT NULL, skill_id TEXT NOT NULL, requested_reference TEXT NOT NULL,
    resolved_revision TEXT NOT NULL, effective_version_hash TEXT, pinned_at TEXT NOT NULL,
    PRIMARY KEY (scope, skill_id)
);
CREATE TABLE rollback_records (
    id INTEGER PRIMARY KEY, scope TEXT NOT NULL, skill_id TEXT NOT NULL,
    rolled_back_at TEXT NOT NULL, from_reference TEXT NOT NULL, to_reference TEXT NOT NULL
);
CREATE TABLE telemetry_settings (
    singleton_id INTEGER PRIMARY KEY, consent TEXT NOT NULL, notice_seen_at TEXT,
    updated_at TEXT NOT NULL
);
CREATE TABLE history_events (
    id INTEGER PRIMARY KEY, kind TEXT NOT NULL, scope TEXT, skill_id TEXT, target TEXT,
    occurred_at TEXT NOT NULL, summary TEXT NOT NULL, details_json TEXT
);
INSERT INTO install_records VALUES
    ('workspace', 'release-notes', 'git', 'https://example.com/release-notes.git',
     '.agents/skills/release-notes', 'aaaaaaaaaaaaaaaa', 'bbbbbbbbbbbbbbbb',
     'sha256:content', 'sha256:none', 'sha256:effective-a',
     '2026-03-19T12:00:00Z', '2026-03-19T12:00:00Z', 0, 0),
    ('user', 'ai-sdk', 'git', 'https://example.com/ai-sdk.git',
     'skills/ai-sdk', 'cccccccccccccccc', NULL,
     'sha256:content-c', 'sha256:none', 'sha256:effective-c',
     '2026-03-19T12:10:00Z', '2026-03-19T12:10:00Z', 0, 0);
INSERT INTO projection_records VALUES
    ('workspace', 'release-notes', 'codex', 'copy', '.codex/skills', 'release-notes',
     'sha256:effective-a', '2026-03-19T12:01:00Z');
INSERT INTO update_checks VALUES
    (1, 'workspace', 'release-notes', '2026-03-19T12:02:00Z', 'aaaaaaaaaaaaaaaa',
     'bbbbbbbbbbbbbbbb', 'update-available', 0, 0, 'upstream advanced');
INSERT INTO local_modifications VALUES
    (1, 'workspace', 'release-notes', '2026-03-19T12:03:00Z', 'projected-copy',
     '.codex/skills/release-notes/SKILL.md', 'edited generated copy');
INSERT INTO pins VALUES
    ('workspace', 'release-notes', 'refs/tags/v1.0.0', 'aaaaaaaaaaaaaaaa',
     'sha256:effective-a', '2026-03-19T12:04:00Z');
INSERT INTO rollback_records VALUES
    (1, 'workspace', 'release-notes', '2026-03-19T12:05:00Z',
     'sha256:old', 'sha256:effective-a');
INSERT INTO telemetry_settings VALUES (1, 'enabled', '2026-03-19T12:06:00Z', '2026-03-19T12:06:00Z');
INSERT INTO history_events VALUES
    (1, 'install', 'workspace', 'release-notes', NULL, '2026-03-19T12:00:00Z',
     'Installed release-notes', NULL),
    (2, 'projection', 'workspace', 'release-notes', 'codex', '2026-03-19T12:01:00Z',
     'Projected release-notes into codex', NULL);
PRAGMA user_version = 1;
";

/// Rows that identify every legacy table: table name to sorted row identities.
fn legacy_row_identities(path: &Path) -> BTreeMap<String, Vec<String>> {
    let identities: &[(&str, &str)] = &[
        ("install_records", "scope, skill_id, resolved_revision"),
        (
            "projection_records",
            "scope, skill_id, target, physical_root",
        ),
        ("update_checks", "scope, skill_id, checked_at, outcome"),
        ("local_modifications", "scope, skill_id, detected_at, kind"),
        ("pins", "scope, skill_id, resolved_revision"),
        (
            "rollback_records",
            "scope, skill_id, rolled_back_at, to_reference",
        ),
        (
            "history_events",
            "kind, scope, skill_id, occurred_at, summary",
        ),
        ("telemetry_settings", "consent, updated_at"),
    ];

    let connection = Connection::open(path).expect("database opens");
    let mut rows = BTreeMap::new();
    for (table, columns) in identities {
        let mut statement = connection
            .prepare(&format!(
                "SELECT {columns} FROM [{table}] ORDER BY {columns}"
            ))
            .expect("identity query prepares");
        let count = statement.column_count();
        let values = statement
            .query_map([], |row| {
                (0..count)
                    .map(|index| row.get::<_, Option<String>>(index))
                    .collect::<Result<Vec<_>, _>>()
            })
            .expect("identity query runs")
            .collect::<Result<Vec<_>, _>>()
            .expect("identity rows read");
        rows.insert(
            (*table).to_string(),
            values
                .into_iter()
                .map(|row| format!("{row:?}"))
                .collect::<Vec<_>>(),
        );
    }
    rows
}

fn legacy_table_names(path: &Path) -> Vec<String> {
    let connection = Connection::open(path).expect("database opens");
    let mut statement = connection
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name")
        .expect("table query prepares");
    statement
        .query_map([], |row| row.get::<_, String>(0))
        .expect("table query runs")
        .collect::<Result<Vec<_>, _>>()
        .expect("table names read")
}

fn create_legacy_v1_database(path: &Path) {
    let connection = Connection::open(path).expect("legacy database opens");
    connection
        .execute_batch(LEGACY_V1_SCHEMA)
        .expect("legacy schema installs");
}

#[test]
fn schema_version_advances_only_inside_the_migration_transaction() {
    let temp = tempdir().expect("tempdir exists");
    let path = temp.path().join("state.db");
    create_legacy_v1_database(&path);

    let mut connection = Connection::open(&path).expect("database opens");
    let transaction = connection.transaction().expect("transaction begins");
    transaction
        .execute_batch("ALTER TABLE install_records RENAME TO install_records_v1")
        .expect("rename runs");
    transaction
        .pragma_update(None, "user_version", CURRENT_LOCAL_STATE_VERSION)
        .expect("version advances");
    transaction.rollback().expect("transaction rolls back");

    let version = connection
        .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
        .expect("version reads");
    assert_eq!(
        version, 1,
        "an uncommitted version advance must not persist"
    );

    let renamed: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name = 'install_records_v1'",
            [],
            |row| row.get(0),
        )
        .expect("rename check runs");
    assert_eq!(renamed, 0, "an uncommitted rename must not persist");
}

#[test]
fn migration_failure_at_every_boundary_preserves_the_previous_state() {
    for stage in MIGRATION_STAGES {
        let temp = tempdir().expect("tempdir exists");
        let path = temp.path().join("state.db");
        create_legacy_v1_database(&path);
        let before = legacy_row_identities(&path);
        let tables_before = legacy_table_names(&path);

        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).expect("workspace exists");
        let workspace_key = workspace_key_for_path(&workspace).expect("workspace key resolves");

        let mut connection = Connection::open(&path).expect("database opens");
        let expected_stage = *stage;
        let error = migrate_schema(
            &mut connection,
            &path,
            &workspace_key,
            1,
            CURRENT_LOCAL_STATE_VERSION,
            &|reached| {
                if reached == expected_stage {
                    Err(AppError::LocalStateValidation {
                        path: path.clone(),
                        message: format!("injected failure at {}", reached.as_str()),
                    })
                } else {
                    Ok(())
                }
            },
        )
        .expect_err("injected migration failure surfaces");

        assert!(
            error.to_string().contains("injected failure"),
            "unexpected error at {:?}: {error}",
            expected_stage
        );

        let after = legacy_row_identities(&path);
        assert_eq!(
            before, after,
            "row counts and identities must survive a failure at {expected_stage:?}"
        );
        assert_eq!(
            legacy_table_names(&path),
            tables_before,
            "the legacy tables must still use their original names after {expected_stage:?}"
        );
        let store = LocalStateStore::open_at_for(&path, &workspace)
            .unwrap_or_else(|error| panic!("reopen after {expected_stage:?} failed: {error}"));
        assert_eq!(
            store.schema_version().expect("schema version reads"),
            CURRENT_LOCAL_STATE_VERSION,
            "the next open must still complete the migration after {expected_stage:?}"
        );
    }
}

#[test]
fn migration_copies_every_table_with_counts_and_identities() {
    let temp = tempdir().expect("tempdir exists");
    let path = temp.path().join("state.db");
    create_legacy_v1_database(&path);

    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).expect("workspace exists");
    let store = LocalStateStore::open_at_for(&path, &workspace).expect("migration succeeds");

    assert_eq!(
        store.schema_version().expect("schema version reads"),
        CURRENT_LOCAL_STATE_VERSION
    );

    let installs = store.list_install_records().expect("installs load");
    let ids = installs
        .iter()
        .map(|record| {
            (
                record.skill.scope.as_str().to_string(),
                record.skill.skill_id.clone(),
                record.resolved_revision.clone(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        ids,
        vec![
            (
                "workspace".to_string(),
                "release-notes".to_string(),
                "aaaaaaaaaaaaaaaa".to_string()
            ),
            (
                "user".to_string(),
                "ai-sdk".to_string(),
                "cccccccccccccccc".to_string()
            ),
        ],
        "install identities and order must survive the migration"
    );

    let skill = ManagedSkillRef::new(ManagedScope::Workspace, "release-notes");
    assert_eq!(
        store
            .projection_records(Some(&skill))
            .expect("projections load")
            .len(),
        1
    );
    assert_eq!(
        store
            .update_checks(Some(&skill))
            .expect("update checks load")
            .len(),
        1
    );
    assert_eq!(
        store
            .local_modifications(Some(&skill))
            .expect("modifications load")
            .len(),
        1
    );
    assert!(store.pin_record(&skill).expect("pin loads").is_some());
    assert_eq!(
        store
            .rollback_records(Some(&skill))
            .expect("rollbacks load")
            .len(),
        1
    );
    assert_eq!(
        store
            .history_entries(&HistoryQuery {
                skill: Some(skill),
                limit: None,
            })
            .expect("history loads")
            .len(),
        2
    );
    assert_eq!(
        store
            .telemetry_settings()
            .expect("telemetry loads")
            .expect("telemetry settings exist")
            .consent,
        TelemetryConsent::Enabled
    );

    for table in legacy_table_names(&path) {
        assert!(
            !table.ends_with("_v1"),
            "legacy table {table} must be dropped after migration"
        );
    }
}

fn create_current_database(path: &Path, workspace: &Path) {
    fs::create_dir_all(workspace).expect("workspace exists");
    let _store = LocalStateStore::open_at_for(path, workspace).expect("current schema installs");
}

#[test]
fn stranded_unclaimed_legacy_rows_are_recovered_on_open() {
    let temp = tempdir().expect("tempdir exists");
    let path = temp.path().join("state.db");
    let workspace = temp.path().join("workspace");
    create_current_database(&path, &workspace);

    let connection = Connection::open(&path).expect("database opens");
    connection
        .execute_batch(
            "CREATE TABLE install_records_v1 (
                scope TEXT NOT NULL, skill_id TEXT NOT NULL, source_kind TEXT NOT NULL,
                source_url TEXT NOT NULL, source_subpath TEXT NOT NULL,
                resolved_revision TEXT NOT NULL, upstream_revision TEXT, content_hash TEXT NOT NULL,
                overlay_hash TEXT NOT NULL, effective_version_hash TEXT NOT NULL,
                installed_at TEXT NOT NULL, updated_at TEXT NOT NULL, detached INTEGER NOT NULL,
                forked INTEGER NOT NULL
             );
             INSERT INTO install_records_v1 VALUES
                ('workspace', 'release-notes', 'git', 'https://example.com/release-notes.git',
                 '.agents/skills/release-notes', 'aaaaaaaaaaaaaaaa', NULL, 'sha256:content',
                 'sha256:none', 'sha256:effective-a', '2026-03-19T12:00:00Z',
                 '2026-03-19T12:00:00Z', 0, 0);",
        )
        .expect("stranded legacy table installs");
    drop(connection);

    let store = LocalStateStore::open_at_for(&path, &workspace).expect("stranded recovery opens");
    let install = store
        .install_record(&ManagedSkillRef::new(
            ManagedScope::Workspace,
            "release-notes",
        ))
        .expect("install loads")
        .expect("recovered install exists");
    assert_eq!(install.resolved_revision, "aaaaaaaaaaaaaaaa");

    let remaining: i64 = {
        let mut statement = store
            .connection
            .prepare("SELECT COUNT(*) FROM sqlite_master WHERE name = 'install_records_v1'")
            .expect("table query prepares");
        statement
            .query_row([], |row| row.get(0))
            .expect("table count reads")
    };
    assert_eq!(remaining, 0, "the recovered legacy table is dropped");
}

#[test]
fn stranded_legacy_rows_already_present_are_dropped_without_duplicates() {
    let temp = tempdir().expect("tempdir exists");
    let path = temp.path().join("state.db");
    let workspace = temp.path().join("workspace");
    create_current_database(&path, &workspace);

    let mut store = LocalStateStore::open_at_for(&path, &workspace).expect("store opens");
    store
        .upsert_install_record(&InstallRecord {
            skill: ManagedSkillRef::new(ManagedScope::Workspace, "release-notes"),
            source_kind: SourceKind::Git,
            source_url: "https://example.com/release-notes.git".to_string(),
            source_subpath: ".agents/skills/release-notes".to_string(),
            resolved_revision: "aaaaaaaaaaaaaaaa".to_string(),
            upstream_revision: None,
            content_hash: "sha256:content".to_string(),
            overlay_hash: "sha256:none".to_string(),
            effective_version_hash: "sha256:effective-a".to_string(),
            installed_at: "2026-03-19T12:00:00Z".to_string(),
            updated_at: "2026-03-19T12:00:00Z".to_string(),
            detached: false,
            forked: false,
        })
        .expect("current install writes");
    drop(store);

    let connection = Connection::open(&path).expect("database opens");
    connection
        .execute_batch(
            "CREATE TABLE install_records_v1 (
                scope TEXT NOT NULL, skill_id TEXT NOT NULL, source_kind TEXT NOT NULL,
                source_url TEXT NOT NULL, source_subpath TEXT NOT NULL,
                resolved_revision TEXT NOT NULL, upstream_revision TEXT, content_hash TEXT NOT NULL,
                overlay_hash TEXT NOT NULL, effective_version_hash TEXT NOT NULL,
                installed_at TEXT NOT NULL, updated_at TEXT NOT NULL, detached INTEGER NOT NULL,
                forked INTEGER NOT NULL
             );
             INSERT INTO install_records_v1 VALUES
                ('workspace', 'release-notes', 'git', 'https://example.com/release-notes.git',
                 '.agents/skills/release-notes', 'aaaaaaaaaaaaaaaa', NULL, 'sha256:content',
                 'sha256:none', 'sha256:effective-a', '2026-03-19T12:00:00Z',
                 '2026-03-19T12:00:00Z', 0, 0);",
        )
        .expect("already-migrated legacy table installs");
    drop(connection);

    let store = LocalStateStore::open_at_for(&path, &workspace).expect("reopen succeeds");
    let installs = store.list_install_records().expect("installs load");
    assert_eq!(installs.len(), 1, "no duplicate rows are created");
}

#[test]
fn stranded_legacy_rows_owned_by_another_workspace_are_refused_untouched() {
    let temp = tempdir().expect("tempdir exists");
    let path = temp.path().join("state.db");
    let owner = temp.path().join("owner");
    let other = temp.path().join("other");
    create_current_database(&path, &owner);

    fs::create_dir_all(&other).expect("other workspace exists");

    // Another workspace already claimed the row under its own key.
    let connection = Connection::open(&path).expect("database opens");
    connection
        .execute_batch(
            "INSERT INTO install_records VALUES
                ('workspace', 'foreign-key', 'release-notes', 'git',
                 'https://example.com/release-notes.git', '.agents/skills/release-notes',
                 'aaaaaaaaaaaaaaaa', NULL, 'sha256:content', 'sha256:none',
                 'sha256:effective-a', '2026-03-19T12:00:00Z', '2026-03-19T12:00:00Z', 0, 0);
             CREATE TABLE install_records_v1 (
                scope TEXT NOT NULL, skill_id TEXT NOT NULL, source_kind TEXT NOT NULL,
                source_url TEXT NOT NULL, source_subpath TEXT NOT NULL,
                resolved_revision TEXT NOT NULL, upstream_revision TEXT, content_hash TEXT NOT NULL,
                overlay_hash TEXT NOT NULL, effective_version_hash TEXT NOT NULL,
                installed_at TEXT NOT NULL, updated_at TEXT NOT NULL, detached INTEGER NOT NULL,
                forked INTEGER NOT NULL
             );
             INSERT INTO install_records_v1 VALUES
                ('workspace', 'release-notes', 'git', 'https://example.com/release-notes.git',
                 '.agents/skills/release-notes', 'aaaaaaaaaaaaaaaa', NULL, 'sha256:content',
                 'sha256:none', 'sha256:effective-a', '2026-03-19T12:00:00Z',
                 '2026-03-19T12:00:00Z', 0, 0);",
        )
        .expect("ambiguous stranded state installs");
    drop(connection);

    let before = legacy_row_identities(&path);
    let error =
        LocalStateStore::open_at_for(&path, &other).expect_err("ambiguous ownership is refused");
    let message = error.to_string();
    assert!(
        message.contains("owning workspace cannot be determined"),
        "unexpected error: {message}"
    );
    assert!(
        message.contains("doctor --repair-state"),
        "the error must point at the recovery entry point: {message}"
    );

    // The refusal must not change any rows.
    assert_eq!(legacy_row_identities(&path), before);
}

#[test]
fn stranded_legacy_rows_changed_after_copy_are_refused() {
    let temp = tempdir().expect("tempdir exists");
    let path = temp.path().join("state.db");
    let workspace = temp.path().join("workspace");
    create_current_database(&path, &workspace);

    // The current row claims the same identity with different content, so a
    // copy would overwrite a row another writer changed.
    let workspace_key = workspace_key_for_path(&workspace).expect("workspace key resolves");
    let connection = Connection::open(&path).expect("database opens");
    connection
        .execute_batch(&format!(
            "INSERT INTO install_records VALUES
                ('workspace', '{workspace_key}', 'release-notes', 'git',
                 'https://example.com/release-notes.git', '.agents/skills/release-notes',
                 'dddddddddddddddd', NULL, 'sha256:content', 'sha256:none',
                 'sha256:effective-d', '2026-03-19T12:00:00Z', '2026-03-19T12:00:00Z', 0, 0);
             CREATE TABLE install_records_v1 (
                scope TEXT NOT NULL, skill_id TEXT NOT NULL, source_kind TEXT NOT NULL,
                source_url TEXT NOT NULL, source_subpath TEXT NOT NULL,
                resolved_revision TEXT NOT NULL, upstream_revision TEXT, content_hash TEXT NOT NULL,
                overlay_hash TEXT NOT NULL, effective_version_hash TEXT NOT NULL,
                installed_at TEXT NOT NULL, updated_at TEXT NOT NULL, detached INTEGER NOT NULL,
                forked INTEGER NOT NULL
             );
             INSERT INTO install_records_v1 VALUES
                ('workspace', 'release-notes', 'git', 'https://example.com/release-notes.git',
                 '.agents/skills/release-notes', 'aaaaaaaaaaaaaaaa', NULL, 'sha256:content',
                 'sha256:none', 'sha256:effective-a', '2026-03-19T12:00:00Z',
                 '2026-03-19T12:00:00Z', 0, 0);",
        ))
        .expect("conflicting stranded state installs");
    drop(connection);

    let error = LocalStateStore::open_at_for(&path, &workspace)
        .expect_err("conflicting identity is refused");
    assert!(
        error
            .to_string()
            .contains("same identity but different content"),
        "unexpected error: {error}"
    );
}

#[test]
fn journal_snapshots_and_restores_paths_and_the_state_database() {
    let temp = tempdir().expect("tempdir exists");
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).expect("workspace exists");

    let database = temp.path().join("state.db");
    let mut store = LocalStateStore::open_at_for(&database, &workspace).expect("store opens");
    store
        .upsert_install_record(&InstallRecord {
            skill: ManagedSkillRef::new(ManagedScope::Workspace, "release-notes"),
            source_kind: SourceKind::Git,
            source_url: "https://example.com/release-notes.git".to_string(),
            source_subpath: ".agents/skills/release-notes".to_string(),
            resolved_revision: "aaaaaaaaaaaaaaaa".to_string(),
            upstream_revision: None,
            content_hash: "sha256:content".to_string(),
            overlay_hash: "sha256:none".to_string(),
            effective_version_hash: "sha256:effective-a".to_string(),
            installed_at: "2026-03-19T12:00:00Z".to_string(),
            updated_at: "2026-03-19T12:00:00Z".to_string(),
            detached: false,
            forked: false,
        })
        .expect("install writes");
    drop(store);

    let changed_file = workspace.join("skillctl.yaml");
    fs::write(&changed_file, "version: 1\n").expect("file writes");
    let removed_file = workspace.join("removed.txt");
    fs::write(&removed_file, "before\n").expect("file writes");

    let workspace_key = workspace_key_for_path(&workspace).expect("workspace key resolves");
    let mut journal = OperationJournal::create(
        &journal_root(temp.path()),
        &workspace_key,
        &workspace,
        "probe",
    )
    .expect("journal creates");
    journal.track_path(&changed_file).expect("file is staged");
    journal.track_path(&removed_file).expect("file is staged");
    journal
        .track_state_database(&database)
        .expect("database is staged");

    // Simulate the mutation an interrupted operation leaves behind.
    fs::write(&changed_file, "version: 2\n").expect("file changes");
    fs::remove_file(&removed_file).expect("file disappears");
    let mut store = LocalStateStore::open_at_for(&database, &workspace).expect("store reopens");
    store
        .delete_current_skill_state(&ManagedSkillRef::new(
            ManagedScope::Workspace,
            "release-notes",
        ))
        .expect("install is deleted");
    store
        .append_history_entry(&HistoryEntry {
            id: None,
            kind: HistoryEventKind::Cleanup,
            scope: None,
            skill_id: None,
            target: None,
            occurred_at: "2026-11-05T00:00:00Z".to_string(),
            summary: "interrupted work".to_string(),
            details: BTreeMap::new(),
        })
        .expect("history writes");
    drop(store);

    journal.restore().expect("restore runs");

    assert_eq!(
        fs::read_to_string(&changed_file).expect("file reads"),
        "version: 1\n",
        "tracked files return to their pre-operation content"
    );
    assert_eq!(
        fs::read_to_string(&removed_file).expect("file reads"),
        "before\n",
        "files removed by the operation are restored"
    );
    let store = LocalStateStore::open_at_for(&database, &workspace).expect("store opens");
    assert!(
        store
            .install_record(&ManagedSkillRef::new(
                ManagedScope::Workspace,
                "release-notes"
            ))
            .expect("install loads")
            .is_some(),
        "database rows return to their pre-operation state"
    );
    assert!(
        store
            .history_entries(&HistoryQuery {
                skill: None,
                limit: None
            })
            .expect("history loads")
            .is_empty(),
        "history written by the interrupted operation is removed"
    );
}

#[test]
fn pending_journals_are_recovered_after_a_termination() {
    let temp = tempdir().expect("tempdir exists");
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).expect("workspace exists");
    let workspace_key = workspace_key_for_path(&workspace).expect("workspace key resolves");
    let journal_root = journal_root(temp.path());

    let target = workspace.join("generated.txt");
    fs::write(&target, "old\n").expect("file writes");

    let mut journal =
        OperationJournal::create(&journal_root, &workspace_key, &workspace, "install")
            .expect("journal creates");
    journal.track_path(&target).expect("path is staged");
    fs::write(&target, "new\n").expect("operation mutates the path");

    let pending = pending_operations(&journal_root).expect("pending journals list");
    assert_eq!(pending.len(), 1, "an interrupted operation stays visible");
    assert_eq!(pending[0].operation, "install");
    assert_eq!(pending[0].state, "active");
    assert!(
        pending[0]
            .tracked_paths
            .contains(&target.to_string_lossy().into_owned()),
        "the tracked path is reported: {:?}",
        pending[0].tracked_paths
    );

    let outcomes = recover_pending_operations(&journal_root).expect("recovery runs");
    assert_eq!(outcomes.len(), 1, "one journal is recovered");
    assert!(matches!(outcomes[0], RecoveryOutcome::Recovered(_)));
    assert_eq!(
        fs::read_to_string(&target).expect("file reads"),
        "old\n",
        "recovery restores the pre-operation state"
    );
    assert!(
        pending_operations(&journal_root)
            .expect("pending journals list")
            .is_empty(),
        "the recovered journal is removed"
    );
}

#[test]
fn journals_without_a_database_snapshot_need_review() {
    let temp = tempdir().expect("tempdir exists");
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).expect("workspace exists");
    let workspace_key = workspace_key_for_path(&workspace).expect("workspace key resolves");
    let journal_root = journal_root(temp.path());

    let database = temp.path().join("state.db");
    let mut store = LocalStateStore::open_at_for(&database, &workspace).expect("store opens");
    store
        .upsert_telemetry_settings(&TelemetrySettings {
            consent: TelemetryConsent::Disabled,
            notice_seen_at: None,
            updated_at: "2026-11-05T00:00:00Z".to_string(),
        })
        .expect("telemetry writes");
    drop(store);

    let mut journal =
        OperationJournal::create(&journal_root, &workspace_key, &workspace, "install")
            .expect("journal creates");
    journal
        .track_state_database(&database)
        .expect("database is staged");
    drop(journal);

    // A crash that loses the staged snapshot leaves an unrecoverable journal.
    let pending = pending_operations(&journal_root)
        .expect("pending journals list")
        .pop()
        .expect("the journal is pending");
    fs::remove_file(pending.directory.join("state-snapshot.db")).expect("snapshot is removed");

    let outcomes = recover_pending_operations(&journal_root).expect("recovery runs");
    assert_eq!(outcomes.len(), 1);
    match &outcomes[0] {
        RecoveryOutcome::NeedsReview { reason, .. } => {
            assert!(
                reason.contains("snapshot is missing"),
                "unexpected reason: {reason}"
            );
        }
        other => panic!("expected a needs-review outcome, got {other:?}"),
    }
    assert!(
        !pending_operations(&journal_root)
            .expect("pending journals list")
            .is_empty(),
        "an unrecoverable journal stays on disk for review"
    );
}

#[test]
fn migration_backups_keep_only_the_newest_copies() {
    let temp = tempdir().expect("tempdir exists");
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).expect("workspace exists");
    let database = temp.path().join("state.db");
    let _store = LocalStateStore::open_at_for(&database, &workspace).expect("store opens");
    drop(_store);

    let state_root = temp.path().to_path_buf();
    for _ in 0..5 {
        journal::record_migration_backup(&database, 2, 2).expect("backup records");
    }

    let retained = journal::migration_backups(&state_root);
    assert!(
        retained.len() <= 3,
        "only the newest backups are retained, found {retained:?}"
    );
    assert!(!retained.is_empty(), "at least one backup is retained");
}

#[test]
fn files_are_replaced_through_a_staged_write() {
    let temp = tempdir().expect("tempdir exists");
    let directory = temp.path().join("nested");
    fs::create_dir_all(&directory).expect("directory exists");
    let path = directory.join("state.yaml");
    fs::write(&path, "one\n").expect("file writes");

    journal::write_file_atomically(&path, b"two\n").expect("write stages and replaces");
    assert_eq!(fs::read_to_string(&path).expect("file reads"), "two\n");

    let leftovers = fs::read_dir(path.parent().expect("parent exists"))
        .expect("directory reads")
        .filter_map(std::result::Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(".skillctl-stage")
        })
        .count();
    assert_eq!(leftovers, 0, "staged files are renamed away");
}

#[test]
fn coordination_lock_is_reentrant_for_nested_operations() {
    let temp = tempdir().expect("tempdir exists");
    let database = temp.path().join("state.db");
    fs::write(&database, b"placeholder").expect("database exists");

    let outer = super::lock::acquire(&database, "outer").expect("outer acquires");
    let inner = super::lock::acquire(&database, "inner").expect("inner acquires reentrantly");
    drop(inner);
    drop(outer);

    let probe = fs::File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(super::lock::lock_path_for(&database).expect("lock path resolves"))
        .expect("lock file opens");
    assert!(
        probe.try_lock().is_ok(),
        "the lock is free once every guard is dropped"
    );
}

#[test]
fn state_connections_wait_longer_than_the_default_busy_timeout() {
    let temp = tempdir().expect("tempdir exists");
    let store = LocalStateStore::open_at(temp.path().join("state.db")).expect("state store opens");

    let timeout: i64 = store
        .connection
        .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
        .expect("busy timeout reads");
    assert_eq!(
        timeout,
        DEFAULT_STATE_BUSY_TIMEOUT.as_millis() as i64,
        "every state connection must wait out a starved writer's commit: {timeout} ms"
    );

    // The in-memory store shares the same configuration, so logic callers and
    // file-backed callers see the same waiting behavior.
    let memory = LocalStateStore::open_in_memory().expect("in-memory store opens");
    let memory_timeout: i64 = memory
        .connection
        .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
        .expect("in-memory busy timeout reads");
    assert_eq!(
        memory_timeout,
        DEFAULT_STATE_BUSY_TIMEOUT.as_millis() as i64
    );
}

#[test]
fn busy_timeout_falls_back_to_the_default_when_unset_or_invalid() {
    assert_eq!(
        state_busy_timeout_from_env(None),
        DEFAULT_STATE_BUSY_TIMEOUT
    );
    assert_eq!(
        state_busy_timeout_from_env(Some("")),
        DEFAULT_STATE_BUSY_TIMEOUT
    );
    assert_eq!(
        state_busy_timeout_from_env(Some("not-a-number")),
        DEFAULT_STATE_BUSY_TIMEOUT
    );
}

#[test]
fn busy_timeout_accepts_an_override() {
    assert_eq!(
        state_busy_timeout_from_env(Some("1500")),
        Duration::from_millis(1500)
    );
    assert_eq!(
        state_busy_timeout_from_env(Some(" 1500 ")),
        Duration::from_millis(1500)
    );
    assert_eq!(
        state_busy_timeout_from_env(Some("0")),
        Duration::from_millis(1),
        "a zero override still allows one retry instead of disabling the wait"
    );
}

/// Take one real busy failure from a second connection that cannot proceed.
///
/// The holder keeps one exclusive transaction open, and the probe waits one
/// millisecond, so SQLite returns the same `DatabaseBusy` code a reader meets
/// when its busy timeout runs out.
fn busy_error_from_a_held_writer(database: &std::path::Path) -> rusqlite::Error {
    let holder = Connection::open(database).expect("holder opens the database");
    holder
        .execute_batch("BEGIN EXCLUSIVE;")
        .expect("holder takes the exclusive lock");

    let probe = Connection::open(database).expect("probe opens the database");
    probe
        .busy_timeout(Duration::from_millis(1))
        .expect("probe shortens its wait");
    let error = probe
        .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
        .expect_err("the probe meets the held writer");

    holder
        .execute_batch("ROLLBACK;")
        .expect("holder releases the lock");
    error
}

#[test]
fn a_busy_timeout_failure_names_the_wait_controls() {
    let temp = tempdir().expect("tempdir exists");
    let database = temp.path().join("state.db");
    LocalStateStore::open_at(&database).expect("state store opens");
    let source = busy_error_from_a_held_writer(&database);

    assert!(
        schema::is_busy_timeout_error(&source),
        "SQLite reported a busy lock: {source}"
    );

    let error = local_state_query(&database, "read schema version", source);
    assert!(
        matches!(error, AppError::LocalStateBusyTimeout { .. }),
        "a busy lock must not surface as a plain query failure: {error}"
    );
    assert_eq!(
        error.exit_status(),
        crate::error::ExitStatus::OperationalError
    );

    let message = error.to_string();
    assert!(
        message.contains("SKILLCTL_STATE_BUSY_TIMEOUT_MS"),
        "the failure names the wait it exhausted: {message}"
    );
    assert!(
        message.contains("SKILLCTL_LOCK_TIMEOUT_MS"),
        "the failure separates the coordination lock wait: {message}"
    );
    assert!(
        message.contains("database is locked"),
        "the failure keeps the SQLite text: {message}"
    );
    assert!(
        message.contains("read schema version"),
        "the failure names the operation that waited: {message}"
    );
    assert!(
        message.contains(&*database.to_string_lossy()),
        "the failure names the database: {message}"
    );
}

#[test]
fn other_query_failures_keep_the_plain_query_error() {
    let temp = tempdir().expect("tempdir exists");
    let database = temp.path().join("state.db");

    let error = local_state_query(
        &database,
        "read schema version",
        rusqlite::Error::QueryReturnedNoRows,
    );
    assert!(
        matches!(error, AppError::LocalStateQuery { .. }),
        "only a busy lock rewrites the failure: {error}"
    );
}

/// Schema of a schema-v2 state database: namespaced like the current schema,
/// but without `skill_reviews` and with the v2 history event kind list.
fn create_v2_database(path: &Path, workspace: &Path) -> String {
    let workspace_key = workspace_key_for_path(workspace).expect("workspace key resolves");
    let connection = Connection::open(path).expect("v2 database opens");
    connection
        .execute_batch(
            r"
CREATE TABLE install_records (
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
CREATE TABLE projection_records (
    scope TEXT NOT NULL CHECK (scope IN ('workspace', 'user')),
    workspace_key TEXT NOT NULL,
    skill_id TEXT NOT NULL,
    target TEXT NOT NULL CHECK (target IN
        ('codex', 'claude-code', 'github-copilot', 'gemini-cli', 'amp', 'opencode')),
    generation_mode TEXT NOT NULL CHECK (generation_mode IN ('copy', 'symlink')),
    physical_root TEXT NOT NULL,
    projected_path TEXT NOT NULL,
    effective_version_hash TEXT NOT NULL,
    generated_at TEXT NOT NULL,
    PRIMARY KEY (scope, workspace_key, skill_id, target, physical_root)
);
CREATE TABLE update_checks (
    id INTEGER PRIMARY KEY,
    scope TEXT NOT NULL CHECK (scope IN ('workspace', 'user')),
    workspace_key TEXT NOT NULL,
    skill_id TEXT NOT NULL,
    checked_at TEXT NOT NULL,
    pinned_revision TEXT NOT NULL,
    latest_revision TEXT,
    outcome TEXT NOT NULL CHECK (outcome IN
        ('up-to-date', 'update-available', 'blocked', 'detached', 'local-source', 'failed')),
    overlay_detected INTEGER NOT NULL CHECK (overlay_detected IN (0, 1)),
    local_modification_detected INTEGER NOT NULL
        CHECK (local_modification_detected IN (0, 1)),
    notes TEXT
);
CREATE TABLE local_modifications (
    id INTEGER PRIMARY KEY,
    scope TEXT NOT NULL CHECK (scope IN ('workspace', 'user')),
    workspace_key TEXT NOT NULL,
    skill_id TEXT NOT NULL,
    detected_at TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('overlay', 'projected-copy', 'detached-fork')),
    path TEXT,
    details TEXT
);
CREATE TABLE pins (
    scope TEXT NOT NULL CHECK (scope IN ('workspace', 'user')),
    workspace_key TEXT NOT NULL,
    skill_id TEXT NOT NULL,
    requested_reference TEXT NOT NULL,
    resolved_revision TEXT NOT NULL,
    effective_version_hash TEXT,
    pinned_at TEXT NOT NULL,
    PRIMARY KEY (scope, workspace_key, skill_id)
);
CREATE TABLE rollback_records (
    id INTEGER PRIMARY KEY,
    scope TEXT NOT NULL CHECK (scope IN ('workspace', 'user')),
    workspace_key TEXT NOT NULL,
    skill_id TEXT NOT NULL,
    rolled_back_at TEXT NOT NULL,
    from_reference TEXT NOT NULL,
    to_reference TEXT NOT NULL
);
CREATE TABLE telemetry_settings (
    singleton_id INTEGER PRIMARY KEY CHECK (singleton_id = 1),
    consent TEXT NOT NULL CHECK (consent IN ('unknown', 'enabled', 'disabled')),
    notice_seen_at TEXT,
    updated_at TEXT NOT NULL
);
CREATE TABLE history_events (
    id INTEGER PRIMARY KEY,
    workspace_key TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (
        kind IN ('install', 'update-check', 'update-applied', 'projection', 'pin', 'rollback',
                 'overlay-created', 'direct-modification-detected', 'detach', 'fork', 'cleanup',
                 'prune', 'telemetry-consent-changed')
    ),
    scope TEXT CHECK (scope IN ('workspace', 'user')),
    skill_id TEXT,
    target TEXT CHECK (target IS NULL OR target IN
        ('codex', 'claude-code', 'github-copilot', 'gemini-cli', 'amp', 'opencode')),
    occurred_at TEXT NOT NULL,
    summary TEXT NOT NULL,
    details_json TEXT
);
INSERT INTO install_records VALUES
    ('workspace', 'wk', 'delta', 'git', 'https://example.com/delta.git', '.agents/skills/delta',
     'aaaaaaaaaaaaaaaa', NULL, 'sha256:content', 'sha256:none', 'sha256:effective',
     '2026-11-06T09:00:00Z', '2026-11-06T09:00:00Z', 0, 0);
INSERT INTO history_events VALUES
    (1, 'wk', 'install', 'workspace', 'delta', NULL, '2026-11-06T09:00:00Z',
     'Installed delta', NULL);
",
        )
        .expect("v2 schema installs");
    // The rows must sit in the namespace this workspace opens, so the key is
    // rewritten after the schema exists instead of being interpolated into it.
    connection
        .execute_batch(&format!(
            "UPDATE install_records SET workspace_key = '{workspace_key}';
             UPDATE history_events SET workspace_key = '{workspace_key}';
             PRAGMA user_version = 2;"
        ))
        .expect("v2 rows are namespaced");

    workspace_key
}

#[test]
fn opening_a_v2_database_migrates_additively_and_keeps_every_row() {
    let temp = tempdir().expect("tempdir exists");
    let path = temp.path().join("state.db");
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).expect("workspace exists");
    create_v2_database(&path, &workspace);

    let mut store = LocalStateStore::open_at_for(&path, &workspace).expect("v2 database migrates");

    assert_eq!(
        store.schema_version().expect("schema version reads"),
        CURRENT_LOCAL_STATE_VERSION,
        "the additive migration advances the marker",
    );

    let installs = store.list_install_records().expect("installs load");
    assert_eq!(
        installs
            .iter()
            .map(|record| record.skill.skill_id.as_str())
            .collect::<Vec<_>>(),
        vec!["delta"],
        "the v2 install row survives the migration",
    );

    let skill = ManagedSkillRef::new(ManagedScope::Workspace, "delta");
    let history = store
        .history_entries(&HistoryQuery {
            skill: Some(skill.clone()),
            limit: None,
        })
        .expect("history loads");
    assert_eq!(
        history
            .iter()
            .map(|entry| entry.kind.as_str())
            .collect::<Vec<_>>(),
        vec!["install"],
        "the v2 history row survives the rebuilt kind constraint",
    );

    // The rebuilt `history_events` table must accept the kinds this version
    // writes, and the new table must be usable.
    let review = TrustReviewRecord {
        skill: skill.clone(),
        decision: ReviewDecision::Approved,
        resolved_revision: "aaaaaaaaaaaaaaaa".to_string(),
        effective_version_hash: "sha256:effective".to_string(),
        contains_scripts: true,
        reviewed_at: "2026-11-06T10:00:00Z".to_string(),
        note: None,
    };
    HistoryLedger::new(&mut store)
        .record_trust_review(&review)
        .expect("a review records against the migrated schema");
    assert_eq!(
        store.trust_review(&skill).expect("review loads"),
        Some(review),
    );
    assert_eq!(
        store
            .history_entries(&HistoryQuery {
                skill: Some(skill),
                limit: None,
            })
            .expect("history loads")
            .iter()
            .map(|entry| entry.kind.as_str())
            .collect::<Vec<_>>(),
        vec!["trust-review", "install"],
        "the migrated schema records the new history kind",
    );
}

/// Schema of a schema-v3 state database: the current schema with the v3 update
/// outcome list, which does not yet accept `commit-pinned`.
fn create_v3_database(path: &Path, workspace: &Path) {
    let mut store = LocalStateStore::open_at_for(path, workspace).expect("current schema installs");
    HistoryLedger::new(&mut store)
        .record_install_with_trust(
            &InstallRecord {
                skill: ManagedSkillRef::new(ManagedScope::Workspace, "delta"),
                source_kind: SourceKind::Git,
                source_url: "https://example.com/delta.git".to_string(),
                source_subpath: "skills/delta".to_string(),
                resolved_revision: "aaaaaaaaaaaaaaaa".to_string(),
                upstream_revision: None,
                content_hash: "sha256:content".to_string(),
                overlay_hash: "sha256:none".to_string(),
                effective_version_hash: "sha256:effective".to_string(),
                installed_at: "2026-11-06T09:00:00Z".to_string(),
                updated_at: "2026-11-06T09:00:00Z".to_string(),
                detached: false,
                forked: false,
            },
            &crate::trust::SkillTrust::local(false),
        )
        .expect("install row records");
    drop(store);

    Connection::open(path)
        .expect("v3 database opens")
        .execute_batch(
            "DROP INDEX IF EXISTS idx_update_checks_skill_time;
             ALTER TABLE update_checks RENAME TO update_checks_v3;
             CREATE TABLE update_checks (
                 id INTEGER PRIMARY KEY,
                 scope TEXT NOT NULL CHECK (scope IN ('workspace', 'user')),
                 workspace_key TEXT NOT NULL,
                 skill_id TEXT NOT NULL,
                 checked_at TEXT NOT NULL,
                 pinned_revision TEXT NOT NULL,
                 latest_revision TEXT,
                 outcome TEXT NOT NULL CHECK (outcome IN
                     ('up-to-date', 'update-available', 'blocked', 'detached', 'local-source',
                      'failed')),
                 overlay_detected INTEGER NOT NULL CHECK (overlay_detected IN (0, 1)),
                 local_modification_detected INTEGER NOT NULL
                     CHECK (local_modification_detected IN (0, 1)),
                 notes TEXT
             );
             DROP TABLE update_checks_v3;
             PRAGMA user_version = 3;",
        )
        .expect("v3 schema installs");
}

#[test]
fn opening_a_v3_database_widens_the_update_outcome_check_and_keeps_rows() {
    let temp = tempdir().expect("tempdir exists");
    let path = temp.path().join("state.db");
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).expect("workspace exists");
    create_v3_database(&path, &workspace);

    let mut store = LocalStateStore::open_at_for(&path, &workspace).expect("v3 database migrates");

    assert_eq!(
        store.schema_version().expect("schema version reads"),
        CURRENT_LOCAL_STATE_VERSION,
        "the additive migration advances the marker",
    );
    assert_eq!(
        store
            .list_install_records()
            .expect("installs load")
            .iter()
            .map(|record| record.skill.skill_id.as_str())
            .collect::<Vec<_>>(),
        vec!["delta"],
        "the v3 install row survives the migration",
    );

    // The rebuilt `update_checks` table must accept the outcome this version
    // writes, and the row must stay readable.
    let skill = ManagedSkillRef::new(ManagedScope::Workspace, "delta");
    HistoryLedger::new(&mut store)
        .record_update_check_with_trust(
            &UpdateCheckRecord {
                id: None,
                skill: skill.clone(),
                checked_at: "2026-11-06T10:00:00Z".to_string(),
                pinned_revision: "aaaaaaaaaaaaaaaa".to_string(),
                latest_revision: None,
                outcome: UpdateCheckOutcome::CommitPinned,
                overlay_detected: false,
                local_modification_detected: false,
                apply_blocked_by: None,
                notes: None,
            },
            None,
        )
        .expect("a commit-pinned check records against the migrated schema");
    assert_eq!(
        store
            .latest_update_check(&skill)
            .expect("update check loads")
            .expect("update check exists")
            .outcome,
        UpdateCheckOutcome::CommitPinned,
    );
}

/// Schema of a schema-v4 state database: the current tables without the column
/// that records why a check held back an apply.
fn create_v4_database(path: &Path, workspace: &Path) {
    let mut store = LocalStateStore::open_at_for(path, workspace).expect("current schema installs");
    HistoryLedger::new(&mut store)
        .record_update_check_with_trust(
            &UpdateCheckRecord {
                id: None,
                skill: ManagedSkillRef::new(ManagedScope::Workspace, "delta"),
                checked_at: "2026-11-07T10:00:00Z".to_string(),
                pinned_revision: "aaaaaaaaaaaaaaaa".to_string(),
                latest_revision: Some("bbbbbbbbbbbbbbbb".to_string()),
                outcome: UpdateCheckOutcome::UpdateAvailable,
                overlay_detected: false,
                local_modification_detected: false,
                apply_blocked_by: None,
                notes: None,
            },
            None,
        )
        .expect("v4 update check records");
    drop(store);

    Connection::open(path)
        .expect("v4 database opens")
        .execute_batch(
            "DROP INDEX IF EXISTS idx_update_checks_skill_time;
             ALTER TABLE update_checks RENAME TO update_checks_v4;
             CREATE TABLE update_checks (
                 id INTEGER PRIMARY KEY,
                 scope TEXT NOT NULL CHECK (scope IN ('workspace', 'user')),
                 workspace_key TEXT NOT NULL,
                 skill_id TEXT NOT NULL,
                 checked_at TEXT NOT NULL,
                 pinned_revision TEXT NOT NULL,
                 latest_revision TEXT,
                 outcome TEXT NOT NULL CHECK (outcome IN
                     ('up-to-date', 'update-available', 'blocked', 'detached', 'local-source',
                      'commit-pinned', 'failed')),
                 overlay_detected INTEGER NOT NULL CHECK (overlay_detected IN (0, 1)),
                 local_modification_detected INTEGER NOT NULL
                     CHECK (local_modification_detected IN (0, 1)),
                 notes TEXT
             );
             INSERT INTO update_checks (
                 id, scope, workspace_key, skill_id, checked_at, pinned_revision, latest_revision,
                 outcome, overlay_detected, local_modification_detected, notes
             )
             SELECT id, scope, workspace_key, skill_id, checked_at, pinned_revision,
                    latest_revision, outcome, overlay_detected, local_modification_detected, notes
               FROM update_checks_v4;
             DROP TABLE update_checks_v4;
             CREATE INDEX idx_update_checks_skill_time
                 ON update_checks (scope, workspace_key, skill_id, checked_at DESC, id DESC);
             PRAGMA user_version = 4;",
        )
        .expect("v4 schema installs");
}

/// Schema of a schema-v5 state database: the current tables without the
/// `well-known` value in the `install_records` source kind constraint.
fn create_v5_database(path: &Path, workspace: &Path) {
    let mut store = LocalStateStore::open_at_for(path, workspace).expect("current schema installs");
    store
        .upsert_install_record(&InstallRecord {
            skill: ManagedSkillRef::new(ManagedScope::Workspace, "delta"),
            source_kind: SourceKind::Git,
            source_url: "https://example.com/delta.git".to_string(),
            source_subpath: ".agents/skills/delta".to_string(),
            resolved_revision: "aaaaaaaaaaaaaaaa".to_string(),
            upstream_revision: None,
            content_hash: "sha256:content".to_string(),
            overlay_hash: "sha256:none".to_string(),
            effective_version_hash: "sha256:effective".to_string(),
            installed_at: "2026-11-07T09:00:00Z".to_string(),
            updated_at: "2026-11-07T09:00:00Z".to_string(),
            detached: false,
            forked: false,
        })
        .expect("v5 install records");
    drop(store);

    Connection::open(path)
        .expect("v5 database opens")
        .execute_batch(
            "ALTER TABLE install_records RENAME TO install_records_v5;
             CREATE TABLE install_records (
                 scope TEXT NOT NULL CHECK (scope IN ('workspace', 'user')),
                 workspace_key TEXT NOT NULL,
                 skill_id TEXT NOT NULL,
                 source_kind TEXT NOT NULL
                     CHECK (source_kind IN ('git', 'local-path', 'archive')),
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
             INSERT INTO install_records (
                 scope, workspace_key, skill_id, source_kind, source_url, source_subpath,
                 resolved_revision, upstream_revision, content_hash, overlay_hash,
                 effective_version_hash, installed_at, updated_at, detached, forked
             )
             SELECT scope, workspace_key, skill_id, source_kind, source_url, source_subpath,
                    resolved_revision, upstream_revision, content_hash, overlay_hash,
                    effective_version_hash, installed_at, updated_at, detached, forked
               FROM install_records_v5;
             DROP TABLE install_records_v5;
             PRAGMA user_version = 5;",
        )
        .expect("v5 schema installs");
}

#[test]
fn opening_a_v5_database_widens_the_source_kind_check_and_keeps_rows() {
    let temp = tempdir().expect("tempdir exists");
    let path = temp.path().join("state.db");
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).expect("workspace exists");
    create_v5_database(&path, &workspace);

    let mut store = LocalStateStore::open_at_for(&path, &workspace).expect("v5 database migrates");

    assert_eq!(
        store.schema_version().expect("schema version reads"),
        CURRENT_LOCAL_STATE_VERSION,
        "the additive migration advances the marker",
    );
    assert_eq!(
        store
            .list_install_records()
            .expect("installs load")
            .iter()
            .map(|record| record.skill.skill_id.as_str())
            .collect::<Vec<_>>(),
        vec!["delta"],
        "the v5 install row survives the rebuild",
    );

    // The rebuilt table must accept the source kind this version writes.
    store
        .upsert_install_record(&InstallRecord {
            skill: ManagedSkillRef::new(ManagedScope::Workspace, "code-review"),
            source_kind: SourceKind::WellKnown,
            source_url: "https://skills.example.com/.well-known/agent-skills/index.json"
                .to_string(),
            source_subpath: "skills/code-review".to_string(),
            resolved_revision:
                "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                    .to_string(),
            upstream_revision: None,
            content_hash: "sha256:content".to_string(),
            overlay_hash: "sha256:none".to_string(),
            effective_version_hash: "sha256:effective".to_string(),
            installed_at: "2026-11-07T10:00:00Z".to_string(),
            updated_at: "2026-11-07T10:00:00Z".to_string(),
            detached: false,
            forked: false,
        })
        .expect("a well-known install records against the migrated schema");

    let record = store
        .install_record(&ManagedSkillRef::new(
            ManagedScope::Workspace,
            "code-review",
        ))
        .expect("record loads")
        .expect("the record exists");
    assert_eq!(record.source_kind, SourceKind::WellKnown);
}

#[test]
fn opening_a_v4_database_adds_the_check_block_column_and_keeps_rows() {
    let temp = tempdir().expect("tempdir exists");
    let path = temp.path().join("state.db");
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).expect("workspace exists");
    create_v4_database(&path, &workspace);

    let mut store = LocalStateStore::open_at_for(&path, &workspace).expect("v4 database migrates");

    assert_eq!(
        store.schema_version().expect("schema version reads"),
        CURRENT_LOCAL_STATE_VERSION,
        "the additive migration advances the marker",
    );

    let skill = ManagedSkillRef::new(ManagedScope::Workspace, "delta");
    let migrated = store
        .latest_update_check(&skill)
        .expect("update check loads")
        .expect("the v4 row survives the rebuild");
    assert_eq!(migrated.outcome, UpdateCheckOutcome::UpdateAvailable);
    assert_eq!(
        migrated.latest_revision.as_deref(),
        Some("bbbbbbbbbbbbbbbb"),
        "the migrated row keeps the revision it observed",
    );
    assert_eq!(
        migrated.apply_blocked_by, None,
        "a row the older version wrote holds no block reason",
    );

    // The rebuilt table must accept the block reason this version writes.
    HistoryLedger::new(&mut store)
        .record_update_check_with_trust(
            &UpdateCheckRecord {
                id: None,
                skill: skill.clone(),
                checked_at: "2026-11-07T10:05:00Z".to_string(),
                pinned_revision: "aaaaaaaaaaaaaaaa".to_string(),
                latest_revision: Some("bbbbbbbbbbbbbbbb".to_string()),
                outcome: UpdateCheckOutcome::Blocked,
                overlay_detected: false,
                local_modification_detected: true,
                apply_blocked_by: Some(UpdateCheckBlockReason::IncomingScripts),
                notes: None,
            },
            None,
        )
        .expect("a blocked check records against the migrated schema");
    assert_eq!(
        store
            .latest_update_check(&skill)
            .expect("update check loads")
            .expect("update check exists")
            .apply_blocked_by,
        Some(UpdateCheckBlockReason::IncomingScripts),
    );
}

#[test]
fn a_failed_additive_migration_keeps_the_v2_database_untouched() {
    for stage in [
        MigrationStage::AfterSchemaCreation,
        MigrationStage::AfterVersionAdvance,
    ] {
        let temp = tempdir().expect("tempdir exists");
        let path = temp.path().join("state.db");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).expect("workspace exists");
        create_v2_database(&path, &workspace);
        let before = legacy_row_identities(&path);
        let workspace_key = workspace_key_for_path(&workspace).expect("workspace key resolves");

        let mut connection = Connection::open(&path).expect("database opens");
        let expected_stage = stage;
        let error = migrate_schema(
            &mut connection,
            &path,
            &workspace_key,
            2,
            CURRENT_LOCAL_STATE_VERSION,
            &|reached| {
                if reached == expected_stage {
                    Err(AppError::LocalStateValidation {
                        path: path.clone(),
                        message: format!("injected failure at {}", reached.as_str()),
                    })
                } else {
                    Ok(())
                }
            },
        )
        .expect_err("injected additive migration failure surfaces");

        assert!(
            error.to_string().contains("injected failure"),
            "unexpected error at {expected_stage:?}: {error}",
        );
        assert_eq!(
            legacy_row_identities(&path),
            before,
            "the v2 rows must survive a failure at {expected_stage:?}",
        );
        assert_eq!(
            connection
                .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
                .expect("version reads"),
            2,
            "a rolled-back additive migration must keep the v2 marker at {expected_stage:?}",
        );

        // The next open finishes the migration, so an interruption never
        // strands the database.
        let store = LocalStateStore::open_at_for(&path, &workspace)
            .unwrap_or_else(|error| panic!("reopen after {expected_stage:?} failed: {error}"));
        assert_eq!(
            store.schema_version().expect("schema version reads"),
            CURRENT_LOCAL_STATE_VERSION,
        );
    }
}

#[test]
fn database_health_classifies_missing_and_readable_databases() {
    let temp = tempdir().expect("tempdir exists");
    let path = temp.path().join("state.db");

    assert_eq!(
        schema::database_health(&path),
        schema::DatabaseHealth::Missing,
        "a missing database is not a finding"
    );
    assert!(!path.exists(), "the health probe must not create the file");

    let store = LocalStateStore::open_at(&path).expect("state store opens");
    drop(store);
    assert_eq!(
        schema::database_health(&path),
        schema::DatabaseHealth::Readable,
        "a bootstrapped database is readable"
    );
}

#[test]
fn an_unreadable_state_database_is_refused_with_the_documented_recovery() {
    let temp = tempdir().expect("tempdir exists");
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).expect("workspace exists");
    let path = temp.path().join("state.db");
    fs::write(&path, "garbage\n").expect("corrupt database writes");

    assert_eq!(
        schema::database_health(&path),
        schema::DatabaseHealth::Unreadable {
            reason: "file is not a database".to_string()
        },
        "a foreign file is classified before any command opens it"
    );

    let error = LocalStateStore::open_at_for(&path, &workspace)
        .expect_err("a foreign file must not be opened as local state");
    assert!(
        matches!(error, AppError::LocalStateDatabaseUnreadable { .. }),
        "unexpected error: {error}"
    );
    let message = error.to_string();
    assert!(
        message.contains("is not a readable SQLite database"),
        "the failure names the corrupt file: {message}"
    );
    assert!(
        message.contains("doctor --repair-state --apply"),
        "the failure names the recovery entry point: {message}"
    );
    assert!(
        message.contains(".agents/skillctl.yaml"),
        "the failure names what local state is rebuilt from: {message}"
    );
    assert_eq!(
        fs::read_to_string(&path).expect("the corrupt file survives"),
        "garbage\n",
        "a refused open must not delete the file"
    );
}

#[test]
fn snapshotting_an_unreadable_state_database_names_the_recovery() {
    let temp = tempdir().expect("tempdir exists");
    let path = temp.path().join("state.db");
    fs::write(&path, "garbage\n").expect("corrupt database writes");

    let error = journal::snapshot_state_database(&path, &temp.path().join("snapshot.db"))
        .expect_err("a foreign file cannot be snapshotted");
    assert!(
        matches!(error, AppError::LocalStateDatabaseUnreadable { .. }),
        "unexpected error: {error}"
    );
    assert!(
        !error.to_string().contains("not an error"),
        "the backup API's meaningless text must not reach the user: {error}"
    );
}

#[test]
fn quarantining_an_unreadable_state_database_rebuilds_it() {
    let temp = tempdir().expect("tempdir exists");
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).expect("workspace exists");
    let database = temp.path().join("state.db");
    fs::write(&database, "garbage\n").expect("corrupt database writes");

    let mut report = StateRecoveryReport {
        state_database_unreadable: Some("file is not a database".to_string()),
        state_database_exists: true,
        ..StateRecoveryReport::default()
    };
    quarantine_unreadable_state_database(&database, &workspace, &mut report)
        .expect("the quarantine repairs the store");

    let quarantined: Vec<_> = fs::read_dir(temp.path())
        .expect("state root lists")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("state.db.corrupt."))
        })
        .collect();
    assert_eq!(quarantined.len(), 1, "exactly one quarantine is kept");
    assert_eq!(
        fs::read_to_string(&quarantined[0]).expect("the corrupt file is kept"),
        "garbage\n",
        "the quarantine preserves the original bytes"
    );
    assert_eq!(
        report.schema_version, CURRENT_LOCAL_STATE_VERSION,
        "the repair records the rebuilt schema version"
    );
    assert!(
        report
            .repairs
            .iter()
            .any(|repair| repair.contains("moved the unreadable state database")),
        "the repair names what it did: {:?}",
        report.repairs
    );
    assert_eq!(
        LocalStateStore::open_at(&database)
            .and_then(|store| store.schema_version())
            .expect("the rebuilt database opens"),
        CURRENT_LOCAL_STATE_VERSION,
        "the rebuilt database carries the current schema"
    );
}

#[test]
fn a_pending_journal_is_left_untouched_while_the_database_is_unreadable() {
    let temp = tempdir().expect("tempdir exists");
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).expect("workspace exists");
    let workspace_key = workspace_key_for_path(&workspace).expect("workspace key resolves");

    let database = temp.path().join("state.db");
    {
        let mut store = LocalStateStore::open_at_for(&database, &workspace).expect("store opens");
        store
            .upsert_install_record(&InstallRecord {
                skill: ManagedSkillRef::new(ManagedScope::Workspace, "release-notes"),
                source_kind: SourceKind::LocalPath,
                source_url: "file:///tmp/shared-skills".to_string(),
                source_subpath: ".agents/skills/release-notes".to_string(),
                resolved_revision: "aaaaaaaaaaaaaaaa".to_string(),
                upstream_revision: None,
                content_hash: "sha256:content".to_string(),
                overlay_hash: "sha256:none".to_string(),
                effective_version_hash: "sha256:effective-a".to_string(),
                installed_at: "2026-11-05T12:00:00Z".to_string(),
                updated_at: "2026-11-05T12:00:00Z".to_string(),
                detached: false,
                forked: false,
            })
            .expect("install writes");
    }

    // The journal is created below the state root, so recovery resolves the
    // database beside it exactly as the default layout does.
    let mut journal = OperationJournal::create(temp.path(), &workspace_key, &workspace, "install")
        .expect("journal creates");
    journal
        .track_state_database(&database)
        .expect("database is staged");
    drop(journal);

    // A corrupt database must not strand the journal in `needs-review`. Its
    // snapshot is the only copy of the committed rows that survives.
    fs::write(&database, "garbage\n").expect("corrupt database writes");

    let outcomes = recover_pending_operations(temp.path()).expect("recovery runs");
    assert_eq!(outcomes.len(), 1);
    match &outcomes[0] {
        RecoveryOutcome::NeedsReview { reason, .. } => {
            assert!(
                reason.contains("not a readable SQLite database"),
                "unexpected reason: {reason}"
            );
            assert!(
                reason.contains("doctor --repair-state --apply"),
                "the reason names the recovery entry point: {reason}"
            );
        }
        other => panic!("expected a needs-review outcome, got {other:?}"),
    }
    let pending = pending_operations(temp.path()).expect("pending journals list");
    assert_eq!(pending.len(), 1, "the journal stays on disk");
    assert_eq!(
        pending[0].state, "active",
        "the journal is not stranded in needs-review"
    );
    assert!(
        pending[0].directory.join("state-snapshot.db").is_file(),
        "the snapshot stays usable"
    );

    // After the quarantine-and-rebuild the repair performs, the same recovery
    // restores the snapshot, and with it the committed rows.
    fs::rename(&database, temp.path().join("state.db.corrupt"))
        .expect("the corrupt file moves aside");
    LocalStateStore::open_at_for(&database, &workspace).expect("a fresh database is rebuilt");
    let outcomes = recover_pending_operations(temp.path()).expect("recovery runs");
    assert!(
        matches!(outcomes.first(), Some(RecoveryOutcome::Recovered(_))),
        "unexpected outcomes: {outcomes:?}"
    );
    assert!(
        pending_operations(temp.path())
            .expect("pending journals list")
            .is_empty(),
        "the recovered journal is closed"
    );
    let store = LocalStateStore::open_at_for(&database, &workspace).expect("store opens");
    assert!(
        store
            .install_record(&ManagedSkillRef::new(
                ManagedScope::Workspace,
                "release-notes"
            ))
            .expect("install record reads")
            .is_some(),
        "the snapshot restored the committed install"
    );
}

fn fitted_check_install(revision: &str) -> InstallRecord {
    InstallRecord {
        skill: ManagedSkillRef::new(ManagedScope::Workspace, "release-notes"),
        source_kind: SourceKind::Git,
        source_url: "file:///tmp/git-source".to_string(),
        source_subpath: ".agents/skills/release-notes".to_string(),
        resolved_revision: revision.to_string(),
        upstream_revision: Some(revision.to_string()),
        content_hash: "sha256:content".to_string(),
        overlay_hash: "sha256:none".to_string(),
        effective_version_hash: "sha256:effective".to_string(),
        installed_at: "2026-11-07T00:00:00Z".to_string(),
        updated_at: "2026-11-07T00:13:00Z".to_string(),
        detached: false,
        forked: false,
    }
}

fn fitted_check_record(
    outcome: UpdateCheckOutcome,
    pinned: &str,
    latest: Option<&str>,
) -> UpdateCheckRecord {
    UpdateCheckRecord {
        id: None,
        skill: ManagedSkillRef::new(ManagedScope::Workspace, "release-notes"),
        checked_at: "2026-11-07T00:12:00Z".to_string(),
        pinned_revision: pinned.to_string(),
        latest_revision: latest.map(str::to_string),
        outcome,
        overlay_detected: false,
        local_modification_detected: false,
        apply_blocked_by: None,
        notes: Some(match outcome {
            UpdateCheckOutcome::UpToDate => "pinned revision already matches upstream".to_string(),
            _ => format!(
                "upstream revision {} is newer than the pinned revision {pinned}",
                latest.unwrap_or_default()
            ),
        }),
    }
}

#[test]
fn a_check_whose_latest_revision_is_installed_reports_up_to_date() {
    let install = fitted_check_install("4e6a597");
    let check = fitted_check_record(
        UpdateCheckOutcome::UpdateAvailable,
        "752fd67",
        Some("4e6a597"),
    );
    let fitted = check.fitted_to(&install);

    assert_eq!(fitted.outcome, UpdateCheckOutcome::UpToDate);
    assert_eq!(fitted.pinned_revision, "4e6a597");
    assert_eq!(fitted.latest_revision.as_deref(), Some("4e6a597"));
    assert!(
        fitted
            .notes
            .as_deref()
            .is_some_and(|notes| notes.contains("the install already holds it")),
        "the note names the fit: {:?}",
        fitted.notes
    );
}

#[test]
fn a_blocked_check_whose_latest_revision_is_installed_reports_up_to_date() {
    let install = fitted_check_install("4e6a597");
    let check = fitted_check_record(UpdateCheckOutcome::Blocked, "752fd67", Some("4e6a597"));
    assert_eq!(
        check.fitted_to(&install).outcome,
        UpdateCheckOutcome::UpToDate
    );
}

#[test]
fn a_check_for_a_revision_the_install_does_not_hold_passes_through() {
    let install = fitted_check_install("752fd67");
    for outcome in [
        UpdateCheckOutcome::UpdateAvailable,
        UpdateCheckOutcome::Blocked,
        UpdateCheckOutcome::UpToDate,
        UpdateCheckOutcome::CommitPinned,
        UpdateCheckOutcome::Detached,
        UpdateCheckOutcome::Failed,
        UpdateCheckOutcome::LocalSource,
    ] {
        let check = fitted_check_record(outcome, "752fd67", Some("4e6a597"));
        let fitted = check.fitted_to(&install);

        assert_eq!(fitted.outcome, outcome, "{outcome:?} passes through");
        assert_eq!(fitted.pinned_revision, "752fd67");
        assert_eq!(fitted.latest_revision.as_deref(), Some("4e6a597"));
    }
}

#[test]
fn a_check_without_an_observed_revision_passes_through() {
    let install = fitted_check_install("752fd67");
    let check = fitted_check_record(UpdateCheckOutcome::UpToDate, "752fd67", None);
    let fitted = check.fitted_to(&install);

    assert_eq!(fitted.outcome, UpdateCheckOutcome::UpToDate);
    assert_eq!(fitted.latest_revision, None);
    assert!(
        fitted.notes.is_some(),
        "a check that fits the install keeps its recorded note"
    );
}

#[test]
fn an_up_to_date_check_for_a_revision_the_install_left_reports_an_update() {
    // A rollback, or a pin onto another revision, leaves the stored check
    // certifying a revision the install no longer holds.
    let install = fitted_check_install("4e6a597");
    let check = fitted_check_record(UpdateCheckOutcome::UpToDate, "752fd67", Some("752fd67"));
    let fitted = check.fitted_to(&install);

    assert_eq!(
        fitted.outcome,
        UpdateCheckOutcome::UpdateAvailable,
        "the recorded revision is a revision the install does not hold"
    );
    assert_eq!(fitted.pinned_revision, "4e6a597");
    assert_eq!(fitted.latest_revision.as_deref(), Some("752fd67"));
    assert!(
        fitted
            .notes
            .as_deref()
            .is_some_and(|notes| notes.contains("the install holds 4e6a597 instead")),
        "the note names the fit: {:?}",
        fitted.notes
    );
}

#[test]
fn an_up_to_date_check_that_still_covers_the_installed_revision_passes_through() {
    let install = fitted_check_install("752fd67");
    let check = fitted_check_record(UpdateCheckOutcome::UpToDate, "752fd67", Some("752fd67"));
    let fitted = check.fitted_to(&install);

    assert_eq!(fitted.outcome, UpdateCheckOutcome::UpToDate);
    assert_eq!(fitted.pinned_revision, "752fd67");
    assert_eq!(
        fitted.notes.as_deref(),
        Some("pinned revision already matches upstream"),
        "a check that still describes the install keeps its recorded note"
    );
}
