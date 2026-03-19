//! Source detection and install domain entry points.

use crate::{app::AppContext, error::AppError, response::AppResponse};

/// Supported install source categories.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SourceKind {
    /// Remote Git repository.
    Git,
    /// Local directory path.
    LocalPath,
    /// Local archive file.
    Archive,
}

/// Placeholder install source definition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstallSource {
    /// Unnormalized source value from the CLI.
    pub raw: String,
}

/// Typed request for `skillctl install`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstallRequest {
    /// Requested install source.
    pub source: InstallSource,
}

impl InstallRequest {
    /// Create an install request from parsed CLI arguments.
    pub fn new(source: String) -> Self {
        Self {
            source: InstallSource { raw: source },
        }
    }
}

/// Handle `skillctl install`.
pub fn handle_install(
    _context: &AppContext,
    _request: InstallRequest,
) -> Result<AppResponse, AppError> {
    Err(AppError::NotYetImplemented { command: "install" })
}
