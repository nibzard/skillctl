//! Lockfile domain types and constants.

use std::path::PathBuf;

/// Default relative path to the workspace lockfile.
pub const DEFAULT_LOCKFILE_PATH: &str = ".agents/skillctl.lock";

/// Placeholder model for the workspace lockfile.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceLockfile {
    /// Filesystem path to the lockfile.
    pub path: PathBuf,
}

impl Default for WorkspaceLockfile {
    fn default() -> Self {
        Self {
            path: PathBuf::from(DEFAULT_LOCKFILE_PATH),
        }
    }
}
