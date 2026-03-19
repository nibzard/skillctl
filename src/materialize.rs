//! Projection materialization and cleanup domain entry points.

use std::{
    collections::BTreeSet,
    env, fs, io,
    path::{Component, Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::Serialize;
use serde_json::json;

use crate::{
    adapter::{AdapterRegistry, TargetRuntime, TargetScope},
    app::AppContext,
    cli::Scope,
    error::AppError,
    lockfile::WorkspaceLockfile,
    manifest::{ProjectionMode, WorkspaceManifest},
    planner::{self, ProjectionPlan},
    resolver::{self, InternalSkillId, ProjectionOutcome, ResolvedSkillCandidate, SkillScope},
    response::AppResponse,
};

/// Metadata file written into generated projection directories.
pub const PROJECTION_METADATA_FILE: &str = ".skillctl-projection.json";

const IMPORTS_STORE_RELATIVE_PATH: &str = ".skillctl/store/imports";
const UNUSED_IMPORTS_PLACEHOLDER_DIR: &str = ".agents/.skillctl-unused-imports";

/// Structured report for one completed `sync` operation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct MaterializationReport {
    /// Active projection-root plan for the sync run.
    pub plan: ProjectionPlan,
    /// Projection mode requested by the manifest.
    pub mode: ProjectionMode,
    /// Physical roots reused as canonical sources instead of generated copies.
    pub canonical_roots: Vec<String>,
    /// Generated roots that received copied projections.
    pub generated_roots: Vec<GeneratedRootReport>,
    /// Total number of generated skill directories materialized across all roots.
    pub materialized_skills: usize,
    /// Total number of stale skillctl-managed directories pruned across all roots.
    pub pruned_skills: usize,
}

/// Per-root output for generated projections.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct GeneratedRootReport {
    /// Root path exactly as selected by the planner.
    pub path: String,
    /// Skill names materialized into this root.
    pub materialized: Vec<String>,
    /// Stale skill names pruned from this root.
    pub pruned: Vec<String>,
}

/// Typed request for `skillctl sync`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SyncRequest;

/// Typed request for `skillctl clean`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CleanRequest;

/// Compute the materialization report for the current workspace inputs.
pub fn sync_workspace(context: &AppContext) -> Result<MaterializationReport, AppError> {
    let manifest = WorkspaceManifest::load_from_workspace(&context.working_directory)?;
    if manifest.projection.mode != ProjectionMode::Copy {
        return Err(AppError::MaterializationValidation {
            message: format!(
                "projection mode '{}' is not implemented yet; use 'copy'",
                projection_mode_label(manifest.projection.mode)
            ),
        });
    }

    let lockfile = load_lockfile_or_default(&context.working_directory)?;
    let scope = sync_scope(context);
    let targets = selected_targets(context, &manifest)?;
    let registry = AdapterRegistry::new();
    let plan = planner::plan_target_roots(
        &registry,
        scope,
        manifest.projection.policy,
        &targets,
        &manifest.adapters,
    )?;
    let imports_directory = resolve_imports_directory(context, &manifest)?;
    let request = resolver::ResolveWorkspaceRequest::new(
        &context.working_directory,
        imports_directory,
        manifest.clone(),
        lockfile,
    );
    let graph = resolver::build_effective_skill_graph(&request)?;

    ensure_scope_conflict_free(&graph, scope)?;

    let winners: Vec<_> = graph
        .projections
        .iter()
        .filter_map(|projection| match &projection.outcome {
            ProjectionOutcome::Selected { winner, .. } if winner.scope == skill_scope(scope) => {
                Some(winner.as_ref())
            }
            _ => None,
        })
        .collect();

    let canonical_root = canonical_root_path(scope, context, &manifest)?;
    let mut canonical_roots = Vec::new();
    let mut generated_roots = Vec::new();
    let mut materialized_skills = 0usize;
    let mut pruned_skills = 0usize;

    for root in &plan.physical_roots {
        let resolved_root = normalize_path(&resolve_runtime_root_path(context, &root.path)?);
        let root_winners: Vec<_> = if canonical_root.as_ref() == Some(&resolved_root) {
            winners
                .iter()
                .copied()
                .filter(|winner| winner.import.is_some())
                .collect()
        } else {
            winners.clone()
        };

        if root_winners.is_empty() {
            if canonical_root.as_ref() == Some(&resolved_root) {
                canonical_roots.push(root.path.clone());
                continue;
            }
        }

        let report = materialize_generated_root(
            &resolved_root,
            &root.path,
            &root_winners,
            manifest.projection.prune,
        )?;
        materialized_skills += report.materialized.len();
        pruned_skills += report.pruned.len();
        generated_roots.push(report);
    }

    Ok(MaterializationReport {
        plan,
        mode: manifest.projection.mode,
        canonical_roots,
        generated_roots,
        materialized_skills,
        pruned_skills,
    })
}

/// Handle `skillctl sync`.
pub fn handle_sync(context: &AppContext, _request: SyncRequest) -> Result<AppResponse, AppError> {
    let report = sync_workspace(context)?;
    let summary = sync_summary(&report);

    Ok(AppResponse::success("sync")
        .with_summary(summary)
        .with_data(serde_json::to_value(report)?))
}

/// Handle `skillctl clean`.
pub fn handle_clean(
    _context: &AppContext,
    _request: CleanRequest,
) -> Result<AppResponse, AppError> {
    Err(AppError::NotYetImplemented { command: "clean" })
}

fn load_lockfile_or_default(working_directory: &Path) -> Result<WorkspaceLockfile, AppError> {
    match WorkspaceLockfile::load_from_workspace(working_directory) {
        Ok(lockfile) => Ok(lockfile),
        Err(AppError::FilesystemOperation {
            action: "read lockfile",
            path,
            source,
        }) if source.kind() == io::ErrorKind::NotFound => Ok(WorkspaceLockfile::default_at(path)),
        Err(error) => Err(error),
    }
}

fn selected_targets(
    context: &AppContext,
    manifest: &WorkspaceManifest,
) -> Result<Vec<TargetRuntime>, AppError> {
    if context.selector.targets.is_empty() {
        if manifest.targets.is_empty() {
            return Err(AppError::PlannerValidation {
                message: "sync requires at least one enabled target".into(),
            });
        }
        return Ok(manifest.targets.clone());
    }

    let enabled: BTreeSet<_> = manifest.targets.iter().copied().collect();
    context
        .selector
        .targets
        .iter()
        .map(|target| {
            let target = parse_target_runtime(target)?;
            if enabled.contains(&target) {
                Ok(target)
            } else {
                Err(AppError::PlannerValidation {
                    message: format!(
                        "target '{}' is not enabled in the manifest",
                        target.as_str()
                    ),
                })
            }
        })
        .collect()
}

fn parse_target_runtime(value: &str) -> Result<TargetRuntime, AppError> {
    TargetRuntime::all()
        .iter()
        .copied()
        .find(|target| target.as_str() == value)
        .ok_or_else(|| AppError::PlannerValidation {
            message: format!(
                "unknown target '{}'; expected one of {}",
                value,
                TargetRuntime::all()
                    .iter()
                    .map(|target| target.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        })
}

fn sync_scope(context: &AppContext) -> TargetScope {
    match context.selector.scope.unwrap_or(Scope::Workspace) {
        Scope::Workspace => TargetScope::Workspace,
        Scope::User => TargetScope::User,
    }
}

fn skill_scope(scope: TargetScope) -> SkillScope {
    match scope {
        TargetScope::Workspace => SkillScope::Workspace,
        TargetScope::User => SkillScope::User,
    }
}

fn ensure_scope_conflict_free(
    graph: &resolver::EffectiveSkillGraph,
    scope: TargetScope,
) -> Result<(), AppError> {
    let conflicts: Vec<_> = graph
        .conflicts()
        .into_iter()
        .filter(|conflict| {
            conflict
                .contenders
                .iter()
                .any(|candidate| candidate.scope == skill_scope(scope))
        })
        .collect();

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

fn resolve_imports_directory(
    context: &AppContext,
    manifest: &WorkspaceManifest,
) -> Result<PathBuf, AppError> {
    if manifest.imports.iter().any(|import| import.enabled) {
        Ok(home_directory()?.join(IMPORTS_STORE_RELATIVE_PATH))
    } else {
        Ok(context
            .working_directory
            .join(UNUSED_IMPORTS_PLACEHOLDER_DIR))
    }
}

fn canonical_root_path(
    scope: TargetScope,
    context: &AppContext,
    manifest: &WorkspaceManifest,
) -> Result<Option<PathBuf>, AppError> {
    match scope {
        TargetScope::Workspace => Ok(Some(normalize_path(
            &context
                .working_directory
                .join(manifest.layout.skills_dir.as_str()),
        ))),
        TargetScope::User => Ok(None),
    }
}

fn resolve_runtime_root_path(context: &AppContext, root: &str) -> Result<PathBuf, AppError> {
    if let Some(suffix) = root.strip_prefix("~/") {
        return Ok(normalize_path(&home_directory()?.join(suffix)));
    }
    if root == "~" {
        return Ok(normalize_path(&home_directory()?));
    }

    let path = PathBuf::from(root);
    if path.is_absolute() {
        Ok(normalize_path(&path))
    } else {
        Ok(normalize_path(&context.working_directory.join(path)))
    }
}

fn home_directory() -> Result<PathBuf, AppError> {
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .ok_or_else(|| AppError::MaterializationValidation {
            message: "unable to resolve the home directory for sync".into(),
        })
}

fn materialize_generated_root(
    root_path: &Path,
    root_display: &str,
    winners: &[&ResolvedSkillCandidate],
    prune: bool,
) -> Result<GeneratedRootReport, AppError> {
    let mut report = GeneratedRootReport {
        path: root_display.to_string(),
        materialized: Vec::new(),
        pruned: Vec::new(),
    };

    let mut desired_names = BTreeSet::new();
    for winner in winners {
        ensure_directory_parent(root_path)?;
        materialize_projection(root_path, root_display, winner)?;
        report
            .materialized
            .push(winner.skill.name.as_str().to_string());
        desired_names.insert(winner.skill.name.as_str().to_string());
    }

    if prune {
        report.pruned = prune_stale_projections(root_path, &desired_names)?;
    }

    Ok(report)
}

fn ensure_directory_parent(root_path: &Path) -> Result<(), AppError> {
    match fs::metadata(root_path) {
        Ok(metadata) if metadata.is_dir() => Ok(()),
        Ok(_) => Err(AppError::PathConflict {
            path: root_path.to_path_buf(),
            expected: "directory",
        }),
        Err(source) if source.kind() == io::ErrorKind::NotFound => fs::create_dir_all(root_path)
            .map_err(|source| AppError::FilesystemOperation {
                action: "create projection root",
                path: root_path.to_path_buf(),
                source,
            }),
        Err(source) => Err(AppError::FilesystemOperation {
            action: "inspect projection root",
            path: root_path.to_path_buf(),
            source,
        }),
    }
}

fn materialize_projection(
    root_path: &Path,
    root_display: &str,
    winner: &ResolvedSkillCandidate,
) -> Result<(), AppError> {
    let target_dir = root_path.join(winner.skill.name.as_str());
    prepare_target_directory(&target_dir, winner.skill.name.as_str())?;

    let mut files = winner.files.iter().collect::<Vec<_>>();
    files.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));

    for file in files {
        if file.relative_path.as_path() == Path::new(PROJECTION_METADATA_FILE) {
            continue;
        }

        let destination = target_dir.join(&file.relative_path);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).map_err(|source| AppError::FilesystemOperation {
                action: "create projection parent directory",
                path: parent.to_path_buf(),
                source,
            })?;
        }

        fs::copy(&file.source_path, &destination).map_err(|source| {
            AppError::FilesystemOperation {
                action: "copy projected file",
                path: destination.clone(),
                source,
            }
        })?;
    }

    write_projection_metadata(&target_dir, root_display, winner)
}

fn prepare_target_directory(target_dir: &Path, skill_name: &str) -> Result<(), AppError> {
    match fs::metadata(target_dir) {
        Ok(metadata) if !metadata.is_dir() => Err(AppError::PathConflict {
            path: target_dir.to_path_buf(),
            expected: "directory",
        }),
        Ok(_) if is_skillctl_generated_directory(target_dir)? => {
            fs::remove_dir_all(target_dir).map_err(|source| AppError::FilesystemOperation {
                action: "remove prior generated projection",
                path: target_dir.to_path_buf(),
                source,
            })?;
            fs::create_dir_all(target_dir).map_err(|source| AppError::FilesystemOperation {
                action: "recreate generated projection directory",
                path: target_dir.to_path_buf(),
                source,
            })
        }
        Ok(_) => Err(AppError::MaterializationValidation {
            message: format!(
                "refusing to overwrite hand-authored runtime skill directory '{}' for '{}'",
                target_dir.display(),
                skill_name,
            ),
        }),
        Err(source) if source.kind() == io::ErrorKind::NotFound => fs::create_dir_all(target_dir)
            .map_err(|source| AppError::FilesystemOperation {
                action: "create projected skill directory",
                path: target_dir.to_path_buf(),
                source,
            }),
        Err(source) => Err(AppError::FilesystemOperation {
            action: "inspect projected skill directory",
            path: target_dir.to_path_buf(),
            source,
        }),
    }
}

fn prune_stale_projections(
    root_path: &Path,
    desired_names: &BTreeSet<String>,
) -> Result<Vec<String>, AppError> {
    let entries = match fs::read_dir(root_path) {
        Ok(entries) => entries,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(AppError::FilesystemOperation {
                action: "read projection root",
                path: root_path.to_path_buf(),
                source,
            });
        }
    };

    let mut pruned = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| AppError::FilesystemOperation {
            action: "read projection root entry",
            path: root_path.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|source| AppError::FilesystemOperation {
                action: "inspect projection root entry",
                path: path.clone(),
                source,
            })?;
        if !file_type.is_dir() {
            continue;
        }

        let name = entry.file_name().to_string_lossy().into_owned();
        if desired_names.contains(&name) || !is_skillctl_generated_directory(&path)? {
            continue;
        }

        fs::remove_dir_all(&path).map_err(|source| AppError::FilesystemOperation {
            action: "prune stale projection",
            path: path.clone(),
            source,
        })?;
        pruned.push(name);
    }

    pruned.sort();
    Ok(pruned)
}

fn is_skillctl_generated_directory(path: &Path) -> Result<bool, AppError> {
    let metadata_path = path.join(PROJECTION_METADATA_FILE);
    let contents = match fs::read_to_string(&metadata_path) {
        Ok(contents) => contents,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(source) => {
            return Err(AppError::FilesystemOperation {
                action: "read projection metadata",
                path: metadata_path,
                source,
            });
        }
    };

    let parsed = serde_json::from_str::<serde_json::Value>(&contents).map_err(|source| {
        AppError::MaterializationValidation {
            message: format!(
                "projection metadata '{}' is invalid JSON: {source}",
                metadata_path.display()
            ),
        }
    })?;

    Ok(parsed
        .get("tool")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|tool| tool == "skillctl"))
}

fn write_projection_metadata(
    target_dir: &Path,
    root_display: &str,
    winner: &ResolvedSkillCandidate,
) -> Result<(), AppError> {
    let metadata_path = target_dir.join(PROJECTION_METADATA_FILE);
    let metadata = json!({
        "tool": "skillctl",
        "version": env!("CARGO_PKG_VERSION"),
        "generated_at": projection_timestamp(),
        "generation_mode": "copy",
        "physical_root": root_display,
        "skill_name": winner.skill.name.as_str(),
        "internal_id": internal_id_label(&winner.internal_id),
        "source": projection_source(winner),
    });
    let serialized = serde_json::to_string_pretty(&metadata)?;

    fs::write(&metadata_path, format!("{serialized}\n")).map_err(|source| {
        AppError::FilesystemOperation {
            action: "write projection metadata",
            path: metadata_path,
            source,
        }
    })
}

fn projection_source(winner: &ResolvedSkillCandidate) -> serde_json::Value {
    match (&winner.internal_id, &winner.import, &winner.overlay) {
        (
            InternalSkillId::Local {
                scope,
                relative_path,
            },
            _,
            _,
        ) => json!({
            "kind": "canonical-local",
            "scope": scope.as_str(),
            "relative_path": relative_path,
            "resolved_revision": serde_json::Value::Null,
            "overlay_path": serde_json::Value::Null,
        }),
        (
            InternalSkillId::Imported {
                scope,
                import_id,
                source_url,
                subpath,
            },
            Some(import),
            overlay,
        ) => json!({
            "kind": "imported",
            "scope": scope.as_str(),
            "import_id": import_id,
            "source_url": source_url,
            "selected_subpath": subpath,
            "resolved_revision": import.resolved_revision,
            "overlay_path": overlay.as_ref().map(|overlay| overlay.root.display().to_string()),
        }),
        _ => json!({
            "kind": "unknown",
        }),
    }
}

fn internal_id_label(internal_id: &InternalSkillId) -> String {
    match internal_id {
        InternalSkillId::Local {
            scope,
            relative_path,
        } => format!("local:{}:{relative_path}", scope.as_str()),
        InternalSkillId::Imported {
            scope,
            import_id,
            subpath,
            ..
        } => format!("import:{}:{import_id}:{subpath}", scope.as_str()),
    }
}

fn projection_timestamp() -> String {
    if let Some(value) = env::var_os("SOURCE_DATE_EPOCH") {
        return value.to_string_lossy().into_owned();
    }

    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time must be after unix epoch")
        .as_secs()
        .to_string()
}

fn projection_mode_label(mode: ProjectionMode) -> &'static str {
    match mode {
        ProjectionMode::Copy => "copy",
        ProjectionMode::Symlink => "symlink",
    }
}

fn sync_summary(report: &MaterializationReport) -> String {
    let generated = report.materialized_skills;
    let pruned = report.pruned_skills;
    let generated_roots = report.generated_roots.len();
    let canonical_suffix = if report.canonical_roots.is_empty() {
        String::new()
    } else {
        format!(
            " Reused canonical root{}: {}.",
            plural_suffix(report.canonical_roots.len()),
            report.canonical_roots.join(", ")
        )
    };

    if generated == 0 {
        if pruned == 0 {
            format!("No generated projections were required.{canonical_suffix}")
        } else {
            format!(
                "No generated projections were required. Pruned {pruned} stale projection{}.{canonical_suffix}",
                plural_suffix(pruned),
            )
        }
    } else {
        let mut summary = format!(
            "Materialized {generated} generated projection{} across {generated_roots} root{}.",
            plural_suffix(generated),
            plural_suffix(generated_roots),
        );
        if pruned > 0 {
            summary.push_str(&format!(
                " Pruned {pruned} stale projection{}.",
                plural_suffix(pruned),
            ));
        }
        summary.push_str(&canonical_suffix);
        summary
    }
}

fn plural_suffix(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();

    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(Path::new("/")),
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(segment) => normalized.push(segment),
        }
    }

    normalized
}
