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

    let mut registry = registry().lock().expect("coordination registry locks");
    match registry.get_mut(&lock_path) {
        Some(held) => {
            // Another thread of this process registered the lock while this
            // call waited for the file lock. Follow that registration instead.
            held.depth += 1;
            drop(registry);
            let _ = file.unlock();
            Ok(CoordinationGuard {
                lock_path: Some(lock_path),
            })
        }
        None => {
            registry.insert(lock_path.clone(), HeldLock { file, depth: 1 });
            Ok(CoordinationGuard {
                lock_path: Some(lock_path),
            })
        }
    }
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
    std::env::var(LOCK_TIMEOUT_ENV)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map_or(DEFAULT_TIMEOUT, |milliseconds| {
            Duration::from_millis(milliseconds.max(1))
        })
}

fn lock_timeout(lock_path: &Path, purpose: &'static str, started: Instant) -> AppError {
    AppError::LocalStateLockTimeout {
        path: lock_path.to_path_buf(),
        purpose,
        waited_ms: started.elapsed().as_millis() as u64,
        holder: read_holder(lock_path),
    }
}
