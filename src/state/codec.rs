use super::*;

type InstallRow = (
    String,
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
    String,
);

type UpdateCheckRow = (
    i64,
    String,
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
    String,
    Option<String>,
    Option<String>,
);

type PinRow = (
    String,
    String,
    String,
    String,
    String,
    Option<String>,
    String,
);

type RollbackRow = (i64, String, String, String, String, String, String);

type TelemetrySettingsRow = (String, Option<String>, String);

type HistoryEntryRow = (
    i64,
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    String,
    String,
    Option<String>,
);

pub(super) fn decode_install_record(
    path: &Path,
    row: InstallRow,
) -> Result<InstallRecord, AppError> {
    let scope = parse_scope(path, "install_records.scope", &row.1)?;
    validate_workspace_key(path, "install_records.workspace_key", scope, &row.0)?;
    Ok(InstallRecord {
        skill: ManagedSkillRef::new(scope, row.2),
        source_kind: parse_source_kind(path, "install_records.source_kind", &row.3)?,
        source_url: row.4,
        source_subpath: row.5,
        resolved_revision: row.6,
        upstream_revision: row.7,
        content_hash: row.8,
        overlay_hash: row.9,
        effective_version_hash: row.10,
        installed_at: row.11,
        updated_at: row.12,
        detached: int_to_bool(path, "install_records.detached", row.13)?,
        forked: int_to_bool(path, "install_records.forked", row.14)?,
    })
}

pub(super) fn decode_projection_record(
    path: &Path,
    row: ProjectionRow,
) -> Result<ProjectionRecord, AppError> {
    let scope = parse_scope(path, "projection_records.scope", &row.1)?;
    validate_workspace_key(path, "projection_records.workspace_key", scope, &row.0)?;
    Ok(ProjectionRecord {
        skill: ManagedSkillRef::new(scope, row.2),
        target: parse_target_runtime(path, "projection_records.target", &row.3)?,
        generation_mode: parse_projection_mode(path, "projection_records.generation_mode", &row.4)?,
        physical_root: row.5,
        projected_path: row.6,
        effective_version_hash: row.7,
        generated_at: row.8,
    })
}

pub(super) fn decode_update_check_record(
    path: &Path,
    row: UpdateCheckRow,
) -> Result<UpdateCheckRecord, AppError> {
    let scope = parse_scope(path, "update_checks.scope", &row.2)?;
    validate_workspace_key(path, "update_checks.workspace_key", scope, &row.1)?;
    Ok(UpdateCheckRecord {
        id: Some(row.0),
        skill: ManagedSkillRef::new(scope, row.3),
        checked_at: row.4,
        pinned_revision: row.5,
        latest_revision: row.6,
        outcome: parse_update_check_outcome(path, "update_checks.outcome", &row.7)?,
        overlay_detected: int_to_bool(path, "update_checks.overlay_detected", row.8)?,
        local_modification_detected: int_to_bool(
            path,
            "update_checks.local_modification_detected",
            row.9,
        )?,
        notes: row.10,
    })
}

pub(super) fn decode_local_modification_record(
    path: &Path,
    row: LocalModificationRow,
) -> Result<LocalModificationRecord, AppError> {
    let scope = parse_scope(path, "local_modifications.scope", &row.2)?;
    validate_workspace_key(path, "local_modifications.workspace_key", scope, &row.1)?;
    Ok(LocalModificationRecord {
        id: Some(row.0),
        skill: ManagedSkillRef::new(scope, row.3),
        detected_at: row.4,
        kind: parse_local_modification_kind(path, "local_modifications.kind", &row.5)?,
        path: row.6,
        details: row.7,
    })
}

pub(super) fn decode_pin_record(path: &Path, row: PinRow) -> Result<PinRecord, AppError> {
    let scope = parse_scope(path, "pins.scope", &row.1)?;
    validate_workspace_key(path, "pins.workspace_key", scope, &row.0)?;
    Ok(PinRecord {
        skill: ManagedSkillRef::new(scope, row.2),
        requested_reference: row.3,
        resolved_revision: row.4,
        effective_version_hash: row.5,
        pinned_at: row.6,
    })
}

pub(super) fn decode_rollback_record(
    path: &Path,
    row: RollbackRow,
) -> Result<RollbackRecord, AppError> {
    let scope = parse_scope(path, "rollback_records.scope", &row.2)?;
    validate_workspace_key(path, "rollback_records.workspace_key", scope, &row.1)?;
    Ok(RollbackRecord {
        id: Some(row.0),
        skill: ManagedSkillRef::new(scope, row.3),
        rolled_back_at: row.4,
        from_reference: row.5,
        to_reference: row.6,
    })
}

pub(super) fn decode_telemetry_settings(
    path: &Path,
    row: TelemetrySettingsRow,
) -> Result<TelemetrySettings, AppError> {
    Ok(TelemetrySettings {
        consent: parse_telemetry_consent(path, "telemetry_settings.consent", &row.0)?,
        notice_seen_at: row.1,
        updated_at: row.2,
    })
}

pub(super) fn decode_history_entry(
    path: &Path,
    row: HistoryEntryRow,
) -> Result<HistoryEntry, AppError> {
    let scope = match row.3 {
        Some(scope) => Some(parse_scope(path, "history_events.scope", &scope)?),
        None => None,
    };
    if let Some(scope) = scope {
        validate_workspace_key(path, "history_events.workspace_key", scope, &row.1)?;
    } else if row.1 != GLOBAL_WORKSPACE_KEY {
        return Err(local_state_validation(
            path,
            format!(
                "history_events.workspace_key must be '{}' for global events, found '{}'",
                GLOBAL_WORKSPACE_KEY, row.1
            ),
        ));
    }
    let target = match row.5 {
        Some(target) => Some(parse_target_runtime(
            path,
            "history_events.target",
            &target,
        )?),
        None => None,
    };

    Ok(HistoryEntry {
        id: Some(row.0),
        kind: HistoryEventKind::parse(&row.2).ok_or_else(|| {
            local_state_validation(
                path,
                format!("history_events.kind has unsupported value '{}'", row.2),
            )
        })?,
        scope,
        skill_id: row.4,
        target,
        occurred_at: row.6,
        summary: row.7,
        details: decode_history_details(path, row.8)?,
    })
}

fn validate_workspace_key(
    path: &Path,
    field: &str,
    scope: ManagedScope,
    value: &str,
) -> Result<(), AppError> {
    if value.trim().is_empty() {
        return Err(local_state_validation(
            path,
            format!("{field} must not be empty"),
        ));
    }

    if scope == ManagedScope::User && value != GLOBAL_WORKSPACE_KEY {
        return Err(local_state_validation(
            path,
            format!(
                "{field} must be '{}' for user scope rows, found '{}'",
                GLOBAL_WORKSPACE_KEY, value
            ),
        ));
    }

    Ok(())
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

pub(super) fn source_kind_as_str(kind: SourceKind) -> &'static str {
    match kind {
        SourceKind::Git => "git",
        SourceKind::LocalPath => "local-path",
        SourceKind::Archive => "archive",
    }
}

pub(super) fn encode_history_details(
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

pub(super) fn bool_to_int(value: bool) -> i64 {
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

pub(super) fn validate_install_record(path: &Path, record: &InstallRecord) -> Result<(), AppError> {
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

pub(super) fn validate_projection_record(
    path: &Path,
    record: &ProjectionRecord,
) -> Result<(), AppError> {
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

pub(super) fn validate_update_check_record(
    path: &Path,
    record: &UpdateCheckRecord,
) -> Result<(), AppError> {
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

pub(super) fn validate_local_modification_record(
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

pub(super) fn validate_pin_record(path: &Path, record: &PinRecord) -> Result<(), AppError> {
    validate_skill_ref(path, &record.skill)?;
    validate_non_empty_trimmed(path, "pin.requested_reference", &record.requested_reference)?;
    validate_token(path, "pin.resolved_revision", &record.resolved_revision)?;
    if let Some(hash) = &record.effective_version_hash {
        validate_token(path, "pin.effective_version_hash", hash)?;
    }
    validate_timestamp(path, "pin.pinned_at", &record.pinned_at)?;
    Ok(())
}

pub(super) fn validate_rollback_record(
    path: &Path,
    record: &RollbackRecord,
) -> Result<(), AppError> {
    validate_skill_ref(path, &record.skill)?;
    validate_timestamp(path, "rollback.rolled_back_at", &record.rolled_back_at)?;
    validate_non_empty_trimmed(path, "rollback.from_reference", &record.from_reference)?;
    validate_non_empty_trimmed(path, "rollback.to_reference", &record.to_reference)?;
    Ok(())
}

pub(super) fn validate_telemetry_settings(
    path: &Path,
    settings: &TelemetrySettings,
) -> Result<(), AppError> {
    if let Some(notice_seen_at) = &settings.notice_seen_at {
        validate_timestamp(path, "telemetry.notice_seen_at", notice_seen_at)?;
    }
    validate_timestamp(path, "telemetry.updated_at", &settings.updated_at)?;
    Ok(())
}

pub(super) fn validate_history_entry(path: &Path, entry: &HistoryEntry) -> Result<(), AppError> {
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

pub(super) fn validate_skill_ref(path: &Path, skill: &ManagedSkillRef) -> Result<(), AppError> {
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
