//! Coordinated, journaled lifecycle transactions.
//!
//! A lifecycle transaction is the single writer boundary for the shared local
//! state database and the workspace files a command rewrites. While it runs it
//! holds the state coordination lock, records a durable operation journal, and
//! stages a backup of every path it is about to change.
//!
//! Recovery is defined at every journal boundary:
//!
//! * an operation that returns an error restores the recorded pre-state, and
//! * a process that terminates mid-operation leaves an active journal that the
//!   next coordinated operation or `doctor --repair-state` restores.
//!
//! The shared database is never restored by copying a file over it. Rows are
//! restored through SQLite inside one transaction.

use std::{
    env,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use crate::{error::AppError, state};

/// Environment variable that terminates the process at one journal boundary.
///
/// The value has the `operation:label` form used by `SKILLCTL_FAILPOINT`.
const TERMINATE_AT_ENV: &str = "SKILLCTL_TERMINATE_AT";
/// Environment variable holding a transaction until one file exists.
///
/// Integration tests use it to prove a second process cannot mutate shared
/// state while the coordination lock is held.
const HOLD_UNTIL_ENV: &str = "SKILLCTL_LIFECYCLE_HOLD";
/// Environment variable naming the boundary a hold waits at.
///
/// The value has the `operation:label` form. Without it the transaction holds
/// at the first boundary it reaches.
const HOLD_AT_ENV: &str = "SKILLCTL_LIFECYCLE_HOLD_AT";
/// Environment variable bounding how long a held transaction waits.
const HOLD_TIMEOUT_ENV: &str = "SKILLCTL_LIFECYCLE_HOLD_TIMEOUT_MS";
/// Default wait for a held transaction.
const DEFAULT_HOLD_TIMEOUT: Duration = Duration::from_secs(60);
/// Pause between checks of the hold sentinel file.
const HOLD_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Run `work` inside one coordinated, journaled lifecycle transaction.
///
/// When `work` succeeds the journal is marked complete and removed. When it
/// returns an error the recorded pre-state is restored. When the process dies
/// the journal stays on disk as an explicit recoverable operation.
pub(crate) fn run_transaction<T>(
    operation: &'static str,
    work: impl FnOnce(&mut LifecycleTransaction) -> Result<T, AppError>,
) -> Result<T, AppError> {
    let mut transaction = LifecycleTransaction::new(operation)?;
    match work(&mut transaction) {
        Ok(result) => {
            transaction.commit()?;
            Ok(result)
        }
        Err(error) => match transaction.rollback() {
            Ok(()) => Err(error),
            Err(rollback_error) => Err(rollback_error),
        },
    }
}

pub(crate) struct LifecycleTransaction {
    operation: &'static str,
    journal: state::OperationJournal,
    _guard: state::CoordinationGuard,
}

impl LifecycleTransaction {
    fn new(operation: &'static str) -> Result<Self, AppError> {
        let state_database = state::default_state_database_path().ok();
        let guard = state::acquire_coordination_lock(
            state_database.as_deref(),
            "run lifecycle transaction",
        )?;

        // A process that died mid-operation leaves its journal behind. Restore
        // it before this operation records changes of its own. Journals from
        // this process belong to enclosing transactions and stay untouched.
        state::recover_interrupted_operations(&state::default_journal_root())?;

        let working_directory =
            env::current_dir().map_err(|source| AppError::CurrentWorkingDirectory { source })?;
        let workspace_key = state::workspace_key_for_path(&working_directory)?;
        let mut journal = state::OperationJournal::create(
            &state::default_journal_root(),
            &workspace_key,
            &working_directory,
            operation,
        )?;
        journal.record_step("opened")?;
        terminate_at_boundary(operation, "opened");
        hold_at_boundary(operation, "opened")?;

        Ok(Self {
            operation,
            journal,
            _guard: guard,
        })
    }

    /// Stage a backup of one path before the operation changes it.
    pub(crate) fn track_path(&mut self, path: impl Into<PathBuf>) -> Result<(), AppError> {
        self.journal.track_path(&path.into())
    }

    /// Stage a logical snapshot of the shared state database.
    pub(crate) fn track_state_database(&mut self) -> Result<(), AppError> {
        self.journal
            .track_state_database(&state::default_state_database_path()?)
    }

    /// Record one journal boundary.
    ///
    /// The boundary is a journal step, a termination point for interruption
    /// tests, and a failpoint for returned-error rollback tests.
    pub(crate) fn checkpoint(&mut self, label: &'static str) -> Result<(), AppError> {
        self.journal.record_step(label)?;
        terminate_at_boundary(self.operation, label);
        hold_at_boundary(self.operation, label)?;

        let expected = format!("{}:{label}", self.operation);
        if env::var("SKILLCTL_FAILPOINT").ok().as_deref() == Some(expected.as_str()) {
            return Err(AppError::FilesystemOperation {
                action: "execute lifecycle failpoint",
                path: PathBuf::from(expected),
                source: std::io::Error::other("injected lifecycle failure"),
            });
        }

        Ok(())
    }

    /// Mark the operation complete and release the coordination lock.
    fn commit(&mut self) -> Result<(), AppError> {
        self.journal.record_step("committing")?;
        terminate_at_boundary(self.operation, "committing");
        self.journal.finish(state::JournalState::Completed)
    }

    /// Restore the recorded pre-state and release the coordination lock.
    fn rollback(&mut self) -> Result<(), AppError> {
        match self.journal.restore() {
            Ok(()) => self.journal.finish(state::JournalState::RolledBack),
            Err(error) => {
                let _ = self.journal.mark_needs_review(&error.to_string());
                Err(error)
            }
        }
    }
}

fn terminate_at_boundary(operation: &str, label: &str) {
    let Some(expected) = env::var(TERMINATE_AT_ENV).ok() else {
        return;
    };
    if expected.trim() == format!("{operation}:{label}") {
        // Exit without unwinding so destructors cannot clean the journal up,
        // which matches what a terminated process leaves behind.
        std::process::exit(70);
    }
}

/// Hold a transaction at one boundary until the sentinel file appears.
///
/// `SKILLCTL_LIFECYCLE_HOLD_AT` names the boundary in `operation:label` form.
/// Without it the first boundary holds.
fn hold_at_boundary(operation: &str, label: &str) -> Result<(), AppError> {
    let Ok(sentinel) = env::var(HOLD_UNTIL_ENV) else {
        return Ok(());
    };
    if env::var(HOLD_AT_ENV).is_ok_and(|expected| expected.trim() != format!("{operation}:{label}"))
    {
        return Ok(());
    }

    let deadline = Instant::now() + env_timeout(HOLD_TIMEOUT_ENV).unwrap_or(DEFAULT_HOLD_TIMEOUT);
    while !Path::new(&sentinel).exists() {
        if Instant::now() >= deadline {
            return Err(AppError::FilesystemOperation {
                action: "wait for lifecycle hold sentinel",
                path: PathBuf::from(sentinel),
                source: std::io::Error::other("lifecycle hold timed out"),
            });
        }
        std::thread::sleep(HOLD_POLL_INTERVAL);
    }

    Ok(())
}

fn env_timeout(name: &str) -> Option<Duration> {
    env::var(name)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(Duration::from_millis)
}
