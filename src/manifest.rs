//! Workspace manifest domain entry points.

use std::path::PathBuf;

use crate::{app::AppContext, error::AppError, response::AppResponse};

/// Default relative path to the workspace manifest.
pub const DEFAULT_MANIFEST_PATH: &str = ".agents/skillctl.yaml";

/// Placeholder model for the workspace manifest.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceManifest {
    /// Filesystem path to the manifest.
    pub path: PathBuf,
}

impl Default for WorkspaceManifest {
    fn default() -> Self {
        Self {
            path: PathBuf::from(DEFAULT_MANIFEST_PATH),
        }
    }
}

/// Typed request for `skillctl init`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct InitRequest;

/// Handle `skillctl init`.
pub fn handle_init(_context: &AppContext, _request: InitRequest) -> Result<AppResponse, AppError> {
    Err(AppError::NotYetImplemented { command: "init" })
}
