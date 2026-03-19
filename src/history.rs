//! History ledger and version-tracking domain entry points.

use crate::{app::AppContext, error::AppError, response::AppResponse};

/// Kinds of history events the ledger will record.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HistoryEventKind {
    /// Skill installation.
    Install,
    /// Skill update.
    Update,
    /// Revision pinning.
    Pin,
    /// Rollback activation.
    Rollback,
}

/// Typed request for `skillctl pin`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PinRequest {
    /// Managed skill name.
    pub skill: String,
    /// Exact revision to pin.
    pub reference: String,
}

impl PinRequest {
    /// Create a pin request from parsed CLI arguments.
    pub fn new(skill: String, reference: String) -> Self {
        Self { skill, reference }
    }
}

/// Typed request for `skillctl rollback`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RollbackRequest {
    /// Managed skill name.
    pub skill: String,
    /// Previous version or commit identifier.
    pub version_or_commit: String,
}

impl RollbackRequest {
    /// Create a rollback request from parsed CLI arguments.
    pub fn new(skill: String, version_or_commit: String) -> Self {
        Self {
            skill,
            version_or_commit,
        }
    }
}

/// Typed request for `skillctl history`.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct HistoryRequest {
    /// Optional managed skill filter.
    pub skill: Option<String>,
}

impl HistoryRequest {
    /// Create a history request from parsed CLI arguments.
    pub fn new(skill: Option<String>) -> Self {
        Self { skill }
    }
}

/// Handle `skillctl pin`.
pub fn handle_pin(_context: &AppContext, _request: PinRequest) -> Result<AppResponse, AppError> {
    Err(AppError::NotYetImplemented { command: "pin" })
}

/// Handle `skillctl rollback`.
pub fn handle_rollback(
    _context: &AppContext,
    _request: RollbackRequest,
) -> Result<AppResponse, AppError> {
    Err(AppError::NotYetImplemented {
        command: "rollback",
    })
}

/// Handle `skillctl history`.
pub fn handle_history(
    _context: &AppContext,
    _request: HistoryRequest,
) -> Result<AppResponse, AppError> {
    Err(AppError::NotYetImplemented { command: "history" })
}
