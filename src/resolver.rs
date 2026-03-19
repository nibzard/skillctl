//! Effective-skill resolution, overlay application, and conflict analysis.

use std::{
    collections::BTreeMap,
    fs, io,
    path::{Component, Path, PathBuf},
};

use crate::{
    error::AppError,
    lockfile::{LockfilePath, WorkspaceLockfile},
    manifest::{ImportDefinition, ManifestPath, ManifestScope, WorkspaceManifest},
    skill::{OPENAI_METADATA_FILE, SKILL_MANIFEST_FILE, SkillDefinition, SkillVendorMetadata},
};

/// Typed scope for canonical and imported skills during effective resolution.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum SkillScope {
    /// Workspace-local skill scope.
    Workspace,
    /// User-level skill scope.
    User,
}

impl SkillScope {
    /// Borrow the stable scope identifier.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Workspace => "workspace",
            Self::User => "user",
        }
    }
}

impl From<ManifestScope> for SkillScope {
    fn from(value: ManifestScope) -> Self {
        match value {
            ManifestScope::Workspace => Self::Workspace,
            ManifestScope::User => Self::User,
        }
    }
}

/// Strongly typed immutable identifier for one candidate skill source.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum InternalSkillId {
    /// Canonical local skill discovered from the workspace layout.
    Local {
        /// Scope where the local skill is active.
        scope: SkillScope,
        /// Portable relative path to the skill root.
        relative_path: String,
    },
    /// Imported skill resolved from a manifest import and lockfile entry.
    Imported {
        /// Scope where the import is intended to land.
        scope: SkillScope,
        /// Stable manifest import identifier.
        import_id: String,
        /// Normalized source URL from the lockfile.
        source_url: String,
        /// Portable selected subpath inside the stored source.
        subpath: String,
    },
}

impl InternalSkillId {
    /// Construct a canonical local skill identifier.
    pub fn local(scope: SkillScope, relative_path: impl Into<String>) -> Self {
        Self::Local {
            scope,
            relative_path: relative_path.into(),
        }
    }

    /// Construct an imported skill identifier.
    pub fn imported(
        scope: SkillScope,
        import_id: impl Into<String>,
        source_url: impl Into<String>,
        subpath: impl Into<String>,
    ) -> Self {
        Self::Imported {
            scope,
            import_id: import_id.into(),
            source_url: source_url.into(),
            subpath: subpath.into(),
        }
    }
}

/// Source-class precedence defined by the specification.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum SkillSourceClass {
    /// Canonical local skills from the workspace root.
    CanonicalLocal,
    /// Imported skills with a declared overlay.
    OverriddenImported,
    /// Imported skills without an overlay.
    Imported,
}

/// Physical origin for one effective file.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EffectiveFileOrigin {
    /// File came from the stored base skill source.
    Base,
    /// File came from a shadow-file overlay.
    Overlay,
}

/// One resolved file in the effective-skill view.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EffectiveSkillFile {
    /// Portable relative path under the skill root.
    pub relative_path: PathBuf,
    /// Physical source path providing the file contents.
    pub source_path: PathBuf,
    /// Whether the file came from the base source or an overlay.
    pub origin: EffectiveFileOrigin,
}

/// Overlay details applied to one imported skill.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AppliedOverlay {
    /// Physical overlay directory.
    pub root: PathBuf,
    /// Files replaced by the overlay.
    pub replaced_files: Vec<PathBuf>,
}

/// Provenance metadata for one imported skill candidate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedImport {
    /// Stable manifest import identifier.
    pub id: String,
    /// Requested relative subpath from the manifest.
    pub requested_path: ManifestPath,
    /// Requested revision selector from the manifest.
    pub requested_ref: String,
    /// Requested scope from the manifest.
    pub scope: SkillScope,
    /// Normalized source URL from the lockfile.
    pub source_url: String,
    /// Selected relative subpath from the lockfile.
    pub locked_subpath: LockfilePath,
    /// Locked resolved revision.
    pub resolved_revision: String,
    /// Last observed upstream revision, when available.
    pub upstream_revision: Option<String>,
    /// Locked content hash.
    pub content_hash: String,
    /// Locked overlay hash.
    pub overlay_hash: String,
    /// Locked effective-version hash.
    pub effective_version_hash: String,
}

/// One candidate inside the effective-skill graph.
#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedSkillCandidate {
    /// Stable internal identifier.
    pub internal_id: InternalSkillId,
    /// Optional explicit manifest priority.
    pub manifest_priority: Option<i32>,
    /// Source-class precedence for conflict resolution.
    pub source_class: SkillSourceClass,
    /// Active scope for the candidate.
    pub scope: SkillScope,
    /// Parsed effective skill after overlay application.
    pub skill: SkillDefinition,
    /// Effective file map available for projection.
    pub files: Vec<EffectiveSkillFile>,
    /// Applied overlay details, when present.
    pub overlay: Option<AppliedOverlay>,
    /// Import metadata for imported candidates.
    pub import: Option<ResolvedImport>,
}

/// Stage that decided one effective-skill outcome.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResolutionStage {
    /// An explicit manifest priority selected the winner.
    ManifestPriority,
    /// Source-class precedence selected the winner.
    SourceClass,
}

/// Structured explanation for a resolved winner.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResolutionTrace {
    /// Active manifest priority filter, when one was applied.
    pub manifest_priority: Option<i32>,
    /// Final source-class precedence of the winner.
    pub source_class: SkillSourceClass,
    /// Stage that made the final decision.
    pub decisive_stage: ResolutionStage,
}

/// Unresolved same-name conflict after applying all precedence rules.
#[derive(Clone, Debug, PartialEq)]
pub struct SkillConflict {
    /// Projected name shared by the contenders.
    pub name: String,
    /// Manifest priority filter shared by the contenders, when present.
    pub manifest_priority: Option<i32>,
    /// Final source class shared by the contenders.
    pub source_class: SkillSourceClass,
    /// Stage where the tie remained unresolved.
    pub stage: ResolutionStage,
    /// Deterministic list of tied contenders.
    pub contenders: Vec<ResolvedSkillCandidate>,
}

/// One projected-name slot in the effective-skill graph.
#[derive(Clone, Debug, PartialEq)]
pub struct EffectiveSkillProjection {
    /// Projected runtime-visible skill name.
    pub name: String,
    /// Resolution result for this projected name.
    pub outcome: ProjectionOutcome,
}

impl EffectiveSkillProjection {
    /// Borrow the selected winner when the projected name resolved cleanly.
    pub fn winner(&self) -> Option<&ResolvedSkillCandidate> {
        match &self.outcome {
            ProjectionOutcome::Selected { winner, .. } => Some(winner.as_ref()),
            ProjectionOutcome::Conflict(_) => None,
        }
    }
}

/// Selected or conflicting outcome for one projected name.
#[derive(Clone, Debug, PartialEq)]
pub enum ProjectionOutcome {
    /// One candidate won and the remainder were shadowed.
    Selected {
        /// Effective winner.
        winner: Box<ResolvedSkillCandidate>,
        /// Shadowed candidates in deterministic order.
        shadowed: Vec<ResolvedSkillCandidate>,
        /// Structured explanation for the winner.
        trace: ResolutionTrace,
    },
    /// The projected name remained ambiguous.
    Conflict(SkillConflict),
}

/// Explainable graph of all resolved candidates and projected-name outcomes.
#[derive(Clone, Debug, PartialEq)]
pub struct EffectiveSkillGraph {
    /// Every discovered and materializable candidate.
    pub candidates: Vec<ResolvedSkillCandidate>,
    /// One outcome per projected skill name.
    pub projections: Vec<EffectiveSkillProjection>,
}

impl EffectiveSkillGraph {
    /// Look up the projected-name outcome for one skill.
    pub fn projection_for(&self, name: &str) -> Option<&EffectiveSkillProjection> {
        self.projections
            .iter()
            .find(|projection| projection.name == name)
    }

    /// Return all unresolved conflicts in deterministic projected-name order.
    pub fn conflicts(&self) -> Vec<&SkillConflict> {
        self.projections
            .iter()
            .filter_map(|projection| match &projection.outcome {
                ProjectionOutcome::Selected { .. } => None,
                ProjectionOutcome::Conflict(conflict) => Some(conflict),
            })
            .collect()
    }

    /// Fail when any projected names remain tied after precedence rules.
    pub fn ensure_conflict_free(&self) -> Result<(), AppError> {
        let conflicts = self.conflicts();
        if conflicts.is_empty() {
            Ok(())
        } else {
            Err(AppError::ResolutionValidation {
                message: format!(
                    "same-name conflicts remain for {}",
                    conflicts
                        .iter()
                        .map(|conflict| conflict.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            })
        }
    }
}

/// Filesystem-backed request to build the effective-skill graph for one workspace.
#[derive(Clone, Debug, PartialEq)]
pub struct ResolveWorkspaceRequest {
    /// Root directory of the active workspace.
    pub working_directory: PathBuf,
    /// Root directory containing stored imported source checkouts keyed by import id.
    pub imports_directory: PathBuf,
    /// Parsed manifest driving resolution.
    pub manifest: WorkspaceManifest,
    /// Parsed lockfile containing resolved import pins.
    pub lockfile: WorkspaceLockfile,
    /// Optional explicit manifest priorities keyed by internal skill id.
    pub manifest_priorities: BTreeMap<InternalSkillId, i32>,
}

impl ResolveWorkspaceRequest {
    /// Construct a request with no explicit manifest priorities.
    pub fn new(
        working_directory: impl Into<PathBuf>,
        imports_directory: impl Into<PathBuf>,
        manifest: WorkspaceManifest,
        lockfile: WorkspaceLockfile,
    ) -> Self {
        Self {
            working_directory: working_directory.into(),
            imports_directory: imports_directory.into(),
            manifest,
            lockfile,
            manifest_priorities: BTreeMap::new(),
        }
    }

    /// Attach explicit manifest priorities for deterministic conflict tests.
    pub fn with_manifest_priorities(
        mut self,
        manifest_priorities: BTreeMap<InternalSkillId, i32>,
    ) -> Self {
        self.manifest_priorities = manifest_priorities;
        self
    }
}

/// Build the explainable effective-skill graph for the workspace inputs.
pub fn build_effective_skill_graph(
    request: &ResolveWorkspaceRequest,
) -> Result<EffectiveSkillGraph, AppError> {
    let mut candidates = discover_local_candidates(request)?;
    candidates.extend(discover_import_candidates(request)?);
    candidates.sort_by(compare_candidates);

    let projections = build_projections(&candidates);

    Ok(EffectiveSkillGraph {
        candidates,
        projections,
    })
}

fn discover_local_candidates(
    request: &ResolveWorkspaceRequest,
) -> Result<Vec<ResolvedSkillCandidate>, AppError> {
    let skills_root = request
        .working_directory
        .join(request.manifest.layout.skills_dir.as_str());

    let metadata = match fs::metadata(&skills_root) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(AppError::FilesystemOperation {
                action: "inspect canonical skills root",
                path: skills_root,
                source,
            });
        }
    };

    if !metadata.is_dir() {
        return Err(AppError::PathConflict {
            path: skills_root,
            expected: "directory",
        });
    }

    let mut skill_roots = read_child_directories(&skills_root, "canonical skill root")?;
    skill_roots.sort();

    skill_roots
        .into_iter()
        .map(|root| {
            let skill = SkillDefinition::load_from_dir(&root)?;
            let files = collect_effective_files(&root)?;
            let relative_path =
                portable_relative_path(root.strip_prefix(&request.working_directory).map_err(
                    |_| AppError::ResolutionValidation {
                        message: format!(
                            "canonical skill root '{}' is outside the workspace '{}'",
                            root.display(),
                            request.working_directory.display()
                        ),
                    },
                )?)?;
            let internal_id = InternalSkillId::local(SkillScope::Workspace, relative_path);
            let manifest_priority = request.manifest_priorities.get(&internal_id).copied();

            Ok(ResolvedSkillCandidate {
                internal_id,
                manifest_priority,
                source_class: SkillSourceClass::CanonicalLocal,
                scope: SkillScope::Workspace,
                skill,
                files,
                overlay: None,
                import: None,
            })
        })
        .collect()
}

fn discover_import_candidates(
    request: &ResolveWorkspaceRequest,
) -> Result<Vec<ResolvedSkillCandidate>, AppError> {
    request
        .manifest
        .imports
        .iter()
        .filter(|import| import.enabled)
        .map(|import| load_import_candidate(request, import))
        .collect()
}

fn load_import_candidate(
    request: &ResolveWorkspaceRequest,
    import: &ImportDefinition,
) -> Result<ResolvedSkillCandidate, AppError> {
    let Some(locked_import) = request.lockfile.imports.get(&import.id) else {
        return Err(AppError::ResolutionValidation {
            message: format!(
                "enabled import '{}' is missing from the lockfile",
                import.id
            ),
        });
    };

    let stored_source_root = request.imports_directory.join(&import.id);
    ensure_directory(
        &stored_source_root,
        "inspect stored import root",
        "directory",
    )?;

    let skill_root = stored_source_root.join(locked_import.source.subpath.as_str());
    ensure_directory(&skill_root, "inspect stored imported skill", "directory")?;

    let base_files = collect_file_map(&skill_root, "imported skill")?;
    let (files, overlay) = match request.manifest.overrides.get(&import.id) {
        Some(overlay_path) => {
            let overlay_root = request.working_directory.join(overlay_path.as_str());
            apply_overlay(base_files, &overlay_root)?
        }
        None => (base_files, None),
    };

    let manifest_file = files
        .get(&PathBuf::from(SKILL_MANIFEST_FILE))
        .ok_or_else(|| AppError::ResolutionValidation {
            message: format!(
                "import '{}' does not contain an effective '{}'",
                import.id, SKILL_MANIFEST_FILE
            ),
        })?;
    let manifest_source = fs::read_to_string(&manifest_file.source_path).map_err(|source| {
        AppError::FilesystemOperation {
            action: "read effective skill manifest",
            path: manifest_file.source_path.clone(),
            source,
        }
    })?;

    let vendor_metadata = load_effective_vendor_metadata(&files)?;
    let skill = SkillDefinition::from_source(
        &skill_root,
        manifest_file.source_path.clone(),
        &manifest_source,
        vendor_metadata,
    )?;

    let scope = SkillScope::from(import.scope);
    let internal_id = InternalSkillId::imported(
        scope,
        import.id.clone(),
        locked_import.source.url.clone(),
        locked_import.source.subpath.as_str().to_string(),
    );
    let manifest_priority = request.manifest_priorities.get(&internal_id).copied();

    Ok(ResolvedSkillCandidate {
        internal_id,
        manifest_priority,
        source_class: if overlay.is_some() {
            SkillSourceClass::OverriddenImported
        } else {
            SkillSourceClass::Imported
        },
        scope,
        skill,
        files: files.into_values().collect(),
        overlay,
        import: Some(ResolvedImport {
            id: import.id.clone(),
            requested_path: import.path.clone(),
            requested_ref: import.ref_spec.clone(),
            scope,
            source_url: locked_import.source.url.clone(),
            locked_subpath: locked_import.source.subpath.clone(),
            resolved_revision: locked_import.revision.resolved.clone(),
            upstream_revision: locked_import.revision.upstream.clone(),
            content_hash: locked_import.hashes.content.clone(),
            overlay_hash: locked_import.hashes.overlay.clone(),
            effective_version_hash: locked_import.hashes.effective_version.clone(),
        }),
    })
}

fn build_projections(candidates: &[ResolvedSkillCandidate]) -> Vec<EffectiveSkillProjection> {
    let mut grouped = BTreeMap::<String, Vec<ResolvedSkillCandidate>>::new();
    for candidate in candidates {
        grouped
            .entry(candidate.skill.name.as_str().to_string())
            .or_default()
            .push(candidate.clone());
    }

    grouped
        .into_iter()
        .map(|(name, mut contenders)| {
            contenders.sort_by(compare_candidates);
            resolve_projection(name, contenders)
        })
        .collect()
}

fn resolve_projection(
    name: String,
    contenders: Vec<ResolvedSkillCandidate>,
) -> EffectiveSkillProjection {
    let mut filtered = contenders.clone();
    let manifest_priority = filtered
        .iter()
        .filter_map(|candidate| candidate.manifest_priority)
        .min();

    if let Some(best_priority) = manifest_priority {
        filtered.retain(|candidate| candidate.manifest_priority == Some(best_priority));
        if let [winner] = filtered.as_slice() {
            let winner = winner.clone();
            let mut shadowed: Vec<_> = contenders
                .into_iter()
                .filter(|candidate| candidate.internal_id != winner.internal_id)
                .collect();
            shadowed.sort_by(compare_candidates);

            return EffectiveSkillProjection {
                name,
                outcome: ProjectionOutcome::Selected {
                    trace: ResolutionTrace {
                        manifest_priority: Some(best_priority),
                        source_class: winner.source_class,
                        decisive_stage: ResolutionStage::ManifestPriority,
                    },
                    winner: Box::new(winner),
                    shadowed,
                },
            };
        }
    }

    let best_source_class = filtered
        .iter()
        .map(|candidate| candidate.source_class)
        .min()
        .unwrap_or(SkillSourceClass::CanonicalLocal);
    filtered.retain(|candidate| candidate.source_class == best_source_class);

    if let [winner] = filtered.as_slice() {
        let winner = winner.clone();
        let mut shadowed: Vec<_> = contenders
            .into_iter()
            .filter(|candidate| candidate.internal_id != winner.internal_id)
            .collect();
        shadowed.sort_by(compare_candidates);

        EffectiveSkillProjection {
            name,
            outcome: ProjectionOutcome::Selected {
                trace: ResolutionTrace {
                    manifest_priority,
                    source_class: winner.source_class,
                    decisive_stage: ResolutionStage::SourceClass,
                },
                winner: Box::new(winner),
                shadowed,
            },
        }
    } else {
        filtered.sort_by(compare_candidates);
        EffectiveSkillProjection {
            name: name.clone(),
            outcome: ProjectionOutcome::Conflict(SkillConflict {
                name,
                manifest_priority,
                source_class: best_source_class,
                stage: ResolutionStage::SourceClass,
                contenders: filtered,
            }),
        }
    }
}

fn collect_effective_files(root: &Path) -> Result<Vec<EffectiveSkillFile>, AppError> {
    Ok(collect_file_map(root, "skill")?.into_values().collect())
}

fn collect_file_map(
    root: &Path,
    kind: &'static str,
) -> Result<BTreeMap<PathBuf, EffectiveSkillFile>, AppError> {
    ensure_directory(root, "inspect skill directory", "directory")?;
    let mut files = BTreeMap::new();
    collect_directory_files(root, root, kind, &mut files)?;
    Ok(files)
}

fn collect_directory_files(
    root: &Path,
    current: &Path,
    kind: &'static str,
    files: &mut BTreeMap<PathBuf, EffectiveSkillFile>,
) -> Result<(), AppError> {
    let mut entries = fs::read_dir(current)
        .map_err(|source| AppError::FilesystemOperation {
            action: "read skill directory",
            path: current.to_path_buf(),
            source,
        })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| AppError::FilesystemOperation {
            action: "read skill directory entry",
            path: current.to_path_buf(),
            source,
        })?;
    entries.sort_by_key(std::fs::DirEntry::path);

    for entry in entries {
        let path = entry.path();
        let metadata = entry
            .metadata()
            .map_err(|source| AppError::FilesystemOperation {
                action: "inspect skill path",
                path: path.clone(),
                source,
            })?;

        if metadata.is_dir() {
            collect_directory_files(root, &path, kind, files)?;
        } else if metadata.is_file() {
            let relative_path =
                normalize_relative_filesystem_path(path.strip_prefix(root).map_err(|_| {
                    AppError::ResolutionValidation {
                        message: format!(
                            "{kind} path '{}' escaped the root '{}'",
                            path.display(),
                            root.display()
                        ),
                    }
                })?)?;
            files.insert(
                relative_path.clone(),
                EffectiveSkillFile {
                    relative_path,
                    source_path: path,
                    origin: EffectiveFileOrigin::Base,
                },
            );
        } else {
            return Err(AppError::ResolutionValidation {
                message: format!(
                    "{kind} directory '{}' contains a non-file, non-directory entry '{}'",
                    root.display(),
                    path.display()
                ),
            });
        }
    }

    Ok(())
}

fn apply_overlay(
    mut base_files: BTreeMap<PathBuf, EffectiveSkillFile>,
    overlay_root: &Path,
) -> Result<
    (
        BTreeMap<PathBuf, EffectiveSkillFile>,
        Option<AppliedOverlay>,
    ),
    AppError,
> {
    ensure_directory(overlay_root, "inspect overlay root", "directory")?;
    let overlay_files = collect_file_map(overlay_root, "overlay")?;
    if overlay_files.is_empty() {
        return Ok((base_files, None));
    }

    let mut replaced_files = Vec::new();
    for (relative_path, overlay_file) in overlay_files {
        let Some(base_file) = base_files.get_mut(&relative_path) else {
            return Err(AppError::ResolutionValidation {
                message: format!(
                    "overlay '{}' file '{}' does not map to an imported file",
                    overlay_root.display(),
                    relative_path.display()
                ),
            });
        };

        base_file.origin = EffectiveFileOrigin::Overlay;
        base_file.source_path = overlay_file.source_path;
        replaced_files.push(relative_path);
    }
    replaced_files.sort();

    Ok((
        base_files,
        Some(AppliedOverlay {
            root: overlay_root.to_path_buf(),
            replaced_files,
        }),
    ))
}

fn load_effective_vendor_metadata(
    files: &BTreeMap<PathBuf, EffectiveSkillFile>,
) -> Result<SkillVendorMetadata, AppError> {
    let mut vendor_files = BTreeMap::new();
    let relative_path = PathBuf::from(OPENAI_METADATA_FILE);

    if let Some(file) = files.get(&relative_path) {
        let contents = fs::read_to_string(&file.source_path).map_err(|source| {
            AppError::FilesystemOperation {
                action: "read effective vendor metadata file",
                path: file.source_path.clone(),
                source,
            }
        })?;
        vendor_files.insert(relative_path, contents);
    }

    Ok(SkillVendorMetadata {
        files: vendor_files,
    })
}

fn read_child_directories(root: &Path, kind: &'static str) -> Result<Vec<PathBuf>, AppError> {
    let mut directories = Vec::new();
    for entry in fs::read_dir(root).map_err(|source| AppError::FilesystemOperation {
        action: "read skill directory",
        path: root.to_path_buf(),
        source,
    })? {
        let entry = entry.map_err(|source| AppError::FilesystemOperation {
            action: "read skill directory entry",
            path: root.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        let metadata = entry
            .metadata()
            .map_err(|source| AppError::FilesystemOperation {
                action: "inspect skill path",
                path: path.clone(),
                source,
            })?;
        if metadata.is_dir() {
            directories.push(path);
        } else {
            return Err(AppError::ResolutionValidation {
                message: format!(
                    "{kind} '{}' contains a non-directory entry '{}'",
                    root.display(),
                    path.display()
                ),
            });
        }
    }

    Ok(directories)
}

fn ensure_directory(
    path: &Path,
    action: &'static str,
    expected: &'static str,
) -> Result<(), AppError> {
    let metadata = fs::metadata(path).map_err(|source| AppError::FilesystemOperation {
        action,
        path: path.to_path_buf(),
        source,
    })?;
    if !metadata.is_dir() {
        return Err(AppError::PathConflict {
            path: path.to_path_buf(),
            expected,
        });
    }
    Ok(())
}

fn compare_candidates(
    left: &ResolvedSkillCandidate,
    right: &ResolvedSkillCandidate,
) -> std::cmp::Ordering {
    priority_key(left)
        .cmp(&priority_key(right))
        .then_with(|| left.source_class.cmp(&right.source_class))
        .then_with(|| left.internal_id.cmp(&right.internal_id))
}

fn priority_key(candidate: &ResolvedSkillCandidate) -> (bool, i32) {
    match candidate.manifest_priority {
        Some(priority) => (false, priority),
        None => (true, i32::MAX),
    }
}

fn normalize_relative_filesystem_path(path: &Path) -> Result<PathBuf, AppError> {
    let mut normalized = PathBuf::new();

    for component in path.components() {
        match component {
            Component::Normal(segment) => normalized.push(segment),
            Component::CurDir
            | Component::ParentDir
            | Component::RootDir
            | Component::Prefix(_) => {
                return Err(AppError::ResolutionValidation {
                    message: format!(
                        "relative filesystem path '{}' must not contain '.', '..', or absolute segments",
                        path.display()
                    ),
                });
            }
        }
    }

    if normalized.as_os_str().is_empty() {
        return Err(AppError::ResolutionValidation {
            message: "relative filesystem path must not be empty".to_string(),
        });
    }

    Ok(normalized)
}

fn portable_relative_path(path: &Path) -> Result<String, AppError> {
    Ok(normalize_relative_filesystem_path(path)?
        .components()
        .map(|component| match component {
            Component::Normal(segment) => segment.to_string_lossy().into_owned(),
            _ => unreachable!("path has already been normalized"),
        })
        .collect::<Vec<_>>()
        .join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    use crate::{
        lockfile::{
            LockedHashes, LockedImport, LockedRevision, LockedSource, LockedTimestamps,
            LockfileTimestamp,
        },
        manifest::ImportSourceType,
        source::SourceKind,
    };

    #[test]
    fn prefers_canonical_local_skills_over_plain_imports() {
        let fixture = ResolverFixture::new();
        fixture.write_skill(
            ".agents/skills/release-notes",
            "release-notes",
            "Prefer the canonical local skill.",
        );
        fixture.write_skill(
            "imports/notes-import/skills/release-notes",
            "release-notes",
            "Imported skill should be shadowed by local.",
        );

        let request = fixture.request(
            vec![import_definition("notes-import", "skills/release-notes")],
            vec![locked_import(
                "notes-import",
                "https://example.com/release.git",
                "skills/release-notes",
            )],
        );

        let graph =
            build_effective_skill_graph(&request).expect("effective skill graph should build");
        graph
            .ensure_conflict_free()
            .expect("graph should be conflict free");

        let projection = graph
            .projection_for("release-notes")
            .expect("projection exists");
        let winner = projection.winner().expect("winner exists");

        assert_eq!(winner.source_class, SkillSourceClass::CanonicalLocal);
        assert_eq!(
            winner.skill.description,
            "Prefer the canonical local skill."
        );
    }

    #[test]
    fn applies_overlays_and_treats_overridden_imports_as_higher_precedence() {
        let fixture = ResolverFixture::new();
        fixture.write_skill(
            "imports/plain-import/skills/release-notes",
            "release-notes",
            "Plain imported skill.",
        );
        fixture.write_skill(
            "imports/overridden-import/skills/release-notes",
            "release-notes",
            "Base imported skill.",
        );
        fixture.write_skill(
            ".agents/overlays/overridden-import",
            "release-notes",
            "Overlay should replace the imported manifest.",
        );

        let mut overrides = BTreeMap::new();
        overrides.insert(
            "overridden-import".to_string(),
            ManifestPath::new(".agents/overlays/overridden-import"),
        );

        let request = fixture.request_with_overrides(
            vec![
                import_definition("plain-import", "skills/release-notes"),
                import_definition("overridden-import", "skills/release-notes"),
            ],
            vec![
                locked_import(
                    "plain-import",
                    "https://example.com/plain.git",
                    "skills/release-notes",
                ),
                locked_import(
                    "overridden-import",
                    "https://example.com/overridden.git",
                    "skills/release-notes",
                ),
            ],
            overrides,
        );

        let graph =
            build_effective_skill_graph(&request).expect("effective skill graph should build");
        graph
            .ensure_conflict_free()
            .expect("graph should be conflict free");

        let projection = graph
            .projection_for("release-notes")
            .expect("projection exists");
        let winner = projection.winner().expect("winner exists");

        assert_eq!(winner.source_class, SkillSourceClass::OverriddenImported);
        assert_eq!(
            winner.skill.description,
            "Overlay should replace the imported manifest."
        );
        let overlay = winner.overlay.as_ref().expect("overlay recorded");
        assert_eq!(
            overlay.root,
            fixture.path(".agents/overlays/overridden-import")
        );
        assert_eq!(overlay.replaced_files, vec![PathBuf::from("SKILL.md")]);

        let skill_file = winner
            .files
            .iter()
            .find(|file| file.relative_path == Path::new("SKILL.md"))
            .expect("effective manifest tracked");
        assert_eq!(skill_file.origin, EffectiveFileOrigin::Overlay);
        assert_eq!(
            skill_file.source_path,
            fixture.path(".agents/overlays/overridden-import/SKILL.md")
        );
    }

    #[test]
    fn reports_unresolved_same_name_ties_as_conflicts() {
        let fixture = ResolverFixture::new();
        fixture.write_skill(
            "imports/first-import/skills/release-notes",
            "release-notes",
            "First imported skill.",
        );
        fixture.write_skill(
            "imports/second-import/skills/release-notes",
            "release-notes",
            "Second imported skill.",
        );

        let request = fixture.request(
            vec![
                import_definition("first-import", "skills/release-notes"),
                import_definition("second-import", "skills/release-notes"),
            ],
            vec![
                locked_import(
                    "first-import",
                    "https://example.com/first.git",
                    "skills/release-notes",
                ),
                locked_import(
                    "second-import",
                    "https://example.com/second.git",
                    "skills/release-notes",
                ),
            ],
        );

        let graph =
            build_effective_skill_graph(&request).expect("effective skill graph should build");

        let conflict = graph
            .projection_for("release-notes")
            .expect("projection exists");
        let ProjectionOutcome::Conflict(conflict) = &conflict.outcome else {
            panic!("same-name imported skills should remain in conflict");
        };

        assert_eq!(conflict.name, "release-notes");
        assert_eq!(conflict.stage, ResolutionStage::SourceClass);
        assert_eq!(conflict.source_class, SkillSourceClass::Imported);
        assert_eq!(conflict.contenders.len(), 2);
        assert!(
            graph.ensure_conflict_free().is_err(),
            "graph should fail validation while conflicts remain"
        );
    }

    #[test]
    fn applies_explicit_manifest_priority_before_source_class_precedence() {
        let fixture = ResolverFixture::new();
        fixture.write_skill(
            ".agents/skills/release-notes",
            "release-notes",
            "Local skill should lose once an explicit priority exists.",
        );
        fixture.write_skill(
            "imports/prioritized-import/skills/release-notes",
            "release-notes",
            "Imported skill wins due to explicit priority.",
        );

        let request = fixture
            .request(
                vec![import_definition(
                    "prioritized-import",
                    "skills/release-notes",
                )],
                vec![locked_import(
                    "prioritized-import",
                    "https://example.com/prioritized.git",
                    "skills/release-notes",
                )],
            )
            .with_manifest_priorities(BTreeMap::from([(
                InternalSkillId::imported(
                    SkillScope::Workspace,
                    "prioritized-import",
                    "https://example.com/prioritized.git",
                    "skills/release-notes",
                ),
                1,
            )]));

        let graph =
            build_effective_skill_graph(&request).expect("effective skill graph should build");
        graph
            .ensure_conflict_free()
            .expect("graph should be conflict free");

        let projection = graph
            .projection_for("release-notes")
            .expect("projection exists");
        let winner = projection.winner().expect("winner exists");

        assert_eq!(winner.source_class, SkillSourceClass::Imported);
        assert_eq!(
            winner.skill.description,
            "Imported skill wins due to explicit priority."
        );
    }

    #[test]
    fn rejects_overlay_files_without_matching_imported_paths() {
        let fixture = ResolverFixture::new();
        fixture.write_skill(
            "imports/overridden-import/skills/release-notes",
            "release-notes",
            "Base imported skill.",
        );
        fixture.write_skill(
            ".agents/overlays/overridden-import",
            "release-notes",
            "Overlay manifest replaces the base manifest.",
        );
        fixture.write_text(
            ".agents/overlays/overridden-import/extra.md",
            "This file does not exist in the imported skill.\n",
        );

        let mut overrides = BTreeMap::new();
        overrides.insert(
            "overridden-import".to_string(),
            ManifestPath::new(".agents/overlays/overridden-import"),
        );

        let request = fixture.request_with_overrides(
            vec![import_definition(
                "overridden-import",
                "skills/release-notes",
            )],
            vec![locked_import(
                "overridden-import",
                "https://example.com/overridden.git",
                "skills/release-notes",
            )],
            overrides,
        );

        let error = build_effective_skill_graph(&request)
            .expect_err("overlay paths without a base mapping should fail validation");

        assert!(
            error.to_string().contains("overlay"),
            "unexpected error: {error}"
        );
    }

    fn import_definition(id: &str, path: &str) -> ImportDefinition {
        ImportDefinition {
            id: id.to_string(),
            kind: ImportSourceType::Git,
            url: format!("https://example.com/{id}.git"),
            ref_spec: "main".to_string(),
            path: ManifestPath::new(path),
            scope: ManifestScope::Workspace,
            enabled: true,
        }
    }

    fn locked_import(id: &str, url: &str, subpath: &str) -> (String, LockedImport) {
        (
            id.to_string(),
            LockedImport {
                source: LockedSource {
                    kind: SourceKind::Git,
                    url: url.to_string(),
                    subpath: LockfilePath::new(subpath),
                },
                revision: LockedRevision {
                    resolved: "0123456789abcdef0123456789abcdef01234567".to_string(),
                    upstream: Some("0123456789abcdef0123456789abcdef01234567".to_string()),
                },
                timestamps: LockedTimestamps {
                    fetched_at: LockfileTimestamp::new("2026-03-19T00:00:00Z"),
                    first_installed_at: LockfileTimestamp::new("2026-03-19T00:00:00Z"),
                    last_updated_at: LockfileTimestamp::new("2026-03-19T00:00:00Z"),
                },
                hashes: LockedHashes {
                    content: format!("sha256:{id}:content"),
                    overlay: format!("sha256:{id}:overlay"),
                    effective_version: format!("sha256:{id}:effective"),
                },
            },
        )
    }

    struct ResolverFixture {
        root: tempfile::TempDir,
    }

    impl ResolverFixture {
        fn new() -> Self {
            let root = tempfile::Builder::new()
                .prefix("skillctl-resolver-")
                .tempdir()
                .expect("tempdir created");
            Self { root }
        }

        fn path(&self, relative: impl AsRef<Path>) -> PathBuf {
            self.root.path().join(relative)
        }

        fn write_skill(&self, relative_root: &str, name: &str, description: &str) {
            self.write_text(
                &format!("{relative_root}/SKILL.md"),
                &format!("---\nname: {name}\ndescription: {description}\n---\n\n# {name}\n",),
            );
        }

        fn write_text(&self, relative_path: &str, contents: &str) {
            let path = self.path(relative_path);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("parent directory exists");
            }
            fs::write(path, contents).expect("fixture file written");
        }

        fn request(
            &self,
            imports: Vec<ImportDefinition>,
            locked_imports: Vec<(String, crate::lockfile::LockedImport)>,
        ) -> ResolveWorkspaceRequest {
            self.request_with_overrides(imports, locked_imports, BTreeMap::new())
        }

        fn request_with_overrides(
            &self,
            imports: Vec<ImportDefinition>,
            locked_imports: Vec<(String, crate::lockfile::LockedImport)>,
            overrides: BTreeMap<String, ManifestPath>,
        ) -> ResolveWorkspaceRequest {
            let mut manifest = WorkspaceManifest::default_at(self.path(".agents/skillctl.yaml"));
            manifest.imports = imports;
            manifest.overrides = overrides;

            let mut lockfile = WorkspaceLockfile::default_at(self.path(".agents/skillctl.lock"));
            lockfile.imports = locked_imports.into_iter().collect();

            ResolveWorkspaceRequest::new(self.root.path(), self.path("imports"), manifest, lockfile)
        }
    }
}
