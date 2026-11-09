//! Durable operation journal, staged replacement, and state recovery.
//!
//! A lifecycle operation records what it is about to change before it changes
//! it. The journal lives under the local state root, survives process
//! termination, and carries enough information to restore the previous state
//! of every tracked path, including a logical snapshot of the shared SQLite
//! database. The database is never restored by copying a file over a live
//! database; rows are restored through SQLite inside one transaction while the
//! coordination lock is held.

use std::{
    fs,
    io::{self, Write as _},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::error::AppError;

/// Journal record schema version.
const JOURNAL_SCHEMA_VERSION: u32 = 1;
/// Directory under the local state root that holds operation journals.
const JOURNAL_DIR: &str = "journal";
/// Directory under the local state root that holds migration backups.
const BACKUP_DIR: &str = "backups";
/// File name of the journal record inside one journal directory.
const JOURNAL_FILE: &str = "journal.json";
/// File name of the state database snapshot inside one journal directory.
const DATABASE_SNAPSHOT_FILE: &str = "state-snapshot.db";
/// Directory inside one journal directory that holds staged path backups.
const BACKUP_SUBDIR: &str = "backup";
/// Number of migration backups retained before the oldest are pruned.
const MIGRATION_BACKUP_RETENTION: usize = 3;

/// One journaled lifecycle operation on disk.
pub(crate) struct OperationJournal {
    directory: PathBuf,
    record: JournalRecord,
}

/// Metadata recorded for one tracked path inside a journal.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct TrackedPath {
    /// Absolute path that was captured before the operation changed it.
    pub path: String,
    /// Captured kind: `missing`, `file`, `directory`, or `symlink`.
    pub kind: String,
    /// Relative location of the staged backup inside the journal directory.
    pub backup: Option<String>,
    /// Link target when `kind` is `symlink`.
    pub symlink_target: Option<String>,
    /// Whether the link target was a directory.
    pub symlink_is_dir: Option<bool>,
    /// Whether this path is the shared state database.
    pub state_database: bool,
}

/// Journal metadata persisted as `journal.json`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct JournalRecord {
    /// Journal record schema version.
    pub schema_version: u32,
    /// Unique journal identifier.
    pub id: String,
    /// Lifecycle operation that created the journal.
    pub operation: String,
    /// Workspace key that owns the operation.
    pub workspace_key: String,
    /// Absolute workspace path recorded for diagnostics.
    pub workspace_path: String,
    /// Process identifier that created the journal.
    pub pid: u32,
    /// Creation timestamp.
    pub started_at: String,
    /// `active`, `completed`, `rolled-back`, or `needs-review`.
    pub state: String,
    /// Ordered journal boundaries reached so far.
    pub steps: Vec<String>,
    /// Paths captured before mutation.
    pub tracked_paths: Vec<TrackedPath>,
    /// Schema version of the state database at snapshot time, when known.
    pub state_database_version: Option<u32>,
}

/// A journal found on disk that never reached a terminal state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingOperation {
    /// Journal identifier.
    pub id: String,
    /// Lifecycle operation that created the journal.
    pub operation: String,
    /// Journal state: `active` or `needs-review`.
    pub state: String,
    /// Absolute workspace path recorded when the journal was created.
    pub workspace_path: String,
    /// Process identifier that created the journal.
    pub pid: u32,
    /// Creation timestamp.
    pub started_at: String,
    /// Journal boundaries reached before the process stopped.
    pub steps: Vec<String>,
    /// Paths the journal can restore.
    pub tracked_paths: Vec<String>,
    /// Absolute journal directory.
    pub directory: PathBuf,
}

/// Outcome of recovering one pending operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RecoveryOutcome {
    /// The journal restored the recorded pre-operation state.
    Recovered(PendingOperation),
    /// The journal needs a human decision and was left untouched.
    NeedsReview {
        /// Journal identifier.
        id: String,
        /// Why automatic recovery refused to act.
        reason: String,
    },
    /// The journal record could not be read.
    Unreadable {
        /// Absolute journal directory.
        directory: PathBuf,
        /// Read failure.
        reason: String,
    },
}

impl OperationJournal {
    /// Create a new active journal for one lifecycle operation.
    pub(crate) fn create(
        state_root: &Path,
        workspace_key: &str,
        workspace_path: &Path,
        operation: &'static str,
    ) -> Result<Self, AppError> {
        let started_at = SystemTime::now();
        let id = format!(
            "{operation}-{}-{}",
            started_at
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or_default(),
            std::process::id()
        );
        let directory = journal_root(state_root)
            .join(sanitize_component(workspace_key))
            .join(sanitize_component(&id));

        fs::create_dir_all(&directory).map_err(|source| AppError::FilesystemOperation {
            action: "create lifecycle journal directory",
            path: directory.clone(),
            source,
        })?;

        let mut journal = Self {
            directory,
            record: JournalRecord {
                schema_version: JOURNAL_SCHEMA_VERSION,
                id,
                operation: operation.to_string(),
                workspace_key: workspace_key.to_string(),
                workspace_path: workspace_path.to_string_lossy().into_owned(),
                pid: std::process::id(),
                started_at: coarse_timestamp(),
                state: JournalState::Active.as_str().to_string(),
                steps: Vec::new(),
                tracked_paths: Vec::new(),
                state_database_version: None,
            },
        };
        journal.record_step("opened")?;
        Ok(journal)
    }

    /// Record one journal boundary durably.
    pub(crate) fn record_step(&mut self, label: &str) -> Result<(), AppError> {
        self.record.steps.push(label.to_string());
        self.persist()
    }

    /// Capture the pre-operation state of one filesystem path.
    pub(crate) fn track_path(&mut self, path: &Path) -> Result<(), AppError> {
        let recorded = path.to_string_lossy().into_owned();
        if self
            .record
            .tracked_paths
            .iter()
            .any(|tracked| tracked.path == recorded)
        {
            return Ok(());
        }

        let index = self.record.tracked_paths.len();
        let staged = format!("{BACKUP_SUBDIR}/{index:04}");
        let captured = snapshot_path(path, &self.directory.join(&staged))?;

        self.record.tracked_paths.push(TrackedPath {
            kind: captured.kind().to_string(),
            backup: captured.staged_backup().map(|_| staged),
            symlink_target: captured.symlink_target(),
            symlink_is_dir: captured.symlink_is_dir(),
            path: recorded.clone(),
            state_database: false,
        });
        self.record_step(&format!("tracked:{recorded}"))
    }

    /// Capture a snapshot of the shared state database.
    ///
    /// The snapshot is taken with SQLite's online backup API, so it is
    /// consistent with concurrent readers and records the schema version as
    /// well as the rows.
    pub(crate) fn track_state_database(&mut self, database_path: &Path) -> Result<(), AppError> {
        let recorded = database_path.to_string_lossy().into_owned();
        if self
            .record
            .tracked_paths
            .iter()
            .any(|tracked| tracked.state_database)
        {
            return Ok(());
        }

        if !database_path.exists() {
            self.record.tracked_paths.push(TrackedPath {
                path: recorded,
                kind: "missing".to_string(),
                backup: None,
                symlink_target: None,
                symlink_is_dir: None,
                state_database: true,
            });
            self.record.state_database_version = None;
            return self.record_step("tracked-state-database:missing");
        }

        let snapshot_path = self.directory.join(DATABASE_SNAPSHOT_FILE);
        let user_version = snapshot_state_database(database_path, &snapshot_path)?;
        self.record.state_database_version = Some(user_version);
        self.record.tracked_paths.push(TrackedPath {
            path: recorded,
            kind: "file".to_string(),
            backup: Some(DATABASE_SNAPSHOT_FILE.to_string()),
            symlink_target: None,
            symlink_is_dir: None,
            state_database: true,
        });
        self.record_step("tracked-state-database")
    }

    /// Restore every tracked path to its captured state.
    ///
    /// Filesystem paths are restored deepest path first. The state database is
    /// restored last through SQLite's backup API, so the live database file is
    /// never replaced behind SQLite's back.
    pub(crate) fn restore(&mut self) -> Result<(), AppError> {
        self.record_step("restoring")?;

        let mut filesystem: Vec<&TrackedPath> = self
            .record
            .tracked_paths
            .iter()
            .filter(|tracked| !tracked.state_database)
            .collect();
        filesystem.sort_by_key(|tracked| {
            std::cmp::Reverse(Path::new(&tracked.path).components().count())
        });

        for tracked in filesystem {
            restore_filesystem_path(tracked, &self.directory)?;
        }

        for tracked in self
            .record
            .tracked_paths
            .iter()
            .filter(|t| t.state_database)
        {
            restore_database_path(tracked, &self.directory)?;
        }

        Ok(())
    }

    /// Move the journal to a terminal state and remove its directory.
    pub(crate) fn finish(&mut self, state: JournalState) -> Result<(), AppError> {
        self.record.state = state.as_str().to_string();
        self.persist()?;
        remove_journal_directory(&self.directory)
    }

    /// Keep the journal on disk in the `needs-review` state.
    pub(crate) fn mark_needs_review(&mut self, reason: &str) -> Result<(), AppError> {
        self.record.state = JournalState::NeedsReview.as_str().to_string();
        self.record.steps.push(format!("needs-review:{reason}"));
        self.persist()
    }

    fn persist(&self) -> Result<(), AppError> {
        write_file_atomically(
            &self.directory.join(JOURNAL_FILE),
            &serde_json::to_vec(&self.record)?,
        )
    }
}

/// Terminal or in-progress state of one journal.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum JournalState {
    /// The operation was interrupted before it reached a terminal state.
    Active,
    /// The operation finished successfully.
    Completed,
    /// The operation was rolled back to its recorded pre-state.
    RolledBack,
    /// Automatic recovery refused to act and a human decision is required.
    NeedsReview,
}

impl JournalState {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Completed => "completed",
            Self::RolledBack => "rolled-back",
            Self::NeedsReview => "needs-review",
        }
    }
}

/// Shape of one captured path plus the details needed to restore it.
enum CapturedPath {
    Missing,
    File,
    Directory,
    Symlink { target: PathBuf, is_dir: bool },
}

impl CapturedPath {
    fn kind(&self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::File => "file",
            Self::Directory => "directory",
            Self::Symlink { .. } => "symlink",
        }
    }

    fn staged_backup(&self) -> Option<()> {
        match self {
            Self::Missing => None,
            _ => Some(()),
        }
    }

    fn symlink_target(&self) -> Option<String> {
        match self {
            Self::Symlink { target, .. } => Some(target.to_string_lossy().into_owned()),
            _ => None,
        }
    }

    fn symlink_is_dir(&self) -> Option<bool> {
        match self {
            Self::Symlink { is_dir, .. } => Some(*is_dir),
            _ => None,
        }
    }
}

/// Return the journal root under one local state root.
pub fn journal_root(state_root: &Path) -> PathBuf {
    state_root.join(JOURNAL_DIR)
}

/// Return the migration backup directory under one local state root.
pub fn migration_backup_root(state_root: &Path) -> PathBuf {
    state_root.join(BACKUP_DIR)
}

/// Coarse wall-clock timestamp used inside journal and lock records.
pub(crate) fn coarse_timestamp() -> String {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => format!("{}.{:06}", duration.as_secs(), duration.subsec_micros()),
        Err(_) => "0".to_string(),
    }
}

/// List journals under one state root that never reached a terminal state.
pub fn pending_operations(state_root: &Path) -> Result<Vec<PendingOperation>, AppError> {
    let root = journal_root(state_root);
    if !root.is_dir() {
        return Ok(Vec::new());
    }

    let mut pending = Vec::new();
    let workspace_dirs = fs::read_dir(&root)
        .map_err(|source| AppError::FilesystemOperation {
            action: "list lifecycle journal workspaces",
            path: root.clone(),
            source,
        })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| AppError::FilesystemOperation {
            action: "read lifecycle journal workspace entry",
            path: root.clone(),
            source,
        })?;

    for workspace_dir in workspace_dirs {
        if !workspace_dir.path().is_dir() {
            continue;
        }
        let journal_dirs = fs::read_dir(workspace_dir.path())
            .map_err(|source| AppError::FilesystemOperation {
                action: "list lifecycle journals",
                path: workspace_dir.path(),
                source,
            })?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|source| AppError::FilesystemOperation {
                action: "read lifecycle journal entry",
                path: workspace_dir.path(),
                source,
            })?;

        for journal_dir in journal_dirs {
            let directory = journal_dir.path();
            let Ok(record) = read_journal_record(&directory) else {
                continue;
            };
            if record.state != JournalState::Active.as_str()
                && record.state != JournalState::NeedsReview.as_str()
            {
                continue;
            }
            pending.push(PendingOperation {
                id: record.id,
                operation: record.operation,
                state: record.state.clone(),
                workspace_path: record.workspace_path,
                pid: record.pid,
                started_at: record.started_at,
                steps: record.steps,
                tracked_paths: record
                    .tracked_paths
                    .iter()
                    .map(|tracked| tracked.path.clone())
                    .collect(),
                directory,
            });
        }
    }

    pending.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(pending)
}

/// Recover every pending journal under one state root.
///
/// Recovery restores the recorded pre-operation state, which is the same action
/// the interrupted process would have taken for a returned error. Journals that
/// cannot be restored without a decision are reported and left in place.
pub fn recover_pending_operations(state_root: &Path) -> Result<Vec<RecoveryOutcome>, AppError> {
    recover_selected_operations(state_root, false)
}

/// Recover pending journals left behind by other processes.
///
/// A lifecycle transaction calls this while it already holds the coordination
/// lock. Journals from the current process belong to enclosing transactions in
/// this same process and must stay untouched until those transactions finish.
pub(crate) fn recover_interrupted_operations(
    state_root: &Path,
) -> Result<Vec<RecoveryOutcome>, AppError> {
    recover_selected_operations(state_root, true)
}

fn recover_selected_operations(
    state_root: &Path,
    skip_own_process: bool,
) -> Result<Vec<RecoveryOutcome>, AppError> {
    let pending = pending_operations(state_root)?
        .into_iter()
        .filter(|operation| !(skip_own_process && operation.pid == std::process::id()))
        .collect::<Vec<_>>();
    let mut outcomes = Vec::new();

    // A journal restores the database through SQLite, which cannot write to a
    // file it cannot read. Trying anyway would restore the filesystem paths,
    // fail at the database, and strand the journal in `needs-review` with its
    // snapshot unused. Such journals stay untouched, because
    // `doctor --repair-state --apply` quarantines the file first and then
    // restores them into the rebuilt store.
    let database_path = state_root.join(super::DEFAULT_LOCAL_STATE_DATABASE_FILE);
    if !pending.is_empty()
        && let super::schema::DatabaseHealth::Unreadable { reason } =
            super::schema::database_health(&database_path)
    {
        return Ok(pending
            .into_iter()
            .map(|operation| {
                let reason = if operation.state == JournalState::NeedsReview.as_str() {
                    recorded_needs_review_reason(&operation)
                } else {
                    format!(
                        "the state database is not a readable SQLite database: {reason}. Run \
                         'skillctl doctor --repair-state --apply' to recover"
                    )
                };
                RecoveryOutcome::NeedsReview {
                    id: operation.id,
                    reason,
                }
            })
            .collect());
    }

    for operation in pending {
        if operation.state == JournalState::NeedsReview.as_str() {
            let reason = recorded_needs_review_reason(&operation);
            outcomes.push(RecoveryOutcome::NeedsReview {
                id: operation.id,
                reason,
            });
            continue;
        }

        let mut journal = match reopen(&operation.directory) {
            Ok(journal) => journal,
            Err(error) => {
                outcomes.push(RecoveryOutcome::Unreadable {
                    directory: operation.directory.clone(),
                    reason: error.to_string(),
                });
                continue;
            }
        };

        match journal.restore() {
            Ok(()) => {
                let operation = operation.clone();
                journal.finish(JournalState::RolledBack)?;
                outcomes.push(RecoveryOutcome::Recovered(operation));
            }
            Err(error) => {
                journal.mark_needs_review(&error.to_string())?;
                outcomes.push(RecoveryOutcome::NeedsReview {
                    id: operation.id,
                    reason: error.to_string(),
                });
            }
        }
    }

    Ok(outcomes)
}

fn reopen(directory: &Path) -> Result<OperationJournal, AppError> {
    let record = read_journal_record(directory)?;
    Ok(OperationJournal {
        directory: directory.to_path_buf(),
        record,
    })
}

/// Why one journal was left in `needs-review` by an earlier attempt.
fn recorded_needs_review_reason(operation: &PendingOperation) -> String {
    operation
        .steps
        .iter()
        .rev()
        .find_map(|step| step.strip_prefix("needs-review:"))
        .unwrap_or("a previous recovery attempt refused to act")
        .to_string()
}

fn read_journal_record(directory: &Path) -> Result<JournalRecord, AppError> {
    let contents = fs::read_to_string(directory.join(JOURNAL_FILE)).map_err(|source| {
        AppError::FilesystemOperation {
            action: "read lifecycle journal record",
            path: directory.join(JOURNAL_FILE),
            source,
        }
    })?;
    serde_json::from_str(&contents).map_err(|source| AppError::LocalStateValidation {
        path: directory.to_path_buf(),
        message: format!("lifecycle journal record is invalid: {source}"),
    })
}

fn remove_journal_directory(directory: &Path) -> Result<(), AppError> {
    fs::remove_dir_all(directory).map_err(|source| AppError::FilesystemOperation {
        action: "remove completed lifecycle journal",
        path: directory.to_path_buf(),
        source,
    })
}

fn sanitize_component(component: &str) -> String {
    let sanitized: String = component
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '-' || character == '_' {
                character
            } else {
                '_'
            }
        })
        .collect();
    if sanitized.is_empty() {
        "unnamed".to_string()
    } else {
        sanitized
    }
}

/// Capture one path into `backup_path`.
fn snapshot_path(path: &Path, backup_path: &Path) -> Result<CapturedPath, AppError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            return Ok(CapturedPath::Missing);
        }
        Err(source) => {
            return Err(AppError::FilesystemOperation {
                action: "inspect lifecycle journal path",
                path: path.to_path_buf(),
                source,
            });
        }
    };

    if metadata.file_type().is_symlink() {
        let target = fs::read_link(path).map_err(|source| AppError::FilesystemOperation {
            action: "read lifecycle journal symlink",
            path: path.to_path_buf(),
            source,
        })?;
        let is_dir = fs::metadata(path).is_ok_and(|metadata| metadata.is_dir());
        return Ok(CapturedPath::Symlink { target, is_dir });
    }

    if metadata.is_dir() {
        copy_tree(path, backup_path, "stage lifecycle directory backup")?;
        return Ok(CapturedPath::Directory);
    }

    if let Some(parent) = backup_path.parent() {
        fs::create_dir_all(parent).map_err(|source| AppError::FilesystemOperation {
            action: "create lifecycle journal backup parent",
            path: parent.to_path_buf(),
            source,
        })?;
    }
    fs::copy(path, backup_path).map_err(|source| AppError::FilesystemOperation {
        action: "stage lifecycle file backup",
        path: path.to_path_buf(),
        source,
    })?;
    Ok(CapturedPath::File)
}

fn restore_filesystem_path(
    tracked: &TrackedPath,
    journal_directory: &Path,
) -> Result<(), AppError> {
    let path = PathBuf::from(&tracked.path);

    if tracked.kind == "missing" {
        remove_existing_path(&path)?;
        prune_empty_parents(&path);
        return Ok(());
    }

    let Some(backup) = tracked.backup.as_deref() else {
        return Err(AppError::LocalStateValidation {
            path,
            message: "lifecycle journal entry has no staged backup".to_string(),
        });
    };
    let backup_path = journal_directory.join(backup);

    if tracked.kind == "symlink" {
        remove_existing_path(&path)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| AppError::FilesystemOperation {
                action: "create lifecycle restore parent",
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let target = PathBuf::from(tracked.symlink_target.clone().unwrap_or_default());
        create_symlink(&target, &path, tracked.symlink_is_dir.unwrap_or(false))?;
        return Ok(());
    }

    if tracked.kind == "directory" {
        remove_existing_path(&path)?;
        copy_tree(&backup_path, &path, "restore lifecycle directory")?;
        return Ok(());
    }

    remove_existing_path(&path)?;
    let contents = fs::read(&backup_path).map_err(|source| AppError::FilesystemOperation {
        action: "read staged lifecycle file backup",
        path: backup_path.clone(),
        source,
    })?;
    write_file_atomically(&path, &contents)
}

fn restore_database_path(tracked: &TrackedPath, journal_directory: &Path) -> Result<(), AppError> {
    let path = PathBuf::from(&tracked.path);

    if tracked.kind == "missing" {
        remove_existing_path(&path)?;
        for suffix in ["-wal", "-shm", "-journal"] {
            let mut sidecar = path.clone().into_os_string();
            sidecar.push(suffix);
            let _ = remove_existing_path(Path::new(&sidecar));
        }
        prune_empty_parents(&path);
        return Ok(());
    }

    let Some(backup) = tracked.backup.as_deref() else {
        return Err(AppError::LocalStateValidation {
            message: "lifecycle journal entry has no state database snapshot".to_string(),
            path,
        });
    };

    restore_state_database(&path, &journal_directory.join(backup))
}

fn remove_existing_path(path: &Path) -> Result<(), AppError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(AppError::FilesystemOperation {
                action: "inspect lifecycle restore path",
                path: path.to_path_buf(),
                source,
            });
        }
    };

    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        fs::remove_dir_all(path).map_err(|source| AppError::FilesystemOperation {
            action: "remove lifecycle directory before restore",
            path: path.to_path_buf(),
            source,
        })?;
    } else {
        fs::remove_file(path).map_err(|source| AppError::FilesystemOperation {
            action: "remove lifecycle file before restore",
            path: path.to_path_buf(),
            source,
        })?;
    }

    Ok(())
}

fn prune_empty_parents(path: &Path) {
    let mut current = path.parent();
    while let Some(parent) = current {
        match fs::remove_dir(parent) {
            Ok(()) => current = parent.parent(),
            Err(_) => break,
        }
    }
}

fn copy_tree(source: &Path, destination: &Path, action: &'static str) -> Result<(), AppError> {
    let metadata =
        fs::symlink_metadata(source).map_err(|source_error| AppError::FilesystemOperation {
            action,
            path: source.to_path_buf(),
            source: source_error,
        })?;

    if metadata.file_type().is_symlink() {
        let target =
            fs::read_link(source).map_err(|source_error| AppError::FilesystemOperation {
                action: "read lifecycle symlink during copy",
                path: source.to_path_buf(),
                source: source_error,
            })?;
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).map_err(|source_error| AppError::FilesystemOperation {
                action,
                path: parent.to_path_buf(),
                source: source_error,
            })?;
        }
        let is_dir = fs::metadata(source).is_ok_and(|metadata| metadata.is_dir());
        return create_symlink(&target, destination, is_dir);
    }

    if metadata.is_dir() {
        fs::create_dir_all(destination).map_err(|source_error| AppError::FilesystemOperation {
            action,
            path: destination.to_path_buf(),
            source: source_error,
        })?;
        let mut entries = fs::read_dir(source)
            .map_err(|source_error| AppError::FilesystemOperation {
                action,
                path: source.to_path_buf(),
                source: source_error,
            })?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|source_error| AppError::FilesystemOperation {
                action,
                path: source.to_path_buf(),
                source: source_error,
            })?;
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            copy_tree(&entry.path(), &destination.join(entry.file_name()), action)?;
        }
        return Ok(());
    }

    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).map_err(|source_error| AppError::FilesystemOperation {
            action,
            path: parent.to_path_buf(),
            source: source_error,
        })?;
    }
    fs::copy(source, destination).map_err(|source_error| AppError::FilesystemOperation {
        action,
        path: destination.to_path_buf(),
        source: source_error,
    })?;
    Ok(())
}

#[cfg(unix)]
fn create_symlink(target: &Path, destination: &Path, _is_dir: bool) -> Result<(), AppError> {
    std::os::unix::fs::symlink(target, destination).map_err(|source| {
        AppError::FilesystemOperation {
            action: "restore lifecycle symlink",
            path: destination.to_path_buf(),
            source,
        }
    })
}

#[cfg(windows)]
fn create_symlink(target: &Path, destination: &Path, is_dir: bool) -> Result<(), AppError> {
    let create = if is_dir {
        std::os::windows::fs::symlink_dir
    } else {
        std::os::windows::fs::symlink_file
    };
    create(target, destination).map_err(|source| AppError::FilesystemOperation {
        action: "restore lifecycle symlink",
        path: destination.to_path_buf(),
        source,
    })
}

/// Replace one file through a staged write.
///
/// The new bytes are written next to the target, flushed, and renamed over it
/// so readers observe either the old or the new content, never a partial file.
pub fn write_file_atomically(path: &Path, contents: &[u8]) -> Result<(), AppError> {
    let parent = path.parent().ok_or_else(|| AppError::FilesystemOperation {
        action: "stage file replacement",
        path: path.to_path_buf(),
        source: io::Error::other("target path has no parent directory"),
    })?;
    fs::create_dir_all(parent).map_err(|source| AppError::FilesystemOperation {
        action: "create staged file parent",
        path: parent.to_path_buf(),
        source,
    })?;

    let file_name = match path.file_name() {
        Some(name) => name.to_string_lossy().into_owned(),
        None => "file".to_string(),
    };
    let staged = parent.join(format!(
        ".skillctl-stage-{}-{file_name}",
        std::process::id()
    ));

    {
        let mut file =
            fs::File::create(&staged).map_err(|source| AppError::FilesystemOperation {
                action: "create staged file",
                path: staged.clone(),
                source,
            })?;
        file.write_all(contents)
            .and_then(|()| file.sync_all())
            .map_err(|source| AppError::FilesystemOperation {
                action: "write staged file",
                path: staged.clone(),
                source,
            })?;
    }

    fs::rename(&staged, path).map_err(|source| AppError::FilesystemOperation {
        action: "replace file with staged copy",
        path: path.to_path_buf(),
        source,
    })?;

    // Best-effort directory sync so the rename itself survives a crash.
    let _ = fs::File::open(parent).and_then(|directory| directory.sync_all());
    Ok(())
}

/// Copy one database into a fresh snapshot file and read its schema version.
///
/// The copy uses SQLite's online backup API, so it is consistent even if the
/// source is open, and it captures the schema version marker as well as the
/// rows. The caller holds the coordination lock while this runs.
pub(crate) fn snapshot_state_database(source: &Path, destination: &Path) -> Result<u32, AppError> {
    if !source.exists() {
        return Err(AppError::LocalStateValidation {
            path: source.to_path_buf(),
            message: "cannot snapshot a missing state database".to_string(),
        });
    }
    // The backup API reports a corrupt source as a meaningless `not an error`,
    // so the file is classified before it is copied and the failure names the
    // recovery instead.
    if let super::schema::DatabaseHealth::Unreadable { reason } =
        super::schema::database_health(source)
    {
        return Err(AppError::LocalStateDatabaseUnreadable {
            path: source.to_path_buf(),
            reason,
        });
    }
    let _ = fs::remove_file(destination);

    let connection = open_database(source)?;
    connection
        .backup(rusqlite::DatabaseName::Main, destination, None)
        .map_err(|source_error| query_error(source, "back up state database", source_error))?;

    let snapshot = open_database(destination)?;
    let user_version = pragma_user_version(&snapshot, "main", destination)?;
    verify_database(&snapshot, destination)?;

    Ok(user_version)
}

/// Restore one database from a snapshot file.
///
/// The restore uses SQLite's online backup API, which rewrites the target pages
/// inside one exclusive transaction. The database file is never replaced behind
/// SQLite's back, and the caller holds the coordination lock while this runs.
pub(crate) fn restore_state_database(target: &Path, snapshot: &Path) -> Result<(), AppError> {
    if !target.exists() {
        return Err(AppError::LocalStateValidation {
            path: target.to_path_buf(),
            message: "cannot restore a missing state database".to_string(),
        });
    }
    if !snapshot.exists() {
        return Err(AppError::LocalStateValidation {
            path: snapshot.to_path_buf(),
            message: "state database snapshot is missing".to_string(),
        });
    }
    // The backup API answers a corrupt target with a meaningless `not an error`,
    // so an unreadable target is classified before the copy starts and the
    // failure names the recovery instead.
    if let super::schema::DatabaseHealth::Unreadable { reason } =
        super::schema::database_health(target)
    {
        return Err(AppError::LocalStateDatabaseUnreadable {
            path: target.to_path_buf(),
            reason,
        });
    }

    let mut connection = open_database(target)?;
    connection
        .restore(
            rusqlite::DatabaseName::Main,
            snapshot,
            None::<fn(rusqlite::backup::Progress)>,
        )
        .map_err(|source_error| query_error(target, "restore state database", source_error))?;
    verify_database(&connection, target)
}

/// Copy the state database file before a schema migration and prune old copies.
pub(crate) fn record_migration_backup(
    database_path: &Path,
    from: u32,
    to: u32,
) -> Result<(), AppError> {
    let Some(state_root) = database_path.parent().map(migration_backup_root) else {
        return Ok(());
    };
    if !database_path.exists() {
        return Ok(());
    }

    fs::create_dir_all(&state_root).map_err(|source| AppError::FilesystemOperation {
        action: "create migration backup directory",
        path: state_root.clone(),
        source,
    })?;

    let destination = state_root.join(format!(
        "state.db.migrate-v{from}-to-v{to}.{}",
        coarse_timestamp().replace('.', "-")
    ));
    fs::copy(database_path, &destination).map_err(|source| AppError::FilesystemOperation {
        action: "back up state database before migration",
        path: destination.clone(),
        source,
    })?;
    let _ = fs::File::open(&destination).and_then(|file| file.sync_all());

    prune_migration_backups(&state_root)
}

/// Keep only the newest migration backups.
pub(crate) fn prune_migration_backups(backup_root: &Path) -> Result<(), AppError> {
    let mut backups: Vec<PathBuf> = match fs::read_dir(backup_root) {
        Ok(entries) => entries
            .filter_map(std::result::Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.is_file())
            .collect(),
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(AppError::FilesystemOperation {
                action: "list migration backups",
                path: backup_root.to_path_buf(),
                source,
            });
        }
    };

    backups.sort_by_key(|path| {
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        std::cmp::Reverse(name)
    });

    for stale in backups.iter().skip(MIGRATION_BACKUP_RETENTION) {
        fs::remove_file(stale).map_err(|source| AppError::FilesystemOperation {
            action: "prune old migration backup",
            path: stale.to_path_buf(),
            source,
        })?;
    }

    Ok(())
}

/// List retained migration backups for diagnostics.
pub fn migration_backups(state_root: &Path) -> Vec<String> {
    let backup_root = migration_backup_root(state_root);
    let mut backups: Vec<String> = match fs::read_dir(&backup_root) {
        Ok(entries) => entries
            .filter_map(std::result::Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.is_file())
            .map(|path| {
                path.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default()
            })
            .filter(|name| !name.is_empty())
            .collect(),
        Err(_) => Vec::new(),
    };
    backups.sort();
    backups
}

/// Verify one database passes SQLite's consistency check.
pub(crate) fn verify_database(connection: &Connection, path: &Path) -> Result<(), AppError> {
    let check: String = connection
        .query_row("PRAGMA quick_check;", [], |row| row.get(0))
        .map_err(|source_error| query_error(path, "verify database integrity", source_error))?;
    if check != "ok" {
        return Err(AppError::LocalStateValidation {
            path: path.to_path_buf(),
            message: format!("state database failed its integrity check: {check}"),
        });
    }
    Ok(())
}

fn open_database(path: &Path) -> Result<Connection, AppError> {
    let connection = Connection::open(path).map_err(|source| AppError::LocalStateOpen {
        path: path.to_path_buf(),
        source,
    })?;
    super::configure_connection(&connection, path)?;
    Ok(connection)
}

fn pragma_user_version(
    connection: &Connection,
    schema: &str,
    path: &Path,
) -> Result<u32, AppError> {
    let version: i64 = connection
        .query_row(&format!("PRAGMA {schema}.user_version"), [], |row| {
            row.get(0)
        })
        .map_err(|source_error| query_error(path, "read schema version", source_error))?;
    u32::try_from(version).map_err(|_| AppError::LocalStateValidation {
        path: path.to_path_buf(),
        message: format!("schema version must be a non-negative integer, found {version}"),
    })
}

fn query_error(path: &Path, operation: &'static str, source: rusqlite::Error) -> AppError {
    // Delegated so a snapshot read that outlasts the busy timeout reports the
    // wait control instead of the raw SQLite text.
    super::local_state_query(path, operation, source)
}
