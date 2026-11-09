//! Cross-process coordination for the shared local state store.
//!
//! Every mutation of the shared SQLite database, the stored import cache, and
//! the workspace files a lifecycle transaction rewrites happens while this lock
//! is held. The lock is advisory, released by the operating system when the
//! owning process dies, and reentrant inside one process so lifecycle code can
//! open stores and write records while it already holds the lock.

use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::Write as _,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

use crate::error::AppError;

/// Suffix appended to a state database path to locate its coordination lock.
const LOCK_FILE_SUFFIX: &str = "lock";
/// Default time a caller waits for the shared state coordination lock.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15);
/// Environment variable that overrides the coordination lock timeout.
const LOCK_TIMEOUT_ENV: &str = "SKILLCTL_LOCK_TIMEOUT_MS";
/// Pause between two attempts to take the lock.
const POLL_INTERVAL: Duration = Duration::from_millis(5);

/// Guard that holds one state coordination lock until it is dropped.
///
/// Dropping the outermost guard releases the lock. Guards for reentrant
/// acquisitions inside the same process do not release anything.
pub(crate) struct CoordinationGuard {
    lock_path: Option<PathBuf>,
}

impl Drop for CoordinationGuard {
    fn drop(&mut self) {
        let Some(lock_path) = self.lock_path.take() else {
            return;
        };

        // Every guard releases one acquisition of this process. The last one
        // releases the file lock itself, which happens when the outermost
        // transaction or store operation finishes.
        let mut registry = registry().lock().expect("coordination registry locks");
        let depth = registry
            .get_mut(&lock_path)
            .map(|held| {
                held.depth = held.depth.saturating_sub(1);
                held.depth
            })
            .unwrap_or_default();
        if depth == 0
            && let Some(held) = registry.remove(&lock_path)
        {
            let _ = held.file.unlock();
        }
    }
}

struct HeldLock {
    file: File,
    depth: u64,
}

fn registry() -> &'static Mutex<BTreeMap<PathBuf, HeldLock>> {
    static REGISTRY: OnceLock<Mutex<BTreeMap<PathBuf, HeldLock>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// Return the coordination lock path that guards one state database.
///
/// Shared and in-memory databases have no lock file, so callers skip
/// coordination for them.
pub(crate) fn lock_path_for(database_path: &Path) -> Option<PathBuf> {
    let name = database_path.file_name()?;
    if name == ":memory:" {
        return None;
    }

    let mut lock_name = name.to_os_string();
    lock_name.push(".");
    lock_name.push(LOCK_FILE_SUFFIX);

    match database_path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => Some(parent.join(lock_name)),
        _ => Some(PathBuf::from(lock_name)),
    }
}

/// Acquire the coordination lock that guards one state database.
///
/// The call blocks until the lock is free or the configured timeout expires.
/// Acquiring the same lock again inside one process returns immediately.
pub(crate) fn acquire(
    database_path: &Path,
    purpose: &'static str,
) -> Result<CoordinationGuard, AppError> {
    let Some(lock_path) = lock_path_for(database_path) else {
        return Ok(CoordinationGuard { lock_path: None });
    };

    if acquire_reentrant(&lock_path) {
        return Ok(CoordinationGuard {
            lock_path: Some(lock_path),
        });
    }

    let file = open_lock_file(&lock_path)?;
    let started = Instant::now();
    let deadline = started + configured_timeout();
    loop {
        match file.try_lock() {
            Ok(()) => break,
            Err(std::fs::TryLockError::WouldBlock) => {
                if Instant::now() >= deadline {
                    return Err(lock_timeout(&lock_path, purpose, started));
                }
                std::thread::sleep(POLL_INTERVAL);
            }
            Err(std::fs::TryLockError::Error(source)) => {
                return Err(AppError::FilesystemOperation {
                    action: "lock local state coordination file",
                    path: lock_path,
                    source,
                });
            }
        }
    }

    record_holder(&file, purpose);

    // A registry entry exists only while its file holds the flock: every
    // entry is inserted right after the inserting call won `try_lock`, and
    // `Drop` removes the entry before it unlocks. Because this call now owns
    // the flock, no other thread of this process can hold a registration for
    // this path, so the insert always creates the process's entry.
    registry()
        .lock()
        .expect("coordination registry locks")
        .insert(lock_path.clone(), HeldLock { file, depth: 1 });
    Ok(CoordinationGuard {
        lock_path: Some(lock_path),
    })
}

/// Register a reentrant acquisition when this process already holds the lock.
fn acquire_reentrant(lock_path: &Path) -> bool {
    let mut registry = registry().lock().expect("coordination registry locks");
    match registry.get_mut(lock_path) {
        Some(held) => {
            held.depth += 1;
            true
        }
        None => false,
    }
}

/// Whether this process already registered one coordination lock.
fn held_in_process(lock_path: &Path) -> bool {
    registry()
        .lock()
        .expect("coordination registry locks")
        .contains_key(lock_path)
}

/// Whether the coordination lock for one state database is currently held.
///
/// The probe tries to take the lock without waiting, so it answers "held"
/// whenever any process — including this one — owns it. Callers that hold no
/// lock themselves use it to tell an operation in progress from an interrupted
/// one: a lifecycle journal exists on disk only while its owner holds this
/// lock, and the operating system releases the lock when that process dies.
/// An unanswerable probe reports "not held", so its caller keeps reporting the
/// journals it found.
pub(crate) fn is_held(database_path: &Path) -> bool {
    let Some(lock_path) = lock_path_for(database_path) else {
        return false;
    };

    if held_in_process(&lock_path) {
        return true;
    }

    let Ok(file) = open_lock_file(&lock_path) else {
        return false;
    };

    let probe = file.try_lock();
    if matches!(probe, Ok(())) {
        let _ = file.unlock();
    }
    probe_reports_held(probe)
}

/// Whether one probe outcome means the lock is held.
///
/// Only `WouldBlock` reports a holder. A successful probe acquired and
/// released the lock itself, and a hard error — an exhausted lock table or an
/// NFS flock failure, the same error [`acquire`] treats as a distinct failure
/// — means the filesystem cannot answer, which this probe reports as "not
/// held" so its caller keeps reporting the journals it found.
fn probe_reports_held(probe: Result<(), std::fs::TryLockError>) -> bool {
    match probe {
        Ok(()) => false,
        Err(std::fs::TryLockError::WouldBlock) => true,
        Err(std::fs::TryLockError::Error(_)) => false,
    }
}

fn open_lock_file(lock_path: &Path) -> Result<File, AppError> {
    if let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent).map_err(|source| AppError::FilesystemOperation {
            action: "create local state coordination parent directory",
            path: parent.to_path_buf(),
            source,
        })?;
    }

    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)
        .map_err(|source| AppError::FilesystemOperation {
            action: "open local state coordination lock",
            path: lock_path.to_path_buf(),
            source,
        })
}

/// Describe the current holder so a timeout is actionable.
fn read_holder(lock_path: &Path) -> String {
    fs::read_to_string(lock_path)
        .map(|contents| contents.trim().to_string())
        .ok()
        .filter(|contents| !contents.is_empty())
        .unwrap_or_else(|| "an unknown process".to_string())
}

fn record_holder(mut file: &File, purpose: &'static str) {
    use std::io::Seek as _;

    let holder = format!(
        "pid={} purpose={} acquired_at={}",
        std::process::id(),
        purpose,
        crate::state::journal::coarse_timestamp()
    );

    let _ = file.set_len(0);
    let _ = file.seek(std::io::SeekFrom::Start(0));
    let _ = file.write_all(holder.as_bytes());
    let _ = file.write_all(b"\n");
    let _ = file.sync_all();
}

fn configured_timeout() -> Duration {
    super::env_millisecond_timeout(
        std::env::var(LOCK_TIMEOUT_ENV).ok().as_deref(),
        DEFAULT_TIMEOUT,
    )
}

fn lock_timeout(lock_path: &Path, purpose: &'static str, started: Instant) -> AppError {
    AppError::LocalStateLockTimeout {
        path: lock_path.to_path_buf(),
        purpose,
        waited_ms: started.elapsed().as_millis() as u64,
        holder: read_holder(lock_path),
    }
}

#[cfg(test)]
mod tests {
    use super::probe_reports_held;
    use std::{fs::TryLockError, io};

    #[test]
    fn only_a_would_block_probe_reports_a_holder() {
        assert!(
            !probe_reports_held(Ok(())),
            "a probe that acquired the lock reports it free"
        );
        assert!(
            probe_reports_held(Err(TryLockError::WouldBlock)),
            "a blocked probe reports the holder"
        );
    }

    #[test]
    fn an_unanswerable_probe_reports_not_held() {
        // ENOLCK (37): the lock table is exhausted, so flock cannot answer.
        // The same class covers an NFS flock failure. An unanswerable probe
        // must keep doctor reporting the journals it found instead of hiding
        // them behind an imaginary holder.
        let unanswerable = TryLockError::Error(io::Error::from_raw_os_error(37));
        assert!(
            !probe_reports_held(Err(unanswerable)),
            "a hard probe error is not a holder"
        );
    }
}
