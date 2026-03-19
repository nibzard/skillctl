//! Telemetry domain entry points.

use crate::{app::AppContext, cli::TelemetryCommand, error::AppError, response::AppResponse};

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
