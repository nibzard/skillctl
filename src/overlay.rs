//! Overlay and detachment domain entry points.

use std::path::PathBuf;

use crate::{app::AppContext, error::AppError, response::AppResponse};

/// Default relative path to the overlay root.
pub const DEFAULT_OVERLAYS_DIR: &str = ".agents/overlays";

/// Placeholder overlay root definition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OverlayRoot {
    /// Filesystem path to the overlay root.
    pub path: PathBuf,
}

impl Default for OverlayRoot {
    fn default() -> Self {
        Self {
            path: PathBuf::from(DEFAULT_OVERLAYS_DIR),
        }
    }
}

/// Typed request for `skillctl override`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OverrideRequest {
    /// Managed skill name.
    pub skill: String,
}

impl OverrideRequest {
    /// Create an override request from parsed CLI arguments.
    pub fn new(skill: String) -> Self {
        Self { skill }
    }
}

/// Typed request for `skillctl fork`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ForkRequest {
    /// Managed skill name.
    pub skill: String,
}

impl ForkRequest {
    /// Create a fork request from parsed CLI arguments.
    pub fn new(skill: String) -> Self {
        Self { skill }
    }
}

/// Handle `skillctl override`.
pub fn handle_override(
    _context: &AppContext,
    _request: OverrideRequest,
) -> Result<AppResponse, AppError> {
    Err(AppError::NotYetImplemented {
        command: "override",
    })
}

/// Handle `skillctl fork`.
pub fn handle_fork(_context: &AppContext, _request: ForkRequest) -> Result<AppResponse, AppError> {
    Err(AppError::NotYetImplemented { command: "fork" })
}
