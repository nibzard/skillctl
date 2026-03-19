//! Diagnostics and validation domain entry points.

use crate::{app::AppContext, error::AppError, response::AppResponse};

/// Placeholder diagnostics report.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DiagnosticReport;

/// Typed request for `skillctl doctor`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DoctorRequest;

/// Typed request for `skillctl validate`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ValidateRequest;

/// Handle `skillctl doctor`.
pub fn handle_doctor(
    _context: &AppContext,
    _request: DoctorRequest,
) -> Result<AppResponse, AppError> {
    Err(AppError::NotYetImplemented { command: "doctor" })
}

/// Handle `skillctl validate`.
pub fn handle_validate(
    _context: &AppContext,
    _request: ValidateRequest,
) -> Result<AppResponse, AppError> {
    Err(AppError::NotYetImplemented {
        command: "validate",
    })
}
