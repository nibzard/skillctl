use std::{
    collections::BTreeMap,
    env, fs, io,
    path::{Path, PathBuf},
};

use tempfile::TempDir;

use crate::{error::AppError, state};

pub(crate) fn run_transaction<T>(
    operation: &'static str,
    work: impl FnOnce(&mut LifecycleTransaction) -> Result<T, AppError>,
) -> Result<T, AppError> {
    let mut transaction = LifecycleTransaction::new(operation)?;
    match work(&mut transaction) {
        Ok(result) => Ok(result),
        Err(error) => match transaction.rollback() {
            Ok(()) => Err(error),
            Err(rollback_error) => Err(rollback_error),
        },
    }
}

pub(crate) struct LifecycleTransaction {
    operation: &'static str,
    backup_root: TempDir,
    tracked_paths: BTreeMap<PathBuf, SnapshotState>,
}

impl LifecycleTransaction {
    fn new(operation: &'static str) -> Result<Self, AppError> {
        let backup_root = TempDir::new().map_err(|source| AppError::FilesystemOperation {
            action: "create lifecycle backup root",
            path: env::temp_dir(),
            source,
        })?;

        Ok(Self {
            operation,
            backup_root,
            tracked_paths: BTreeMap::new(),
        })
    }

    pub(crate) fn track_path(&mut self, path: impl Into<PathBuf>) -> Result<(), AppError> {
        let path = path.into();
        if self.tracked_paths.contains_key(&path) {
            return Ok(());
        }

        let snapshot = snapshot_path(&path, self.backup_root.path(), self.tracked_paths.len())?;
        self.tracked_paths.insert(path, snapshot);
        Ok(())
    }

    pub(crate) fn track_state_database(&mut self) -> Result<(), AppError> {
        self.track_path(state::default_state_database_path()?)
    }

    pub(crate) fn checkpoint(&self, label: &'static str) -> Result<(), AppError> {
        let expected = format!("{}:{label}", self.operation);
        if env::var("SKILLCTL_FAILPOINT").ok().as_deref() == Some(expected.as_str()) {
            return Err(AppError::FilesystemOperation {
                action: "execute lifecycle failpoint",
                path: PathBuf::from(expected),
                source: io::Error::other("injected lifecycle failure"),
            });
        }

        Ok(())
    }

    fn rollback(&self) -> Result<(), AppError> {
        let mut paths = self.tracked_paths.keys().cloned().collect::<Vec<_>>();
        paths.sort_by_key(|path| std::cmp::Reverse(path.components().count()));

        for path in paths {
            let snapshot = self
                .tracked_paths
                .get(&path)
                .expect("tracked snapshot exists");
            restore_path(&path, snapshot)?;
        }

        Ok(())
    }
}

#[derive(Debug)]
enum SnapshotState {
    Missing,
    File(PathBuf),
    Directory(PathBuf),
    Symlink(PathBuf),
}

fn snapshot_path(path: &Path, backup_root: &Path, index: usize) -> Result<SnapshotState, AppError> {
    let backup_path = backup_root.join(format!("{index:04}"));
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            return Ok(SnapshotState::Missing);
        }
        Err(source) => {
            return Err(AppError::FilesystemOperation {
                action: "inspect lifecycle path",
                path: path.to_path_buf(),
                source,
            });
        }
    };

    if metadata.file_type().is_symlink() {
        return fs::read_link(path)
            .map(SnapshotState::Symlink)
            .map_err(|source| AppError::FilesystemOperation {
                action: "read lifecycle symlink",
                path: path.to_path_buf(),
                source,
            });
    }

    if metadata.is_dir() {
        copy_tree(path, &backup_path, "snapshot lifecycle directory")?;
        return Ok(SnapshotState::Directory(backup_path));
    }

    if let Some(parent) = backup_path.parent() {
        fs::create_dir_all(parent).map_err(|source| AppError::FilesystemOperation {
            action: "create lifecycle file backup parent",
            path: parent.to_path_buf(),
            source,
        })?;
    }
    fs::copy(path, &backup_path).map_err(|source| AppError::FilesystemOperation {
        action: "snapshot lifecycle file",
        path: path.to_path_buf(),
        source,
    })?;
    Ok(SnapshotState::File(backup_path))
}

fn restore_path(path: &Path, snapshot: &SnapshotState) -> Result<(), AppError> {
    remove_existing_path(path)?;

    match snapshot {
        SnapshotState::Missing => Ok(()),
        SnapshotState::File(backup_path) => {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).map_err(|source| AppError::FilesystemOperation {
                    action: "create lifecycle restore parent",
                    path: parent.to_path_buf(),
                    source,
                })?;
            }
            fs::copy(backup_path, path).map_err(|source| AppError::FilesystemOperation {
                action: "restore lifecycle file",
                path: path.to_path_buf(),
                source,
            })?;
            Ok(())
        }
        SnapshotState::Directory(backup_path) => {
            copy_tree(backup_path, path, "restore lifecycle directory")
        }
        SnapshotState::Symlink(target) => {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).map_err(|source| AppError::FilesystemOperation {
                    action: "create lifecycle symlink parent",
                    path: parent.to_path_buf(),
                    source,
                })?;
            }
            create_symlink(target, path)
        }
    }
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
                action: "create lifecycle symlink copy parent",
                path: parent.to_path_buf(),
                source: source_error,
            })?;
        }
        return create_symlink(&target, destination);
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
            action: "create lifecycle file copy parent",
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
fn create_symlink(target: &Path, destination: &Path) -> Result<(), AppError> {
    std::os::unix::fs::symlink(target, destination).map_err(|source| {
        AppError::FilesystemOperation {
            action: "restore lifecycle symlink",
            path: destination.to_path_buf(),
            source,
        }
    })
}

#[cfg(windows)]
fn create_symlink(target: &Path, destination: &Path) -> Result<(), AppError> {
    std::os::windows::fs::symlink_file(target, destination).map_err(|source| {
        AppError::FilesystemOperation {
            action: "restore lifecycle symlink",
            path: destination.to_path_buf(),
            source,
        }
    })
}
