//! Typed errors and exit statuses for the command runtime.

use std::{io, path::PathBuf};

use thiserror::Error;

/// Process exit statuses defined by the specification.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExitStatus {
    /// Successful execution with no warnings.
    Success = 0,
    /// Operational failure.
    OperationalError = 1,
    /// Successful execution with warnings.
    SuccessWithWarnings = 2,
    /// Validation or conflict failure.
    ValidationFailure = 3,
    /// Trust gate blocked the operation.
    TrustGateBlocked = 4,
    /// Interactive input was required but not allowed.
    InputRequired = 5,
}

impl ExitStatus {
    /// Return the numeric exit code for the status.
    pub const fn code(self) -> u8 {
        self as u8
    }
}

/// Structured application errors preserved until presentation.
#[derive(Debug, Error)]
pub enum AppError {
    /// The current working directory could not be resolved.
    #[error("failed to determine the current working directory: {source}")]
    CurrentWorkingDirectory {
        /// Source I/O error.
        #[source]
        source: io::Error,
    },
    /// The requested working directory could not be inspected.
    #[error("working directory '{path}' is unavailable: {source}")]
    WorkingDirectoryUnavailable {
        /// Path that failed validation.
        path: PathBuf,
        /// Source I/O error.
        #[source]
        source: io::Error,
    },
    /// The requested working directory points to a non-directory.
    #[error("working directory '{path}' is not a directory")]
    WorkingDirectoryNotDirectory {
        /// Invalid directory path.
        path: PathBuf,
    },
    /// The command has no implementation yet.
    #[error("command '{command}' is not implemented yet")]
    NotYetImplemented {
        /// Stable command identifier.
        command: &'static str,
    },
    /// The command requires interactive input.
    #[error("interactive input is required for command '{command}'")]
    InputRequired {
        /// Stable command identifier.
        command: &'static str,
    },
    /// JSON output rendering failed.
    #[error("failed to render JSON output: {source}")]
    JsonRender {
        /// Serialization error.
        #[from]
        source: serde_json::Error,
    },
}

impl AppError {
    /// Map the typed error to a stable process exit status.
    pub const fn exit_status(&self) -> ExitStatus {
        match self {
            Self::CurrentWorkingDirectory { .. }
            | Self::WorkingDirectoryUnavailable { .. }
            | Self::WorkingDirectoryNotDirectory { .. }
            | Self::NotYetImplemented { .. }
            | Self::JsonRender { .. } => ExitStatus::OperationalError,
            Self::InputRequired { .. } => ExitStatus::InputRequired,
        }
    }
}
