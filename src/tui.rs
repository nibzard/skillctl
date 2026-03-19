//! Terminal UI domain entry points.

use crate::{app::AppContext, error::AppError, response::AppResponse};

/// Placeholder terminal UI application state.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TuiApp;

/// Typed request for `skillctl tui`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct OpenTuiRequest;

/// Handle `skillctl tui`.
pub fn handle_open(
    _context: &AppContext,
    _request: OpenTuiRequest,
) -> Result<AppResponse, AppError> {
    Err(AppError::NotYetImplemented { command: "tui" })
}
