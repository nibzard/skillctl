//! Projection materialization and cleanup domain entry points.

use crate::{app::AppContext, error::AppError, response::AppResponse};

/// Placeholder materialization report.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MaterializationReport;

/// Typed request for `skillctl sync`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SyncRequest;

/// Typed request for `skillctl clean`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CleanRequest;

/// Handle `skillctl sync`.
pub fn handle_sync(_context: &AppContext, _request: SyncRequest) -> Result<AppResponse, AppError> {
    Err(AppError::NotYetImplemented { command: "sync" })
}

/// Handle `skillctl clean`.
pub fn handle_clean(
    _context: &AppContext,
    _request: CleanRequest,
) -> Result<AppResponse, AppError> {
    Err(AppError::NotYetImplemented { command: "clean" })
}
