//! Telemetry domain entry points.
//!
//! Telemetry is a local classification layer today. Install and update events
//! are classified on this machine, and nothing is transmitted because no
//! emission backend exists.

use std::{fs, io, path::Path};

use serde::Serialize;
use url::Url;

use crate::{
    app::AppContext,
    cli::TelemetryCommand,
    error::AppError,
    history::HistoryLedger,
    manifest::{
        DEFAULT_MANIFEST_PATH, TelemetryConfig as ManifestTelemetryConfig,
        TelemetryMode as ManifestTelemetryMode, WorkspaceManifest,
    },
    planner::SkillUpdatePlan,
    response::AppResponse,
    source::{InstalledSkill, NormalizedInstallSource, SourceKind, current_timestamp},
    state::{LocalStateStore, TelemetryConsent, TelemetrySettings as PersistedTelemetrySettings},
};

const FIRST_RUN_NOTICE: &str = concat!(
    "Telemetry is enabled by default. ",
    "Install and update events are classified locally, and nothing is ",
    "transmitted today because no emission backend exists. ",
    "Private repository identifiers and skill contents are never included. ",
    "Run `skillctl telemetry disable` to opt out."
);
const LOCAL_CLASSIFICATION_NOTE: &str = concat!(
    "Install and update events are classified locally, and nothing is ",
    "transmitted today because no emission backend exists."
);
/// Where events are classified. Classification never leaves the machine.
const EVENT_CLASSIFICATION_LOCAL: &str = "local";
/// The emission backend that would transmit classified events. None exists.
const EMISSION_BACKEND_NONE: &str = "none";
const VERIFIED_PUBLIC_GIT_REPOSITORIES: &[(&str, &str)] = &[
    ("github.com", "nibzard/skillctl"),
    ("github.com", "vercel/ai"),
];

/// Supported telemetry collection modes.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum TelemetryMode {
    /// Public-source telemetry only.
    PublicOnly,
    /// Telemetry disabled.
    Off,
}

/// Effective telemetry settings after workspace policy and local consent are applied.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct TelemetrySettings {
    /// Whether telemetry is enabled.
    pub enabled: bool,
    /// Effective telemetry mode.
    pub mode: TelemetryMode,
}

/// Public-only telemetry visibility classification for one install source.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SourceVisibility {
    /// The source is public enough for content-free aggregate telemetry.
    Public,
    /// The source is local-only and must never be emitted remotely.
    SuppressedLocal,
    /// The source is remote but likely private and must never be emitted remotely.
    SuppressedPrivate,
}

/// Classify one normalized install source for public-only telemetry eligibility.
///
/// Classification runs locally. Its result only marks which events a future
/// emission backend could send; it never transmits anything by itself.
pub fn classify_source_visibility(source: &NormalizedInstallSource) -> SourceVisibility {
    match source.kind {
        SourceKind::LocalPath | SourceKind::Archive => SourceVisibility::SuppressedLocal,
        SourceKind::Git => classify_git_source_visibility(source),
        SourceKind::WellKnown => classify_well_known_source_visibility(source),
    }
}

/// Classify one discovery origin for public-only telemetry eligibility.
///
/// The origin-trust policy in [`crate::trust`] names which publishers are
/// public, the same way the verified Git repository list does, so a well-known
/// origin is never guessed at: a loopback index is local, a verified publisher
/// is public, and every other remote origin stays suppressed.
fn classify_well_known_source_visibility(source: &NormalizedInstallSource) -> SourceVisibility {
    match crate::trust::discovery_origin_trust(&source.url) {
        // A loopback origin serves a local index, which is local-only content.
        crate::trust::DiscoveryOriginTrust::Loopback => SourceVisibility::SuppressedLocal,
        crate::trust::DiscoveryOriginTrust::VerifiedPublicHttps => SourceVisibility::Public,
        // An unverified or refused remote origin may publish privately, so its
        // events stay suppressed until the policy verifies the publisher.
        crate::trust::DiscoveryOriginTrust::UnverifiedRemote
        | crate::trust::DiscoveryOriginTrust::InsecureTransport => {
            SourceVisibility::SuppressedPrivate
        }
    }
}

/// Return whether this source is eligible for remote emission under the given settings.
///
/// Eligibility is a local classification only. No emission backend exists, so
/// an eligible event is still not transmitted anywhere.
pub fn allows_remote_emission(
    settings: TelemetrySettings,
    source: &NormalizedInstallSource,
) -> bool {
    settings.enabled
        && matches!(settings.mode, TelemetryMode::PublicOnly)
        && matches!(classify_source_visibility(source), SourceVisibility::Public)
}

/// Handle the `skillctl telemetry` command family.
pub fn handle_command(
    context: &AppContext,
    command: &TelemetryCommand,
) -> Result<AppResponse, AppError> {
    let workspace = load_workspace_telemetry_config(&context.working_directory)?;
    match command {
        TelemetryCommand::Status => {
            let store = LocalStateStore::open_default_for(&context.working_directory)?;
            let persisted = store.telemetry_settings()?;
            let report = build_report(&workspace, persisted.as_ref(), None, Vec::new(), None);

            Ok(AppResponse::success("telemetry-status")
                .with_summary(status_summary(&report))
                .with_data(serde_json::to_value(report)?))
        }
        TelemetryCommand::Enable => update_consent(
            context,
            "telemetry-enable",
            &workspace,
            TelemetryConsent::Enabled,
        ),
        TelemetryCommand::Disable => update_consent(
            context,
            "telemetry-disable",
            &workspace,
            TelemetryConsent::Disabled,
        ),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
enum TelemetrySuppressionReason {
    DisabledByConsent,
    DisabledByWorkspace,
    LocalSource,
    PrivateSource,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct TelemetryReport {
    #[serde(flatten)]
    status: TelemetryStatusView,
    #[serde(skip_serializing_if = "Option::is_none")]
    notice: Option<TelemetryNotice>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    events: Vec<RemoteTelemetryEvent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    changed: Option<bool>,
}

impl TelemetryReport {
    pub(crate) fn notice_message(&self) -> Option<&str> {
        self.notice.as_ref().map(|notice| notice.message.as_str())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct TelemetryStatusView {
    consent: String,
    notice_seen: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    notice_seen_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    updated_at: Option<String>,
    workspace_enabled: bool,
    workspace_mode: TelemetryMode,
    effective_enabled: bool,
    effective_mode: TelemetryMode,
    /// Where events are classified. Always `local`.
    classification: &'static str,
    /// The backend that would transmit classified events. Always `none` today.
    emission_backend: &'static str,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct TelemetryNotice {
    shown: bool,
    message: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct RemoteTelemetryEvent {
    kind: &'static str,
    skill: String,
    /// Whether the event is eligible for remote emission once a backend exists.
    /// No backend exists today, so no event is transmitted.
    emitted: bool,
    source_visibility: SourceVisibility,
    #[serde(skip_serializing_if = "Option::is_none")]
    suppression_reason: Option<TelemetrySuppressionReason>,
    #[serde(skip_serializing_if = "Option::is_none")]
    public_source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    resolved_revision: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pinned_revision: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    latest_revision: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    outcome: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    overlay_detected: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    local_modification_detected: Option<bool>,
}

/// Build the local telemetry report for one install run.
///
/// The report classifies events locally; it never transmits them because no
/// emission backend exists.
pub(crate) fn prepare_install_report(
    context: &AppContext,
    source: &NormalizedInstallSource,
    installed: &[InstalledSkill],
) -> Result<TelemetryReport, AppError> {
    let workspace = load_workspace_telemetry_config(&context.working_directory)?;
    let mut store = LocalStateStore::open_default_for(&context.working_directory)?;
    let (persisted, notice) = ensure_first_run_notice(&mut store)?;
    let effective = effective_settings(&workspace, Some(&persisted));
    let visibility = classify_source_visibility(source);
    let events = installed
        .iter()
        .map(|skill| install_event(&workspace, &persisted, effective, visibility, source, skill))
        .collect();

    Ok(build_report(
        &workspace,
        Some(&persisted),
        notice,
        events,
        None,
    ))
}

/// Build the local telemetry report for one update check run.
///
/// The report classifies events locally; it never transmits them because no
/// emission backend exists.
pub(crate) fn prepare_update_report(
    context: &AppContext,
    plans: &[SkillUpdatePlan],
) -> Result<TelemetryReport, AppError> {
    let workspace = load_workspace_telemetry_config(&context.working_directory)?;
    let mut store = LocalStateStore::open_default_for(&context.working_directory)?;
    let (persisted, notice) = ensure_first_run_notice(&mut store)?;
    let effective = effective_settings(&workspace, Some(&persisted));
    let events = plans
        .iter()
        .map(|plan| update_event(&workspace, &persisted, effective, plan))
        .collect();

    Ok(build_report(
        &workspace,
        Some(&persisted),
        notice,
        events,
        None,
    ))
}

fn install_event(
    workspace: &ManifestTelemetryConfig,
    persisted: &PersistedTelemetrySettings,
    effective: TelemetrySettings,
    visibility: SourceVisibility,
    source: &NormalizedInstallSource,
    skill: &InstalledSkill,
) -> RemoteTelemetryEvent {
    let suppression_reason = suppression_reason(workspace, persisted, effective, visibility);

    RemoteTelemetryEvent {
        kind: "install",
        skill: skill.name.clone(),
        emitted: suppression_reason.is_none(),
        source_visibility: visibility,
        suppression_reason,
        public_source: matches!(visibility, SourceVisibility::Public)
            .then(|| source.url.clone())
            .filter(|_| suppression_reason.is_none()),
        resolved_revision: Some(skill.resolved_revision.clone()),
        pinned_revision: None,
        latest_revision: None,
        outcome: Some("installed".to_string()),
        overlay_detected: None,
        local_modification_detected: None,
    }
}

fn update_event(
    workspace: &ManifestTelemetryConfig,
    persisted: &PersistedTelemetrySettings,
    effective: TelemetrySettings,
    plan: &SkillUpdatePlan,
) -> RemoteTelemetryEvent {
    let source = NormalizedInstallSource {
        raw: plan.source.url.clone(),
        kind: plan.source.kind,
        url: plan.source.url.clone(),
        display: plan.source.url.clone(),
    };
    let visibility = classify_source_visibility(&source);
    let suppression_reason = suppression_reason(workspace, persisted, effective, visibility);

    RemoteTelemetryEvent {
        kind: "update",
        skill: plan.skill.clone(),
        emitted: suppression_reason.is_none(),
        source_visibility: visibility,
        suppression_reason,
        public_source: matches!(visibility, SourceVisibility::Public)
            .then(|| plan.source.url.clone())
            .filter(|_| suppression_reason.is_none()),
        resolved_revision: None,
        pinned_revision: Some(plan.pinned_revision.clone()),
        latest_revision: plan.latest_revision.clone(),
        outcome: Some(plan.outcome.as_str().to_string()),
        overlay_detected: Some(plan.overlay_detected),
        local_modification_detected: Some(plan.local_modification_detected),
    }
}

fn suppression_reason(
    workspace: &ManifestTelemetryConfig,
    _persisted: &PersistedTelemetrySettings,
    effective: TelemetrySettings,
    visibility: SourceVisibility,
) -> Option<TelemetrySuppressionReason> {
    if !effective.enabled {
        return Some(
            if !workspace.enabled || matches!(workspace.mode, ManifestTelemetryMode::Off) {
                TelemetrySuppressionReason::DisabledByWorkspace
            } else {
                TelemetrySuppressionReason::DisabledByConsent
            },
        );
    }

    match visibility {
        SourceVisibility::Public => None,
        SourceVisibility::SuppressedLocal => Some(TelemetrySuppressionReason::LocalSource),
        SourceVisibility::SuppressedPrivate => Some(TelemetrySuppressionReason::PrivateSource),
    }
}

fn update_consent(
    context: &AppContext,
    command: &'static str,
    workspace: &ManifestTelemetryConfig,
    consent: TelemetryConsent,
) -> Result<AppResponse, AppError> {
    let mut store = LocalStateStore::open_default_for(&context.working_directory)?;
    let current = store.telemetry_settings()?;
    let timestamp = current_timestamp();
    let notice_seen_at = current
        .as_ref()
        .and_then(|settings| settings.notice_seen_at.clone())
        .unwrap_or_else(|| timestamp.clone());
    let next = PersistedTelemetrySettings {
        consent,
        notice_seen_at: Some(notice_seen_at),
        updated_at: timestamp,
    };
    let changed = current.as_ref() != Some(&next);

    if changed {
        let mut ledger = HistoryLedger::new(&mut store);
        ledger.record_telemetry_change(&next)?;
    }

    let persisted = current.unwrap_or_else(|| next.clone());
    let report = build_report(
        workspace,
        Some(if changed { &next } else { &persisted }),
        None,
        Vec::new(),
        Some(changed),
    );

    Ok(AppResponse::success(command)
        .with_summary(consent_summary(command, &report, changed))
        .with_data(serde_json::to_value(report)?))
}

fn build_report(
    workspace: &ManifestTelemetryConfig,
    persisted: Option<&PersistedTelemetrySettings>,
    notice: Option<TelemetryNotice>,
    events: Vec<RemoteTelemetryEvent>,
    changed: Option<bool>,
) -> TelemetryReport {
    TelemetryReport {
        status: status_view(workspace, persisted),
        notice,
        events,
        changed,
    }
}

fn status_view(
    workspace: &ManifestTelemetryConfig,
    persisted: Option<&PersistedTelemetrySettings>,
) -> TelemetryStatusView {
    let effective = effective_settings(workspace, persisted);

    TelemetryStatusView {
        consent: persisted.map_or_else(
            || TelemetryConsent::Unknown.as_str().to_string(),
            |settings| settings.consent.as_str().to_string(),
        ),
        notice_seen: persisted
            .and_then(|settings| settings.notice_seen_at.as_ref())
            .is_some(),
        notice_seen_at: persisted.and_then(|settings| settings.notice_seen_at.clone()),
        updated_at: persisted.map(|settings| settings.updated_at.clone()),
        workspace_enabled: workspace.enabled,
        workspace_mode: telemetry_mode_from_manifest(workspace.mode),
        effective_enabled: effective.enabled,
        effective_mode: effective.mode,
        classification: EVENT_CLASSIFICATION_LOCAL,
        emission_backend: EMISSION_BACKEND_NONE,
    }
}

fn status_summary(report: &TelemetryReport) -> String {
    let state = if !report.status.notice_seen {
        "Telemetry consent defaults to enabled after the first-run notice."
    } else if report.status.effective_enabled {
        "Telemetry is enabled."
    } else if !report.status.workspace_enabled
        || matches!(report.status.workspace_mode, TelemetryMode::Off)
    {
        "Telemetry is disabled by the current workspace configuration."
    } else {
        "Telemetry is disabled by local consent."
    };

    format!("{state} {LOCAL_CLASSIFICATION_NOTE}")
}

fn consent_summary(command: &str, report: &TelemetryReport, changed: bool) -> String {
    match command {
        "telemetry-enable" if !changed => {
            format!("Telemetry consent is already enabled. {LOCAL_CLASSIFICATION_NOTE}")
        }
        "telemetry-enable" if report.status.effective_enabled => {
            format!("Telemetry consent enabled. {LOCAL_CLASSIFICATION_NOTE}")
        }
        "telemetry-enable" => {
            "Telemetry consent enabled, but the workspace configuration still disables telemetry."
                .to_string()
        }
        "telemetry-disable" if !changed => "Telemetry consent is already disabled.".to_string(),
        "telemetry-disable" => {
            "Telemetry consent disabled. Local history remains available.".to_string()
        }
        _ => status_summary(report),
    }
}

fn effective_settings(
    workspace: &ManifestTelemetryConfig,
    persisted: Option<&PersistedTelemetrySettings>,
) -> TelemetrySettings {
    let mode = telemetry_mode_from_manifest(workspace.mode);
    let consent_enabled =
        persisted.is_some_and(|settings| settings.consent == TelemetryConsent::Enabled);

    TelemetrySettings {
        enabled: workspace.enabled && matches!(mode, TelemetryMode::PublicOnly) && consent_enabled,
        mode,
    }
}

fn telemetry_mode_from_manifest(mode: ManifestTelemetryMode) -> TelemetryMode {
    match mode {
        ManifestTelemetryMode::PublicOnly => TelemetryMode::PublicOnly,
        ManifestTelemetryMode::Off => TelemetryMode::Off,
    }
}

fn ensure_first_run_notice(
    store: &mut LocalStateStore,
) -> Result<(PersistedTelemetrySettings, Option<TelemetryNotice>), AppError> {
    let Some(current) = store.telemetry_settings()? else {
        let timestamp = current_timestamp();
        let settings = PersistedTelemetrySettings {
            consent: TelemetryConsent::Enabled,
            notice_seen_at: Some(timestamp.clone()),
            updated_at: timestamp,
        };
        store.upsert_telemetry_settings(&settings)?;
        return Ok((settings, Some(first_run_notice())));
    };

    if current.notice_seen_at.is_some() {
        return Ok((current, None));
    }

    let timestamp = current_timestamp();
    let settings = PersistedTelemetrySettings {
        consent: if current.consent == TelemetryConsent::Unknown {
            TelemetryConsent::Enabled
        } else {
            current.consent
        },
        notice_seen_at: Some(timestamp.clone()),
        updated_at: timestamp,
    };
    store.upsert_telemetry_settings(&settings)?;

    Ok((settings, Some(first_run_notice())))
}

fn first_run_notice() -> TelemetryNotice {
    TelemetryNotice {
        shown: true,
        message: FIRST_RUN_NOTICE.to_string(),
    }
}

fn load_workspace_telemetry_config(
    working_directory: &Path,
) -> Result<ManifestTelemetryConfig, AppError> {
    let manifest_path = working_directory.join(DEFAULT_MANIFEST_PATH);
    match fs::metadata(&manifest_path) {
        Ok(_) => Ok(WorkspaceManifest::load_from_workspace(working_directory)?.telemetry),
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            Ok(WorkspaceManifest::default_at(manifest_path).telemetry)
        }
        Err(source) => Err(AppError::FilesystemOperation {
            action: "inspect manifest",
            path: manifest_path,
            source,
        }),
    }
}

fn classify_git_source_visibility(source: &NormalizedInstallSource) -> SourceVisibility {
    if source.url.starts_with("git@") || source.raw.starts_with("git@") {
        return SourceVisibility::SuppressedPrivate;
    }

    let Ok(parsed) = Url::parse(source.url.as_str()) else {
        return SourceVisibility::SuppressedPrivate;
    };

    match parsed.scheme() {
        "file" => SourceVisibility::SuppressedLocal,
        "http" | "https" | "git" => {
            if !parsed.username().is_empty() {
                return SourceVisibility::SuppressedPrivate;
            }

            match parsed.host_str() {
                Some(host) if is_local_host(host) => SourceVisibility::SuppressedLocal,
                Some(host) if is_verified_public_git_repository(host, parsed.path()) => {
                    SourceVisibility::Public
                }
                // Remote Git URLs can represent private repositories even on public forges.
                // Keep them suppressed until visibility is proven by an explicit allowlist.
                Some(_) | None => SourceVisibility::SuppressedPrivate,
            }
        }
        _ => SourceVisibility::SuppressedPrivate,
    }
}

fn is_local_host(host: &str) -> bool {
    crate::trust::is_loopback_host(host)
}

fn is_verified_public_git_repository(host: &str, path: &str) -> bool {
    let normalized_host = host.to_ascii_lowercase();
    let normalized_path = path
        .trim_matches('/')
        .trim_end_matches(".git")
        .to_ascii_lowercase();

    VERIFIED_PUBLIC_GIT_REPOSITORIES
        .iter()
        .any(|(verified_host, verified_path)| {
            normalized_host == *verified_host && normalized_path == *verified_path
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        planner::{SkillUpdatePlan, UpdateAction, UpdateSourceSummary},
        state::ManagedScope,
    };

    fn source(kind: SourceKind, raw: &str, url: &str) -> NormalizedInstallSource {
        NormalizedInstallSource {
            raw: raw.to_string(),
            kind,
            url: url.to_string(),
            display: raw.to_string(),
        }
    }

    #[test]
    fn remote_git_sources_stay_suppressed_without_explicit_public_proof() {
        let settings = TelemetrySettings {
            enabled: true,
            mode: TelemetryMode::PublicOnly,
        };
        let https = source(
            SourceKind::Git,
            "https://github.com/example/private-skill.git",
            "https://github.com/example/private-skill.git",
        );
        let git = source(
            SourceKind::Git,
            "git://example.com/private-skill.git",
            "git://example.com/private-skill.git",
        );

        assert_eq!(
            classify_source_visibility(&https),
            SourceVisibility::SuppressedPrivate
        );
        assert_eq!(
            classify_source_visibility(&git),
            SourceVisibility::SuppressedPrivate
        );
        assert!(!allows_remote_emission(settings, &https));
        assert!(!allows_remote_emission(settings, &git));
    }

    #[test]
    fn verified_public_git_sources_enable_remote_emission() {
        let settings = TelemetrySettings {
            enabled: true,
            mode: TelemetryMode::PublicOnly,
        };
        let public = source(
            SourceKind::Git,
            "https://github.com/vercel/ai.git",
            "https://github.com/vercel/ai.git",
        );

        assert_eq!(
            classify_source_visibility(&public),
            SourceVisibility::Public
        );
        assert!(allows_remote_emission(settings, &public));
    }

    #[test]
    fn local_and_private_sources_are_suppressed_from_remote_emission() {
        let settings = TelemetrySettings {
            enabled: true,
            mode: TelemetryMode::PublicOnly,
        };
        let local_path = source(
            SourceKind::LocalPath,
            "./private-source",
            "file:///tmp/private-source",
        );
        let archive = source(
            SourceKind::Archive,
            "./private-source.tar.gz",
            "file:///tmp/private-source.tar.gz",
        );
        let file_git = source(
            SourceKind::Git,
            "file:///tmp/private-repo",
            "file:///tmp/private-repo",
        );
        let scp = source(
            SourceKind::Git,
            "git@github.com:example/private-skill.git",
            "git@github.com:example/private-skill.git",
        );
        let ssh = source(
            SourceKind::Git,
            "ssh://git@example.com/private-skill.git",
            "ssh://git@example.com/private-skill.git",
        );
        let localhost = source(
            SourceKind::Git,
            "https://localhost/private-skill.git",
            "https://localhost/private-skill.git",
        );
        let credentialed = source(
            SourceKind::Git,
            "https://token@example.com/private-skill.git",
            "https://token@example.com/private-skill.git",
        );

        assert_eq!(
            classify_source_visibility(&local_path),
            SourceVisibility::SuppressedLocal
        );
        assert_eq!(
            classify_source_visibility(&archive),
            SourceVisibility::SuppressedLocal
        );
        assert_eq!(
            classify_source_visibility(&file_git),
            SourceVisibility::SuppressedLocal
        );
        assert_eq!(
            classify_source_visibility(&scp),
            SourceVisibility::SuppressedPrivate
        );
        assert_eq!(
            classify_source_visibility(&ssh),
            SourceVisibility::SuppressedPrivate
        );
        assert_eq!(
            classify_source_visibility(&localhost),
            SourceVisibility::SuppressedLocal
        );
        assert_eq!(
            classify_source_visibility(&credentialed),
            SourceVisibility::SuppressedPrivate
        );
        assert!(!allows_remote_emission(settings, &local_path));
        assert!(!allows_remote_emission(settings, &archive));
        assert!(!allows_remote_emission(settings, &file_git));
        assert!(!allows_remote_emission(settings, &scp));
        assert!(!allows_remote_emission(settings, &ssh));
        assert!(!allows_remote_emission(settings, &localhost));
        assert!(!allows_remote_emission(settings, &credentialed));
    }

    #[test]
    fn well_known_sources_classify_through_the_origin_trust_policy() {
        let settings = TelemetrySettings {
            enabled: true,
            mode: TelemetryMode::PublicOnly,
        };
        let loopback = source(
            SourceKind::WellKnown,
            "http://127.0.0.1:8799",
            "http://127.0.0.1:8799/.well-known/agent-skills/index.json",
        );
        let verified = source(
            SourceKind::WellKnown,
            "https://www.russ.cloud",
            "https://www.russ.cloud/.well-known/agent-skills/index.json",
        );
        let unverified = source(
            SourceKind::WellKnown,
            "https://skills.internal.example",
            "https://skills.internal.example/.well-known/agent-skills/index.json",
        );
        let insecure = source(
            SourceKind::WellKnown,
            "http://skills.example.com",
            "http://skills.example.com/.well-known/agent-skills/index.json",
        );

        // A loopback index is local-only content, not a private remote one.
        assert_eq!(
            classify_source_visibility(&loopback),
            SourceVisibility::SuppressedLocal
        );
        // Only a publisher the policy verified is public enough to emit.
        assert_eq!(
            classify_source_visibility(&verified),
            SourceVisibility::Public
        );
        assert!(allows_remote_emission(settings, &verified));
        // Every other remote origin may publish privately, so it stays
        // suppressed instead of being guessed at.
        assert_eq!(
            classify_source_visibility(&unverified),
            SourceVisibility::SuppressedPrivate
        );
        assert_eq!(
            classify_source_visibility(&insecure),
            SourceVisibility::SuppressedPrivate
        );
        assert!(!allows_remote_emission(settings, &unverified));
        assert!(!allows_remote_emission(settings, &insecure));
    }

    #[test]
    fn disabled_or_off_settings_suppress_even_public_sources() {
        let public_source = source(
            SourceKind::Git,
            "https://github.com/example/release-notes.git",
            "https://github.com/example/release-notes.git",
        );

        assert!(!allows_remote_emission(
            TelemetrySettings {
                enabled: false,
                mode: TelemetryMode::PublicOnly,
            },
            &public_source
        ));
        assert!(!allows_remote_emission(
            TelemetrySettings {
                enabled: true,
                mode: TelemetryMode::Off,
            },
            &public_source
        ));
    }

    #[test]
    fn install_events_emit_only_when_visibility_is_explicitly_public() {
        let workspace = ManifestTelemetryConfig {
            enabled: true,
            mode: ManifestTelemetryMode::PublicOnly,
        };
        let persisted = PersistedTelemetrySettings {
            consent: TelemetryConsent::Enabled,
            notice_seen_at: Some("2026-03-19T12:00:00Z".to_string()),
            updated_at: "2026-03-19T12:00:00Z".to_string(),
        };
        let effective = effective_settings(&workspace, Some(&persisted));
        let public_source = source(
            SourceKind::Git,
            "https://github.com/example/release-notes.git",
            "https://github.com/example/release-notes.git",
        );
        let installed = InstalledSkill {
            id: "release-notes".to_string(),
            name: "release-notes".to_string(),
            scope: "workspace".to_string(),
            source_path: ".agents/skills/release-notes".to_string(),
            selected_subpath: ".agents/skills/release-notes".to_string(),
            stored_source_root: "/tmp/release-notes".to_string(),
            resolved_revision: "0123456789abcdef".to_string(),
            upstream_revision: Some("0123456789abcdef".to_string()),
            content_hash: "sha256:content".to_string(),
            overlay_hash: "sha256:none".to_string(),
            effective_version_hash: "sha256:effective".to_string(),
            safety: crate::skill::SkillSafetySummary::default(),
            trust: crate::trust::SkillTrust::local(false),
        };

        let install_event = install_event(
            &workspace,
            &persisted,
            effective,
            SourceVisibility::Public,
            &public_source,
            &installed,
        );

        assert!(install_event.emitted);
        assert_eq!(
            install_event.public_source.as_deref(),
            Some("https://github.com/example/release-notes.git")
        );
    }

    #[test]
    fn private_https_update_events_are_suppressed_even_with_enabled_consent() {
        let workspace = ManifestTelemetryConfig {
            enabled: true,
            mode: ManifestTelemetryMode::PublicOnly,
        };
        let persisted = PersistedTelemetrySettings {
            consent: TelemetryConsent::Enabled,
            notice_seen_at: Some("2026-03-19T12:00:00Z".to_string()),
            updated_at: "2026-03-19T12:00:00Z".to_string(),
        };
        let effective = effective_settings(&workspace, Some(&persisted));
        let update = SkillUpdatePlan {
            skill: "release-notes".to_string(),
            scope: ManagedScope::Workspace,
            checked_at: "2026-03-19T12:01:00Z".to_string(),
            source: UpdateSourceSummary {
                kind: SourceKind::Git,
                url: "https://github.com/example/private-skill.git".to_string(),
                subpath: "skills/release-notes".to_string(),
            },
            pinned_revision: "0123456789abcdef".to_string(),
            latest_revision: Some("1111111111111111".to_string()),
            outcome: crate::state::UpdateCheckOutcome::UpdateAvailable,
            overlay_detected: false,
            overlay_changed_since_recorded_state: false,
            local_modification_detected: false,
            recommended_action: UpdateAction::Apply,
            available_actions: vec![UpdateAction::Apply],
            modifications: Vec::new(),
            missing_projections: Vec::new(),
            trust: Some(crate::trust::SkillTrust::local(false)),
            notes: Vec::new(),
            store_diverged: false,
        };

        let event = update_event(&workspace, &persisted, effective, &update);

        assert!(!event.emitted);
        assert_eq!(
            event.suppression_reason,
            Some(TelemetrySuppressionReason::PrivateSource)
        );
        assert_eq!(event.public_source, None);
    }
}
