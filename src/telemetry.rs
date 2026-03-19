//! Telemetry domain entry points.

use url::Url;

use crate::{
    app::AppContext,
    cli::TelemetryCommand,
    error::AppError,
    response::AppResponse,
    source::{NormalizedInstallSource, SourceKind},
};

/// Supported telemetry collection modes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TelemetryMode {
    /// Public-source telemetry only.
    PublicOnly,
    /// Telemetry disabled.
    Off,
}

/// Placeholder telemetry settings.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TelemetrySettings {
    /// Whether telemetry is enabled.
    pub enabled: bool,
    /// Effective telemetry mode.
    pub mode: TelemetryMode,
}

/// Public-only telemetry visibility classification for one install source.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SourceVisibility {
    /// The source is public enough for content-free aggregate telemetry.
    Public,
    /// The source is local-only and must never be emitted remotely.
    SuppressedLocal,
    /// The source is remote but likely private and must never be emitted remotely.
    SuppressedPrivate,
}

/// Classify one normalized install source for public-only telemetry emission.
pub fn classify_source_visibility(source: &NormalizedInstallSource) -> SourceVisibility {
    match source.kind {
        SourceKind::LocalPath | SourceKind::Archive => SourceVisibility::SuppressedLocal,
        SourceKind::Git => classify_git_source_visibility(source),
    }
}

/// Return whether a remote telemetry event is allowed for this source and settings.
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
    _context: &AppContext,
    command: &TelemetryCommand,
) -> Result<AppResponse, AppError> {
    match command {
        TelemetryCommand::Status => Err(AppError::NotYetImplemented {
            command: "telemetry-status",
        }),
        TelemetryCommand::Enable => Err(AppError::NotYetImplemented {
            command: "telemetry-enable",
        }),
        TelemetryCommand::Disable => Err(AppError::NotYetImplemented {
            command: "telemetry-disable",
        }),
    }
}

fn classify_git_source_visibility(source: &NormalizedInstallSource) -> SourceVisibility {
    if source.url.starts_with("git@") || source.raw.starts_with("git@") {
        return SourceVisibility::SuppressedPrivate;
    }

    let parsed = match Url::parse(source.url.as_str()) {
        Ok(parsed) => parsed,
        Err(_) => return SourceVisibility::SuppressedPrivate,
    };

    match parsed.scheme() {
        "file" => SourceVisibility::SuppressedLocal,
        "ssh" => SourceVisibility::SuppressedPrivate,
        "http" | "https" | "git" => {
            if !parsed.username().is_empty() {
                return SourceVisibility::SuppressedPrivate;
            }

            match parsed.host_str() {
                Some(host) if is_local_host(host) => SourceVisibility::SuppressedLocal,
                Some(_) => SourceVisibility::Public,
                None => SourceVisibility::SuppressedPrivate,
            }
        }
        _ => SourceVisibility::SuppressedPrivate,
    }
}

fn is_local_host(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "::1")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(kind: SourceKind, raw: &str, url: &str) -> NormalizedInstallSource {
        NormalizedInstallSource {
            raw: raw.to_string(),
            kind,
            url: url.to_string(),
            display: raw.to_string(),
        }
    }

    #[test]
    fn public_git_sources_remain_eligible_for_public_only_telemetry() {
        let settings = TelemetrySettings {
            enabled: true,
            mode: TelemetryMode::PublicOnly,
        };
        let https = source(
            SourceKind::Git,
            "https://github.com/example/release-notes.git",
            "https://github.com/example/release-notes.git",
        );
        let git = source(
            SourceKind::Git,
            "git://example.com/release-notes.git",
            "git://example.com/release-notes.git",
        );

        assert_eq!(classify_source_visibility(&https), SourceVisibility::Public);
        assert_eq!(classify_source_visibility(&git), SourceVisibility::Public);
        assert!(allows_remote_emission(settings, &https));
        assert!(allows_remote_emission(settings, &git));
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
}
