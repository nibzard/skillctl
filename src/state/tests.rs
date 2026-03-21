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
        error
            .to_string()
            .contains("schema version supports 1 through 2, found 999"),
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
