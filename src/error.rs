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
    /// A filesystem path exists but is not the expected kind.
    #[error("path '{path}' exists but is not a {expected}")]
    PathConflict {
        /// Invalid path.
        path: PathBuf,
        /// Expected filesystem object kind.
        expected: &'static str,
    },
    /// A filesystem operation failed.
    #[error("failed to {action} '{path}': {source}")]
    FilesystemOperation {
        /// What the operation attempted to do.
        action: &'static str,
        /// Path involved in the failure.
        path: PathBuf,
        /// Source I/O error.
        #[source]
        source: io::Error,
    },
    /// A `.git` indirection file had an unsupported format.
    #[error("git metadata file '{path}' is not in the expected 'gitdir: <path>' format")]
    InvalidGitDirFile {
        /// Path to the invalid git metadata file.
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
    /// Projection-root planning failed validation.
    #[error("invalid projection plan: {message}")]
    PlannerValidation {
        /// Validation message.
        message: String,
    },
    /// Effective-skill resolution failed validation.
    #[error("invalid effective skill resolution: {message}")]
    ResolutionValidation {
        /// Validation message.
        message: String,
    },
    /// JSON output rendering failed.
    #[error("failed to render JSON output: {source}")]
    JsonRender {
        /// Serialization error.
        #[from]
        source: serde_json::Error,
    },
    /// Manifest YAML parsing failed.
    #[error("failed to parse manifest '{path}': {source}")]
    ManifestParse {
        /// Path to the manifest file.
        path: PathBuf,
        /// Serialization error.
        #[source]
        source: serde_yaml::Error,
    },
    /// Manifest validation failed.
    #[error("manifest '{path}' is invalid: {message}")]
    ManifestValidation {
        /// Path to the manifest file.
        path: PathBuf,
        /// Validation message.
        message: String,
    },
    /// Manifest YAML rendering failed.
    #[error("failed to render manifest '{path}': {source}")]
    ManifestSerialize {
        /// Path to the manifest file.
        path: PathBuf,
        /// Serialization error.
        #[source]
        source: serde_yaml::Error,
    },
    /// Lockfile YAML parsing failed.
    #[error("failed to parse lockfile '{path}': {source}")]
    LockfileParse {
        /// Path to the lockfile file.
        path: PathBuf,
        /// Serialization error.
        #[source]
        source: serde_yaml::Error,
    },
    /// Lockfile validation failed.
    #[error("lockfile '{path}' is invalid: {message}")]
    LockfileValidation {
        /// Path to the lockfile file.
        path: PathBuf,
        /// Validation message.
        message: String,
    },
    /// Lockfile YAML rendering failed.
    #[error("failed to render lockfile '{path}': {source}")]
    LockfileSerialize {
        /// Path to the lockfile file.
        path: PathBuf,
        /// Serialization error.
        #[source]
        source: serde_yaml::Error,
    },
    /// Skill frontmatter parsing failed.
    #[error("failed to parse skill '{path}': {source}")]
    SkillParse {
        /// Path to the skill manifest file.
        path: PathBuf,
        /// Serialization error.
        #[source]
        source: serde_yaml::Error,
    },
    /// Skill validation failed.
    #[error("skill '{path}' is invalid: {message}")]
    SkillValidation {
        /// Path to the skill manifest file or invalid overlay path.
        path: PathBuf,
        /// Validation message.
        message: String,
    },
    /// Install source normalization, staging, or detection failed validation.
    #[error("install source '{input}' is invalid: {message}")]
    SourceValidation {
        /// User-provided install source.
        input: String,
        /// Validation message.
        message: String,
    },
    /// An external helper command required by source staging failed to launch.
    #[error("failed to run external command '{command}': {source}")]
    ExternalCommand {
        /// Command binary that failed to launch.
        command: String,
        /// Underlying process-spawn error.
        #[source]
        source: io::Error,
    },
}

impl AppError {
    /// Map the typed error to a stable process exit status.
    pub const fn exit_status(&self) -> ExitStatus {
        match self {
            Self::CurrentWorkingDirectory { .. }
            | Self::WorkingDirectoryUnavailable { .. }
            | Self::WorkingDirectoryNotDirectory { .. }
            | Self::PathConflict { .. }
            | Self::FilesystemOperation { .. }
            | Self::InvalidGitDirFile { .. }
            | Self::NotYetImplemented { .. }
            | Self::JsonRender { .. } => ExitStatus::OperationalError,
            Self::PlannerValidation { .. }
            | Self::ResolutionValidation { .. }
            | Self::ManifestParse { .. }
            | Self::ManifestValidation { .. }
            | Self::ManifestSerialize { .. }
            | Self::LockfileParse { .. }
            | Self::LockfileValidation { .. }
            | Self::LockfileSerialize { .. }
            | Self::SkillParse { .. }
            | Self::SkillValidation { .. }
            | Self::SourceValidation { .. } => ExitStatus::ValidationFailure,
            Self::ExternalCommand { .. } => ExitStatus::OperationalError,
            Self::InputRequired { .. } => ExitStatus::InputRequired,
        }
    }
}
