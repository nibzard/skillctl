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

/// The operation a trust gate refused.
///
/// Every gated operation moves an unreviewed script-bearing import to a
/// revision no recorded review covers. The refusal names the operation and the
/// repair that fits it, because the fork escape hatch is spelled differently
/// for an update apply than for an explicit revision move.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TrustGateOperation {
    /// Applying an upstream update.
    UpdateApply,
    /// Pinning an import to a revision.
    Pin,
    /// Rolling an import back to a recorded revision.
    Rollback,
}

impl TrustGateOperation {
    /// The wording the refusal uses for the refused operation.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UpdateApply => "update apply",
            Self::Pin => "pin",
            Self::Rollback => "rollback",
        }
    }

    /// The quoted fork command that takes local ownership instead of this
    /// operation.
    ///
    /// An update apply forks through its own `--strategy fork`; a pin or a
    /// rollback has no strategy flag, so the standalone `skillctl fork` is the
    /// command its refusal names.
    pub fn fork_repair(self, skill: &str) -> String {
        match self {
            Self::UpdateApply => format!("'skillctl update {skill} --apply --strategy fork'"),
            Self::Pin | Self::Rollback => format!("'skillctl fork {skill}'"),
        }
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
    /// Projection materialization failed validation.
    #[error("invalid projection materialization: {message}")]
    MaterializationValidation {
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
    /// The manifest a declared-state command requires is absent.
    #[error(
        "no manifest at '{path}': '{command}' reads the imports and overrides a manifest \
         declares, and this workspace holds none. Run 'skillctl init' to create it, or, in a \
         team checkout, retrieve the committed .agents/skillctl.yaml"
    )]
    ManifestMissing {
        /// Path to the missing manifest.
        path: PathBuf,
        /// The command that needs declared state.
        command: &'static str,
    },
    /// The lockfile a pinned-state command requires is absent.
    ///
    /// The message separates the two repairs because they are not
    /// interchangeable: `skillctl init` writes an empty lockfile, so it only
    /// fits a workspace that pinned nothing yet. A checkout that lost a
    /// populated lockfile must retrieve the committed file, or `verify` would
    /// report a healthy workspace that pins nothing.
    #[error(
        "no lockfile at '{path}': '{command}' reads the imports a lockfile pins, and this \
         workspace holds none. In a team checkout, retrieve the committed .agents/skillctl.lock. \
         Run 'skillctl init' only when this workspace pinned nothing yet: init writes an empty \
         lockfile, and an empty lockfile verifies zero imports as if nothing were pinned"
    )]
    LockfileMissing {
        /// Path to the missing lockfile.
        path: PathBuf,
        /// The command that needs pinned state.
        command: &'static str,
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
    /// Canonical bundle capture, parsing, or verification failed.
    #[error("canonical bundle is invalid: {message}")]
    BundleValidation {
        /// Validation message.
        message: String,
    },
    /// Canonical bundle serialization failed.
    #[error("failed to serialize a canonical bundle: {message}")]
    BundleSerialization {
        /// Serialization message.
        message: String,
    },
    /// Immutable artifact storage rejected an operation.
    #[error("artifact store '{path}' rejected the operation: {message}")]
    ArtifactValidation {
        /// Artifact store root.
        path: PathBuf,
        /// Validation message.
        message: String,
    },
    /// A locked dependency could not be reconstructed for a restore.
    #[error("restore of import '{import_id}' failed: {message}")]
    RestoreValidation {
        /// Identifier of the import that failed.
        import_id: String,
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
    /// No usable home directory could be determined for local state.
    #[error("failed to determine a home directory for local state")]
    HomeDirectoryUnavailable,
    /// Opening or bootstrapping the local state store failed.
    #[error("failed to open local state store '{path}': {source}")]
    LocalStateOpen {
        /// Path to the SQLite database.
        path: PathBuf,
        /// SQLite error.
        #[source]
        source: rusqlite::Error,
    },
    /// A local state store query or write failed.
    #[error("failed to {operation} in local state store '{path}': {source}")]
    LocalStateQuery {
        /// Path to the SQLite database.
        path: PathBuf,
        /// Operation that failed.
        operation: &'static str,
        /// SQLite error.
        #[source]
        source: rusqlite::Error,
    },
    /// Local state validation or schema verification failed.
    #[error("local state store '{path}' is invalid: {message}")]
    LocalStateValidation {
        /// Path to the SQLite database.
        path: PathBuf,
        /// Validation message.
        message: String,
    },
    /// The state database exists but SQLite cannot read it.
    ///
    /// The message names the recovery because a corrupt file otherwise blocks
    /// every state-backed command, including the recovery entry point.
    #[error(
        "local state store '{path}' is not a readable SQLite database: {reason}. Move the corrupt \
         file aside or run 'skillctl doctor --repair-state --apply'. The next command rebuilds \
         local state from .agents/skillctl.yaml and .agents/skillctl.lock"
    )]
    LocalStateDatabaseUnreadable {
        /// Path to the SQLite database.
        path: PathBuf,
        /// Why SQLite refused the file.
        reason: String,
    },
    /// Another process held the shared state coordination lock too long.
    #[error(
        "timed out after {waited_ms} ms waiting to {purpose} because the shared state lock \
         '{path}' is held by {holder}"
    )]
    LocalStateLockTimeout {
        /// Path to the coordination lock.
        path: PathBuf,
        /// What the waiting process intended to do.
        purpose: &'static str,
        /// How long the process waited.
        waited_ms: u64,
        /// Holder description recorded in the lock file.
        holder: String,
    },
    /// A state connection stopped waiting for a lock another process held.
    ///
    /// The message names the two wait controls because the raw SQLite text
    /// names neither, and a reader that meets this failure does not hold the
    /// coordination lock that the other control governs.
    #[error(
        "timed out after {timeout_ms} ms waiting to {operation} in local state store '{path}': \
         {reason}. Another process held the database lock longer than the state busy timeout. Set \
         SKILLCTL_STATE_BUSY_TIMEOUT_MS to change how long this command waits. \
         SKILLCTL_LOCK_TIMEOUT_MS and the state coordination lock control a different wait"
    )]
    LocalStateBusyTimeout {
        /// Path to the SQLite database.
        path: PathBuf,
        /// Operation that failed.
        operation: &'static str,
        /// Busy timeout the connection was configured with.
        timeout_ms: u64,
        /// SQLite failure text, for example "database is locked".
        reason: String,
    },
    /// A plan no longer matches the state it was built against.
    #[error("plan '{plan_id}' is stale: {message}")]
    StalePlan {
        /// Identifier of the rejected plan.
        plan_id: String,
        /// What changed since the plan was built.
        message: String,
    },
    /// A plan document was modified after it was written.
    #[error("plan '{plan_id}' is tampered: {message}")]
    TamperedPlan {
        /// Identifier of the rejected plan.
        plan_id: String,
        /// Why the document was rejected.
        message: String,
    },
    /// Unrecorded local edits block a destructive write.
    #[error("unrecorded local edits at '{path}' block this operation: {message}")]
    DirtyContent {
        /// Path that holds unrecorded edits.
        path: PathBuf,
        /// How to resolve the conflict.
        message: String,
    },
    /// A trust gate refused an apply before any content changed.
    #[error(
        "trust gate is blocking {operation} for '{skill}' until it is reviewed or forked{detail}; \
         record a local review with 'skillctl review {skill} --approve', or take local \
         ownership with {fork}",
        operation = operation.as_str(),
        fork = operation.fork_repair(skill),
        detail = detail.as_deref().unwrap_or_default()
    )]
    TrustGateBlocked {
        /// Skill whose trust gate refused the apply.
        skill: String,
        /// The refused operation, which names the repair that fits it.
        operation: TrustGateOperation,
        /// Clause naming the content the gate evaluated, when the installed
        /// content alone does not explain the refusal.
        detail: Option<String>,
    },
    /// A discovery source or index document failed validation.
    #[error("discovery source '{input}' is invalid: {message}")]
    DiscoveryValidation {
        /// User-provided origin or index URL, or the fetched index URL.
        input: String,
        /// Validation message.
        message: String,
    },
    /// Fetching a discovery document over the network failed.
    #[error("failed to fetch discovery index '{url}': {message}")]
    DiscoveryFetch {
        /// Index URL that could not be fetched.
        url: String,
        /// Network or HTTP failure description.
        message: String,
    },
    /// Fetching a discovery artifact over the network failed.
    #[error("failed to fetch discovery artifact for '{skill}' from '{url}': {message}")]
    DiscoveryArtifactFetch {
        /// Skill whose artifact could not be fetched.
        skill: String,
        /// Artifact URL that could not be fetched.
        url: String,
        /// Network or HTTP failure description.
        message: String,
    },
    /// A fetched discovery artifact does not match the digest its index declared.
    #[error(
        "discovery artifact for '{skill}' from '{url}' does not match its declared digest: the \
         index declares {declared}, the fetched bytes hash to {actual}; the content was not used"
    )]
    DiscoveryArtifactDigestMismatch {
        /// Skill whose artifact failed digest verification.
        skill: String,
        /// Artifact URL the bytes were fetched from.
        url: String,
        /// Digest the discovery index declared.
        declared: String,
        /// Digest computed over the fetched bytes.
        actual: String,
    },
    /// A fetched discovery artifact cannot be staged safely.
    #[error("discovery artifact for '{skill}' from '{url}' is rejected: {message}")]
    DiscoveryArtifactRejected {
        /// Skill whose artifact was rejected.
        skill: String,
        /// Artifact URL the bytes were fetched from.
        url: String,
        /// Why the artifact was rejected.
        message: String,
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
            | Self::MaterializationValidation { .. }
            | Self::ManifestMissing { .. }
            | Self::LockfileMissing { .. }
            | Self::ManifestParse { .. }
            | Self::ManifestValidation { .. }
            | Self::ManifestSerialize { .. }
            | Self::LockfileParse { .. }
            | Self::LockfileValidation { .. }
            | Self::LockfileSerialize { .. }
            | Self::SkillParse { .. }
            | Self::SkillValidation { .. }
            | Self::SourceValidation { .. }
            | Self::BundleValidation { .. }
            | Self::BundleSerialization { .. }
            | Self::ArtifactValidation { .. }
            | Self::RestoreValidation { .. }
            | Self::LocalStateValidation { .. }
            | Self::StalePlan { .. }
            | Self::TamperedPlan { .. }
            | Self::DirtyContent { .. }
            | Self::DiscoveryValidation { .. }
            | Self::DiscoveryArtifactDigestMismatch { .. }
            | Self::DiscoveryArtifactRejected { .. } => ExitStatus::ValidationFailure,
            Self::TrustGateBlocked { .. } => ExitStatus::TrustGateBlocked,
            Self::DiscoveryFetch { .. } | Self::DiscoveryArtifactFetch { .. } => {
                ExitStatus::OperationalError
            }
            Self::LocalStateLockTimeout { .. } => ExitStatus::OperationalError,
            Self::LocalStateBusyTimeout { .. } => ExitStatus::OperationalError,
            Self::ExternalCommand { .. }
            | Self::HomeDirectoryUnavailable
            | Self::LocalStateOpen { .. }
            | Self::LocalStateQuery { .. }
            | Self::LocalStateDatabaseUnreadable { .. } => ExitStatus::OperationalError,
            Self::InputRequired { .. } => ExitStatus::InputRequired,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_trust_gate_refusal_maps_to_exit_code_4() {
        let error = AppError::TrustGateBlocked {
            skill: "delta".to_string(),
            operation: TrustGateOperation::UpdateApply,
            detail: None,
        };

        assert_eq!(error.exit_status(), ExitStatus::TrustGateBlocked);
        assert_eq!(error.exit_status().code(), 4);
        let message = error.to_string();
        assert!(
            message.contains("trust gate is blocking update apply"),
            "unexpected message: {message}",
        );
        assert!(
            message.contains("until it is reviewed or forked; record a local review"),
            "the installed-content refusal keeps its sentence: {message}",
        );
    }

    #[test]
    fn a_trust_gate_refusal_for_an_incoming_revision_names_it() {
        let error = AppError::TrustGateBlocked {
            skill: "delta".to_string(),
            operation: TrustGateOperation::UpdateApply,
            detail: Some(
                "; the incoming revision def456 adds a top-level scripts/ directory no recorded \
                 review covers"
                    .to_string(),
            ),
        };

        assert_eq!(error.exit_status().code(), 4);
        let message = error.to_string();
        assert!(
            message.contains("the incoming revision def456 adds a top-level scripts/ directory"),
            "the refusal names the content that would arrive: {message}",
        );
        assert!(
            message.contains("'skillctl review delta --approve'"),
            "the refusal keeps naming the review: {message}",
        );
    }

    #[test]
    fn a_trust_gate_refusal_names_the_operation_and_its_fork_repair() {
        for (operation, wording, fork) in [
            (
                TrustGateOperation::UpdateApply,
                "update apply",
                "'skillctl update delta --apply --strategy fork'",
            ),
            (TrustGateOperation::Pin, "pin", "'skillctl fork delta'"),
            (
                TrustGateOperation::Rollback,
                "rollback",
                "'skillctl fork delta'",
            ),
        ] {
            let error = AppError::TrustGateBlocked {
                skill: "delta".to_string(),
                operation,
                detail: None,
            };

            assert_eq!(error.exit_status().code(), 4);
            let message = error.to_string();
            assert!(
                message.contains(&format!("trust gate is blocking {wording} for 'delta'")),
                "the refusal names the refused operation: {message}",
            );
            assert!(
                message.contains(fork),
                "the refusal names the fork that fits {wording}: {message}",
            );
            assert!(
                message.contains("'skillctl review delta --approve'"),
                "every refusal names the review: {message}",
            );
        }
    }

    #[test]
    fn a_busy_timeout_failure_names_the_wait_it_exhausted() {
        let error = AppError::LocalStateBusyTimeout {
            path: PathBuf::from("/home/user/.skillctl/state.db"),
            operation: "read schema version",
            timeout_ms: 60_000,
            reason: "database is locked".to_string(),
        };

        assert_eq!(error.exit_status(), ExitStatus::OperationalError);
        assert_eq!(error.exit_status().code(), 1);
        let message = error.to_string();
        assert!(
            message.contains("SKILLCTL_STATE_BUSY_TIMEOUT_MS"),
            "the failure names the wait control: {message}"
        );
        assert!(
            message.contains("SKILLCTL_LOCK_TIMEOUT_MS"),
            "the failure separates the coordination lock wait: {message}"
        );
        assert!(
            message.contains("database is locked"),
            "the failure keeps the SQLite text: {message}"
        );
        assert!(
            message.contains("timed out after 60000 ms"),
            "the failure states the wait it used: {message}"
        );
    }

    #[test]
    fn every_documented_exit_status_keeps_its_documented_code() {
        let cases = [
            (ExitStatus::Success, 0),
            (ExitStatus::OperationalError, 1),
            (ExitStatus::SuccessWithWarnings, 2),
            (ExitStatus::ValidationFailure, 3),
            (ExitStatus::TrustGateBlocked, 4),
            (ExitStatus::InputRequired, 5),
        ];

        for (status, code) in cases {
            assert_eq!(status.code(), code, "{status:?} must keep code {code}");
        }
    }
}
