//! Overlay and detachment domain entry points.

use std::{
    fs::{self, File},
    io::{self, Read},
    path::{Component, Path, PathBuf},
};

use serde_json::json;
use sha2::{Digest, Sha256};

use crate::{
    app::AppContext,
    cli::Scope,
    error::AppError,
    history::HistoryLedger,
    lockfile::{LockfileTimestamp, WorkspaceLockfile},
    manifest::{ImportDefinition, ManifestPath, ManifestScope, WorkspaceManifest},
    response::AppResponse,
    skill::SKILL_MANIFEST_FILE,
    source::{compute_effective_version_hash, current_timestamp, imports_store_root},
    state::{LocalStateStore, ManagedScope, ManagedSkillRef},
};

/// Default relative path to the overlay root.
pub const DEFAULT_OVERLAYS_DIR: &str = ".agents/overlays";
/// Hash token used when no overlay files are present.
pub(crate) const NO_OVERLAY_HASH: &str = "sha256:none";

/// Placeholder overlay root definition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OverlayRoot {
    /// Filesystem path to the overlay root.
    pub path: PathBuf,
}

impl Default for OverlayRoot {
    fn default() -> Self {
        Self {
            path: PathBuf::from(DEFAULT_OVERLAYS_DIR),
        }
    }
}

/// Typed request for `skillctl override`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OverrideRequest {
    /// Managed skill name.
    pub skill: String,
}

impl OverrideRequest {
    /// Create an override request from parsed CLI arguments.
    pub fn new(skill: String) -> Self {
        Self { skill }
    }
}

/// Typed request for `skillctl fork`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ForkRequest {
    /// Managed skill name.
    pub skill: String,
}

impl ForkRequest {
    /// Create a fork request from parsed CLI arguments.
    pub fn new(skill: String) -> Self {
        Self { skill }
    }
}

/// Handle `skillctl override`.
pub fn handle_override(
    context: &AppContext,
    request: OverrideRequest,
) -> Result<AppResponse, AppError> {
    let mut manifest = WorkspaceManifest::load_from_workspace(&context.working_directory)?;
    let mut lockfile = WorkspaceLockfile::load_from_workspace(&context.working_directory)?;
    let mut store = LocalStateStore::open_default()?;
    let managed_skill = resolve_managed_skill(&store, &request.skill, context.selector.scope)?;
    let scope = manifest_scope(managed_skill.scope);
    let import = managed_import(&manifest, &request.skill, scope)?;
    let lockfile_entry =
        lockfile
            .imports
            .get_mut(&import.id)
            .ok_or_else(|| AppError::ResolutionValidation {
                message: format!(
                    "managed import '{}' is missing from the lockfile",
                    import.id
                ),
            })?;
    let mut install_record =
        store
            .install_record(&managed_skill)?
            .ok_or_else(|| AppError::ResolutionValidation {
                message: format!(
                    "skill '{}' does not have an installed state record",
                    request.skill
                ),
            })?;
    let mut pin_record = store.pin_record(&managed_skill)?;
    let overlay_path = manifest
        .overrides
        .get(&import.id)
        .cloned()
        .unwrap_or_else(|| default_overlay_path(&manifest, &import.id));
    let stored_skill_root = imports_store_root()?
        .join(&import.id)
        .join(lockfile_entry.source.subpath.as_str());
    let source_manifest = stored_skill_root.join(SKILL_MANIFEST_FILE);
    ensure_file(&source_manifest, "inspect stored imported skill manifest")?;

    let overlay_root = context.working_directory.join(overlay_path.as_str());
    let overlay_manifest = overlay_root.join(SKILL_MANIFEST_FILE);
    let overlay_manifest_display = join_relative_path(overlay_path.as_str(), SKILL_MANIFEST_FILE);

    let mut created = Vec::new();
    let mut skipped = Vec::new();
    let overlay_root_created = ensure_directory(&overlay_root, "overlay directory")?;
    record_path_action(
        overlay_root_created,
        overlay_path.as_str(),
        &mut created,
        &mut skipped,
    );
    let overlay_manifest_created =
        copy_if_missing(&source_manifest, &overlay_manifest, "overlay manifest")?;
    record_path_action(
        overlay_manifest_created,
        &overlay_manifest_display,
        &mut created,
        &mut skipped,
    );

    let manifest_updated = match manifest.overrides.get(&import.id) {
        Some(existing) if existing == &overlay_path => false,
        _ => {
            manifest
                .overrides
                .insert(import.id.clone(), overlay_path.clone());
            manifest.write_to_path()?;
            true
        }
    };

    let timestamp = current_timestamp();
    let overlay_hash = hash_overlay_root(&overlay_root)?;
    let effective_version_hash = compute_effective_version_hash(
        &lockfile_entry.revision.resolved,
        &lockfile_entry.hashes.content,
        &overlay_hash,
    );

    lockfile_entry.hashes.overlay = overlay_hash.clone();
    lockfile_entry.hashes.effective_version = effective_version_hash.clone();
    lockfile_entry.timestamps.last_updated_at = LockfileTimestamp::new(timestamp.clone());
    lockfile.write_to_path()?;

    install_record.overlay_hash = overlay_hash.clone();
    install_record.effective_version_hash = effective_version_hash.clone();
    install_record.updated_at = timestamp.clone();
    store.upsert_install_record(&install_record)?;

    if let Some(pin_record) = pin_record.as_mut() {
        pin_record.effective_version_hash = Some(effective_version_hash.clone());
        store.upsert_pin_record(pin_record)?;
    }

    if overlay_root_created || overlay_manifest_created || manifest_updated {
        let mut ledger = HistoryLedger::new(&mut store);
        ledger.record_overlay_created(&managed_skill, overlay_path.as_str(), &timestamp)?;
    }

    let summary = if overlay_root_created || overlay_manifest_created || manifest_updated {
        format!(
            "Created overlay for {} at {}",
            request.skill,
            overlay_path.as_str()
        )
    } else {
        format!(
            "Overlay for {} is ready at {}",
            request.skill,
            overlay_path.as_str()
        )
    };

    Ok(AppResponse::success("override")
        .with_summary(summary)
        .with_data(json!({
            "skill": request.skill,
            "scope": managed_skill.scope.as_str(),
            "overlay_root": overlay_path.as_str(),
            "overlay_hash": overlay_hash,
            "effective_version_hash": effective_version_hash,
            "created": created,
            "skipped": skipped,
            "manifest_updated": manifest_updated,
        })))
}

/// Handle `skillctl fork`.
pub fn handle_fork(_context: &AppContext, _request: ForkRequest) -> Result<AppResponse, AppError> {
    Err(AppError::NotYetImplemented { command: "fork" })
}

fn resolve_managed_skill(
    store: &LocalStateStore,
    skill: &str,
    scope: Option<Scope>,
) -> Result<ManagedSkillRef, AppError> {
    let scopes = match scope {
        Some(Scope::Workspace) => vec![ManagedScope::Workspace],
        Some(Scope::User) => vec![ManagedScope::User],
        None => vec![ManagedScope::Workspace, ManagedScope::User],
    };
    let mut matches = Vec::new();

    for managed_scope in scopes {
        let candidate = ManagedSkillRef::new(managed_scope, skill);
        if store.install_record(&candidate)?.is_some() {
            matches.push(candidate);
        }
    }

    match matches.len() {
        1 => Ok(matches.remove(0)),
        0 => Err(AppError::ResolutionValidation {
            message: format!("skill '{}' is not a managed imported skill", skill),
        }),
        _ => Err(AppError::ResolutionValidation {
            message: format!(
                "skill '{}' exists in multiple scopes; re-run with --scope",
                skill
            ),
        }),
    }
}

fn managed_import<'a>(
    manifest: &'a WorkspaceManifest,
    skill: &str,
    scope: ManifestScope,
) -> Result<&'a ImportDefinition, AppError> {
    manifest
        .imports
        .iter()
        .find(|import| import.id == skill && import.scope == scope)
        .ok_or_else(|| AppError::ResolutionValidation {
            message: format!(
                "skill '{}' is not a managed import in the workspace manifest",
                skill
            ),
        })
}

fn default_overlay_path(manifest: &WorkspaceManifest, skill_id: &str) -> ManifestPath {
    let overlays_root = manifest.layout.overlays_dir.as_str().trim_end_matches('/');
    ManifestPath::new(format!("{overlays_root}/{skill_id}"))
}

fn manifest_scope(scope: ManagedScope) -> ManifestScope {
    match scope {
        ManagedScope::Workspace => ManifestScope::Workspace,
        ManagedScope::User => ManifestScope::User,
    }
}

fn ensure_directory(path: &Path, expected: &'static str) -> Result<bool, AppError> {
    match fs::metadata(path) {
        Ok(metadata) if metadata.is_dir() => Ok(false),
        Ok(_) => Err(AppError::PathConflict {
            path: path.to_path_buf(),
            expected,
        }),
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(path).map_err(|source| AppError::FilesystemOperation {
                action: "create overlay directory",
                path: path.to_path_buf(),
                source,
            })?;
            Ok(true)
        }
        Err(source) => Err(AppError::FilesystemOperation {
            action: "inspect overlay directory",
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn ensure_file(path: &Path, action: &'static str) -> Result<(), AppError> {
    let metadata = fs::metadata(path).map_err(|source| AppError::FilesystemOperation {
        action,
        path: path.to_path_buf(),
        source,
    })?;

    if metadata.is_file() {
        Ok(())
    } else {
        Err(AppError::PathConflict {
            path: path.to_path_buf(),
            expected: "file",
        })
    }
}

fn copy_if_missing(
    source: &Path,
    destination: &Path,
    action: &'static str,
) -> Result<bool, AppError> {
    match fs::metadata(destination) {
        Ok(metadata) if metadata.is_file() => Ok(false),
        Ok(_) => Err(AppError::PathConflict {
            path: destination.to_path_buf(),
            expected: "file",
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent).map_err(|source| AppError::FilesystemOperation {
                    action: "create overlay file parent directory",
                    path: parent.to_path_buf(),
                    source,
                })?;
            }
            fs::copy(source, destination).map_err(|source| AppError::FilesystemOperation {
                action,
                path: destination.to_path_buf(),
                source,
            })?;
            Ok(true)
        }
        Err(source) => Err(AppError::FilesystemOperation {
            action: "inspect overlay file",
            path: destination.to_path_buf(),
            source,
        }),
    }
}

fn record_path_action(
    created_path: bool,
    path: &str,
    created: &mut Vec<String>,
    skipped: &mut Vec<String>,
) {
    if created_path {
        created.push(path.to_string());
    } else {
        skipped.push(path.to_string());
    }
}

fn join_relative_path(root: &str, child: &str) -> String {
    format!("{}/{}", root.trim_end_matches('/'), child)
}

pub(crate) fn hash_overlay_root(root: &Path) -> Result<String, AppError> {
    match fs::metadata(root) {
        Ok(metadata) if !metadata.is_dir() => {
            return Err(AppError::PathConflict {
                path: root.to_path_buf(),
                expected: "directory",
            });
        }
        Ok(_) => {}
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            return Ok(NO_OVERLAY_HASH.to_string());
        }
        Err(source) => {
            return Err(AppError::FilesystemOperation {
                action: "inspect overlay root",
                path: root.to_path_buf(),
                source,
            });
        }
    }

    let mut files = Vec::new();
    collect_overlay_files(root, root, &mut files)?;
    if files.is_empty() {
        return Ok(NO_OVERLAY_HASH.to_string());
    }
    files.sort();

    let mut hasher = Sha256::new();
    for relative_path in files {
        let portable_path = portable_relative_path(&relative_path)?;
        hasher.update(portable_path.as_bytes());
        hasher.update(b"\0");
        hash_file(&root.join(&relative_path), &mut hasher)?;
        hasher.update(b"\0");
    }

    Ok(format!("sha256:{:x}", hasher.finalize()))
}

fn collect_overlay_files(
    root: &Path,
    current: &Path,
    files: &mut Vec<PathBuf>,
) -> Result<(), AppError> {
    let mut entries = fs::read_dir(current)
        .map_err(|source| AppError::FilesystemOperation {
            action: "read overlay directory",
            path: current.to_path_buf(),
            source,
        })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| AppError::FilesystemOperation {
            action: "read overlay directory entry",
            path: current.to_path_buf(),
            source,
        })?;
    entries.sort_by_key(|entry| entry.path());

    for entry in entries {
        let path = entry.path();
        let metadata = entry
            .metadata()
            .map_err(|source| AppError::FilesystemOperation {
                action: "inspect overlay path",
                path: path.clone(),
                source,
            })?;

        if metadata.is_dir() {
            collect_overlay_files(root, &path, files)?;
        } else if metadata.is_file() {
            files.push(
                path.strip_prefix(root)
                    .expect("overlay file remains under the overlay root")
                    .to_path_buf(),
            );
        } else {
            return Err(AppError::ResolutionValidation {
                message: format!(
                    "overlay '{}' contains a non-file, non-directory entry '{}'",
                    root.display(),
                    path.display()
                ),
            });
        }
    }

    Ok(())
}

fn portable_relative_path(path: &Path) -> Result<String, AppError> {
    let mut segments = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(segment) => {
                let segment = segment
                    .to_str()
                    .ok_or_else(|| AppError::ResolutionValidation {
                        message: format!("overlay path '{}' is not valid UTF-8", path.display()),
                    })?;
                segments.push(segment);
            }
            _ => {
                return Err(AppError::ResolutionValidation {
                    message: format!(
                        "overlay path '{}' must remain relative and portable",
                        path.display()
                    ),
                });
            }
        }
    }

    Ok(segments.join("/"))
}

fn hash_file(path: &Path, hasher: &mut Sha256) -> Result<(), AppError> {
    let mut file = File::open(path).map_err(|source| AppError::FilesystemOperation {
        action: "open overlay file for hashing",
        path: path.to_path_buf(),
        source,
    })?;
    let mut buffer = [0_u8; 8192];

    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|source| AppError::FilesystemOperation {
                action: "read overlay file for hashing",
                path: path.to_path_buf(),
                source,
            })?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }

    Ok(())
}
