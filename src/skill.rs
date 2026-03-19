//! Skill inventory, parsing, and inspection domain entry points.

use std::{
    collections::BTreeMap,
    fs, io,
    path::{Component, Path, PathBuf},
};

use serde_json::json;
use serde_yaml::Value;

use crate::{
    adapter::{AdapterRegistry, TargetScope},
    app::AppContext,
    cli::Scope,
    doctor,
    error::AppError,
    history::{self, HistoryLedger},
    lockfile::WorkspaceLockfile,
    manifest::{ImportDefinition, ManifestScope, WorkspaceManifest},
    materialize::{self, MaterializationReport},
    planner,
    response::AppResponse,
    source::{current_timestamp, imports_store_root},
    state::{LocalStateStore, ManagedScope, ManagedSkillRef, ProjectionMode, ProjectionRecord},
};

/// Default relative path to canonical workspace skills.
pub const DEFAULT_SKILLS_DIR: &str = ".agents/skills";

/// Standard skill manifest file name.
pub const SKILL_MANIFEST_FILE: &str = "SKILL.md";
/// Vendor-specific OpenAI metadata file preserved by `skillctl`.
pub const OPENAI_METADATA_FILE: &str = "agents/openai.yaml";
/// Claude-specific frontmatter fields that should pass through untouched.
pub const CLAUDE_FRONTMATTER_FIELDS: &[&str] = &[
    "disable-model-invocation",
    "user-invocable",
    "context",
    "agent",
    "hooks",
];

const STANDARD_FRONTMATTER_FIELDS: &[&str] = &[
    "name",
    "description",
    "license",
    "compatibility",
    "metadata",
    "allowed-tools",
];

/// Strongly typed skill identifier.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct SkillName(pub String);

impl SkillName {
    /// Parse and validate a skill name against the open Agent Skills contract.
    pub fn parse(value: &str, skill_path: &Path) -> Result<Self, AppError> {
        validate_skill_name(value, skill_path)?;
        Ok(Self(value.to_string()))
    }

    /// Borrow the validated skill name as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Parsed frontmatter for a `SKILL.md` document.
#[derive(Clone, Debug, PartialEq)]
pub struct SkillFrontmatter {
    /// All parsed frontmatter fields, including vendor-specific extensions.
    pub fields: BTreeMap<String, Value>,
    /// Vendor-specific fields that should be passed through unchanged.
    pub vendor_fields: BTreeMap<String, Value>,
}

/// Vendor-specific metadata files associated with a skill directory.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SkillVendorMetadata {
    /// Raw file contents keyed by stable relative path under the skill root.
    pub files: BTreeMap<PathBuf, String>,
}

/// Parsed skill directory definition.
#[derive(Clone, Debug, PartialEq)]
pub struct SkillDefinition {
    /// Identifier declared in `SKILL.md`.
    pub name: SkillName,
    /// Human-facing description declared in `SKILL.md`.
    pub description: String,
    /// Filesystem path to the skill root.
    pub root: PathBuf,
    /// Resolved path to the `SKILL.md` file.
    pub manifest_path: PathBuf,
    /// Markdown body after the frontmatter block.
    pub body: String,
    /// Parsed frontmatter, including vendor-specific passthrough fields.
    pub frontmatter: SkillFrontmatter,
    /// Supported vendor-specific metadata files preserved alongside the skill.
    pub vendor_metadata: SkillVendorMetadata,
}

/// Parsed canonical workspace skill definition.
pub type WorkspaceSkill = SkillDefinition;

impl SkillDefinition {
    /// Load, parse, and validate a skill directory.
    pub fn load_from_dir(root: impl AsRef<Path>) -> Result<Self, AppError> {
        let root = root.as_ref();
        ensure_directory(root)?;

        let manifest_path = root.join(SKILL_MANIFEST_FILE);
        let source = read_skill_manifest(&manifest_path)?;
        let vendor_metadata = load_vendor_metadata(root)?;

        Self::from_source(root, manifest_path, &source, vendor_metadata)
    }

    /// Parse and validate a skill definition from explicit file contents.
    pub fn from_source(
        root: impl AsRef<Path>,
        manifest_path: impl Into<PathBuf>,
        source: &str,
        vendor_metadata: SkillVendorMetadata,
    ) -> Result<Self, AppError> {
        let root = root.as_ref();
        let manifest_path = manifest_path.into();
        let (frontmatter_source, body) = split_frontmatter_sections(source, &manifest_path)?;
        let fields = parse_frontmatter(&frontmatter_source, &manifest_path)?;

        validate_optional_standard_fields(&fields, &manifest_path)?;

        let name = SkillName::parse(
            require_string_field(&fields, "name", &manifest_path)?,
            &manifest_path,
        )?;
        let description = require_string_field(&fields, "description", &manifest_path)?;
        validate_description(description, &manifest_path)?;
        validate_directory_name(root, name.as_str(), &manifest_path)?;

        Ok(Self {
            name,
            description: description.to_string(),
            root: root.to_path_buf(),
            manifest_path,
            body,
            frontmatter: SkillFrontmatter {
                vendor_fields: extract_vendor_frontmatter(&fields),
                fields,
            },
            vendor_metadata,
        })
    }
}

/// Validate and normalize a relative overlay path used for shadow-file mapping.
pub fn normalize_overlay_relative_path(path: impl AsRef<Path>) -> Result<PathBuf, AppError> {
    let path = path.as_ref();
    if path.as_os_str().is_empty() {
        return Err(skill_validation(path, "overlay path must not be empty"));
    }

    let path_display = path.to_string_lossy();
    if path_display.contains('\\') {
        return Err(skill_validation(
            path,
            format!("overlay path '{}' must use '/' separators", path.display()),
        ));
    }

    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(segment) => normalized.push(segment),
            Component::CurDir
            | Component::ParentDir
            | Component::RootDir
            | Component::Prefix(_) => {
                return Err(skill_validation(
                    path,
                    format!(
                        "overlay path '{}' must be relative and must not contain '.' or '..' segments",
                        path.display()
                    ),
                ));
            }
        }
    }

    if normalized.as_os_str().is_empty() {
        return Err(skill_validation(path, "overlay path must not be empty"));
    }

    Ok(normalized)
}

/// Typed request for `skillctl list`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ListRequest;

/// Typed request for `skillctl remove`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoveRequest {
    /// Managed skill name.
    pub skill: SkillName,
}

impl RemoveRequest {
    /// Create a remove request from parsed CLI arguments.
    pub fn new(skill: String) -> Self {
        Self {
            skill: SkillName(skill),
        }
    }
}

/// Typed request for `skillctl explain`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExplainRequest {
    /// Managed skill name.
    pub skill: SkillName,
}

impl ExplainRequest {
    /// Create an explain request from parsed CLI arguments.
    pub fn new(skill: String) -> Self {
        Self {
            skill: SkillName(skill),
        }
    }
}

/// Typed request for `skillctl enable`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EnableRequest {
    /// Managed skill name.
    pub skill: SkillName,
}

impl EnableRequest {
    /// Create an enable request from parsed CLI arguments.
    pub fn new(skill: String) -> Self {
        Self {
            skill: SkillName(skill),
        }
    }
}

/// Typed request for `skillctl disable`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DisableRequest {
    /// Managed skill name.
    pub skill: SkillName,
}

impl DisableRequest {
    /// Create a disable request from parsed CLI arguments.
    pub fn new(skill: String) -> Self {
        Self {
            skill: SkillName(skill),
        }
    }
}

/// Typed request for `skillctl path`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PathRequest {
    /// Managed skill name.
    pub skill: SkillName,
}

impl PathRequest {
    /// Create a path request from parsed CLI arguments.
    pub fn new(skill: String) -> Self {
        Self {
            skill: SkillName(skill),
        }
    }
}

/// Handle `skillctl list`.
pub fn handle_list(context: &AppContext, _request: ListRequest) -> Result<AppResponse, AppError> {
    let store = LocalStateStore::open_default()?;
    let installs = store.list_install_records()?;
    if installs.is_empty() {
        return Ok(AppResponse::success("list")
            .with_summary("No managed skills are installed.")
            .with_data(json!({ "skills": [] })));
    }

    let manifest = load_manifest_or_default(&context.working_directory)?;
    let lockfile = load_lockfile_or_default(&context.working_directory)?;
    let mut skills = Vec::with_capacity(installs.len());

    for install in installs {
        let managed_import = managed_import(&manifest, &install.skill);
        let lockfile_entry = managed_import.and_then(|import| lockfile.imports.get(&import.id));
        let snapshot = store.skill_snapshot(&install.skill)?;

        skills.push(json!({
            "skill": install.skill.skill_id,
            "scope": install.skill.scope.as_str(),
            "managed_import": managed_import.is_some(),
            "managed_import_enabled": managed_import.map(|import| import.enabled),
            "source": {
                "type": install.source_kind,
                "url": install.source_url,
                "subpath": install.source_subpath,
            },
            "resolved_revision": install.resolved_revision,
            "effective_version_hash": install.effective_version_hash,
            "detached": install.detached,
            "forked": install.forked,
            "overlay_root": managed_import
                .and_then(|import| manifest.overrides.get(&import.id))
                .map(|path| path.as_str().to_string()),
            "stored_source_root": lockfile_entry
                .map(|_| imports_store_root())
                .transpose()?
                .map(|root| root.join(&install.skill.skill_id).display().to_string()),
            "active_source_root": lockfile_entry
                .map(|entry| imports_store_root().map(|root| root.join(&install.skill.skill_id).join(entry.source.subpath.as_str())))
                .transpose()?
                .map(|path| path.display().to_string()),
            "pinned_reference": snapshot.pin.as_ref().map(|pin| pin.requested_reference.clone()),
            "latest_update": snapshot.latest_update_check.as_ref().map(|check| json!({
                "outcome": check.outcome,
                "checked_at": check.checked_at,
            })),
            "local_modification_count": snapshot.local_modifications.len(),
            "rollback_count": snapshot.rollbacks.len(),
            "projections": snapshot
                .projections
                .iter()
                .map(|projection| projection_view(context, projection))
                .collect::<Result<Vec<_>, _>>()?,
        }));
    }

    let count = skills.len();
    Ok(AppResponse::success("list")
        .with_summary(format!(
            "Listed {count} managed skill{}.",
            if count == 1 { "" } else { "s" }
        ))
        .with_data(json!({ "skills": skills })))
}

/// Handle `skillctl remove`.
pub fn handle_remove(
    context: &AppContext,
    request: RemoveRequest,
) -> Result<AppResponse, AppError> {
    let mut manifest = load_manifest_or_default(&context.working_directory)?;
    let mut lockfile = load_lockfile_or_default(&context.working_directory)?;
    let mut store = LocalStateStore::open_default()?;
    let managed_skill =
        resolve_installed_skill(&store, request.skill.as_str(), context.selector.scope)?;
    let install_record =
        store
            .install_record(&managed_skill)?
            .ok_or_else(|| AppError::ResolutionValidation {
                message: format!(
                    "skill '{}' does not have an installed state record",
                    request.skill.as_str()
                ),
            })?;
    let timestamp = current_timestamp();
    let mut removed_paths = Vec::new();
    let mut retained_paths = Vec::new();

    if let Some(import_index) = manifest_import_index(&manifest, &managed_skill) {
        manifest.imports.remove(import_index);
        if let Some(overlay_path) = manifest.overrides.remove(request.skill.as_str()) {
            retained_paths.push(overlay_path.as_str().to_string());
        }
        manifest.write_to_path()?;

        lockfile.imports.remove(request.skill.as_str());
        lockfile.write_to_path()?;
    } else if !(install_record.detached || install_record.forked) {
        return Err(AppError::ResolutionValidation {
            message: format!(
                "skill '{}' is not a managed import in the workspace manifest",
                request.skill.as_str()
            ),
        });
    }

    if managed_skill.scope == ManagedScope::Workspace
        && (install_record.detached || install_record.forked)
    {
        let local_root = context
            .working_directory
            .join(manifest.layout.skills_dir.as_str())
            .join(request.skill.as_str());
        if remove_directory_if_exists(&local_root, "remove canonical workspace skill")? {
            removed_paths.push(planner::display_path(context, &local_root));
        }
    }

    let stored_source_root = imports_store_root()?.join(request.skill.as_str());
    if remove_directory_if_exists(&stored_source_root, "remove stored import root")? {
        removed_paths.push(stored_source_root.display().to_string());
    }

    let sync_report =
        materialize::sync_workspace(&history::context_for_scope(context, managed_skill.scope))?;

    {
        let mut ledger = HistoryLedger::new(&mut store);
        record_pruned_projection_history(
            &mut ledger,
            context,
            managed_skill.scope,
            &sync_report,
            &timestamp,
        )?;
        for removed_path in &removed_paths {
            ledger.record_cleanup(Some(&managed_skill), removed_path, &timestamp)?;
        }
    }

    store.delete_current_skill_state(&managed_skill)?;
    rebuild_projection_records_for_scope(
        &mut store,
        managed_skill.scope,
        &sync_report,
        &timestamp,
    )?;

    let mut response = AppResponse::success("remove")
        .with_summary(format!(
            "Removed {} from {} scope.",
            request.skill.as_str(),
            managed_skill.scope.as_str()
        ))
        .with_data(json!({
            "skill": request.skill.as_str(),
            "scope": managed_skill.scope.as_str(),
            "removed_paths": removed_paths,
            "retained_paths": retained_paths,
            "projection": sync_report,
        }));

    for retained_path in &retained_paths {
        response = response.with_warning(format!(
            "retained overlay directory '{}' for manual reuse",
            retained_path
        ));
    }

    Ok(response)
}

/// Handle `skillctl explain`.
pub fn handle_explain(
    context: &AppContext,
    request: ExplainRequest,
) -> Result<AppResponse, AppError> {
    doctor::build_explain_response(context, request.skill.as_str())
}

/// Handle `skillctl enable`.
pub fn handle_enable(
    context: &AppContext,
    request: EnableRequest,
) -> Result<AppResponse, AppError> {
    toggle_managed_import(context, request.skill.as_str(), true, "enable")
}

/// Handle `skillctl disable`.
pub fn handle_disable(
    context: &AppContext,
    request: DisableRequest,
) -> Result<AppResponse, AppError> {
    toggle_managed_import(context, request.skill.as_str(), false, "disable")
}

/// Handle `skillctl path`.
pub fn handle_path(context: &AppContext, request: PathRequest) -> Result<AppResponse, AppError> {
    let manifest = load_manifest_or_default(&context.working_directory)?;
    let lockfile = load_lockfile_or_default(&context.working_directory)?;
    let store = LocalStateStore::open_default()?;
    let managed_skill =
        resolve_installed_skill(&store, request.skill.as_str(), context.selector.scope)?;
    let install_record =
        store
            .install_record(&managed_skill)?
            .ok_or_else(|| AppError::ResolutionValidation {
                message: format!(
                    "skill '{}' does not have an installed state record",
                    request.skill.as_str()
                ),
            })?;
    let snapshot = store.skill_snapshot(&managed_skill)?;
    let managed_import = managed_import(&manifest, &managed_skill);
    let lockfile_entry = managed_import.and_then(|import| lockfile.imports.get(&import.id));
    let planned_roots = planned_root_views(
        context,
        &manifest,
        managed_skill.scope,
        request.skill.as_str(),
    )?;

    Ok(AppResponse::success("path")
        .with_summary(format!("Resolved filesystem paths for {}.", request.skill.as_str()))
        .with_data(json!({
            "skill": request.skill.as_str(),
            "scope": managed_skill.scope.as_str(),
            "managed_import": managed_import.is_some(),
            "managed_import_enabled": managed_import.map(|import| import.enabled),
            "stored_source_root": lockfile_entry
                .map(|_| imports_store_root())
                .transpose()?
                .map(|root| root.join(request.skill.as_str()).display().to_string()),
            "active_source_root": lockfile_entry
                .map(|entry| imports_store_root().map(|root| root.join(request.skill.as_str()).join(entry.source.subpath.as_str())))
                .transpose()?
                .map(|path| path.display().to_string()),
            "overlay_root": managed_import
                .and_then(|import| manifest.overrides.get(&import.id))
                .map(|path| path.as_str().to_string()),
            "canonical_root": if managed_import.is_none() && managed_skill.scope == ManagedScope::Workspace {
                let local_root = context
                    .working_directory
                    .join(manifest.layout.skills_dir.as_str())
                    .join(request.skill.as_str());
                fs::metadata(&local_root)
                    .ok()
                    .filter(|metadata| metadata.is_dir())
                    .map(|_| planner::display_path(context, &local_root))
            } else {
                None
            },
            "source": {
                "type": install_record.source_kind,
                "url": install_record.source_url,
                "subpath": install_record.source_subpath,
            },
            "resolved_revision": install_record.resolved_revision,
            "effective_version_hash": install_record.effective_version_hash,
            "planned_roots": planned_roots,
            "projections": snapshot
                .projections
                .iter()
                .map(|projection| projection_view(context, projection))
                .collect::<Result<Vec<_>, _>>()?,
        })))
}

fn load_manifest_or_default(working_directory: &Path) -> Result<WorkspaceManifest, AppError> {
    match WorkspaceManifest::load_from_workspace(working_directory) {
        Ok(manifest) => Ok(manifest),
        Err(AppError::FilesystemOperation {
            action: "read manifest",
            path,
            source,
        }) if source.kind() == io::ErrorKind::NotFound => Ok(WorkspaceManifest::default_at(path)),
        Err(error) => Err(error),
    }
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

pub(crate) fn resolve_installed_skill(
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
            message: format!("skill '{}' is not installed", skill),
        }),
        _ => Err(AppError::ResolutionValidation {
            message: format!(
                "skill '{}' exists in multiple scopes; re-run with --scope",
                skill
            ),
        }),
    }
}

fn projection_view(
    context: &AppContext,
    projection: &ProjectionRecord,
) -> Result<serde_json::Value, AppError> {
    let root = planner::resolve_runtime_root_path(context, &projection.physical_root)?;
    let projected = root.join(&projection.projected_path);

    Ok(json!({
        "target": projection.target,
        "root": planner::display_path(context, &root),
        "path": planner::display_path(context, &projected),
        "generated_at": projection.generated_at,
        "effective_version_hash": projection.effective_version_hash,
    }))
}

fn planned_root_views(
    context: &AppContext,
    manifest: &WorkspaceManifest,
    scope: ManagedScope,
    skill: &str,
) -> Result<Vec<serde_json::Value>, AppError> {
    if manifest.targets.is_empty() {
        return Ok(Vec::new());
    }

    let plan = planner::plan_target_roots(
        &AdapterRegistry::new(),
        target_scope(scope),
        manifest.projection.policy,
        &manifest.targets,
        &manifest.adapters,
    )?;

    plan.assignments
        .into_iter()
        .map(|assignment| {
            let root = planner::resolve_runtime_root_path(context, &assignment.root)?;
            let path = root.join(skill);
            Ok(json!({
                "target": assignment.target,
                "root": planner::display_path(context, &root),
                "path": planner::display_path(context, &path),
                "source": assignment.source,
            }))
        })
        .collect()
}

fn toggle_managed_import(
    context: &AppContext,
    skill: &str,
    enabled: bool,
    command: &'static str,
) -> Result<AppResponse, AppError> {
    let mut manifest = WorkspaceManifest::load_from_workspace(&context.working_directory)?;
    let mut store = LocalStateStore::open_default()?;
    let managed_skill = resolve_installed_skill(&store, skill, context.selector.scope)?;
    let import_index = manifest_import_index(&manifest, &managed_skill).ok_or_else(|| {
        AppError::ResolutionValidation {
            message: format!(
                "skill '{}' is not a managed import in the workspace manifest",
                skill
            ),
        }
    })?;
    let changed = manifest.imports[import_index].enabled != enabled;
    manifest.imports[import_index].enabled = enabled;
    manifest.write_to_path()?;

    let timestamp = current_timestamp();
    let sync_report =
        materialize::sync_workspace(&history::context_for_scope(context, managed_skill.scope))?;
    let current_projections = rebuild_projection_records_for_scope(
        &mut store,
        managed_skill.scope,
        &sync_report,
        &timestamp,
    )?;

    let mut ledger = HistoryLedger::new(&mut store);
    record_pruned_projection_history(
        &mut ledger,
        context,
        managed_skill.scope,
        &sync_report,
        &timestamp,
    )?;
    for record in current_projections
        .iter()
        .filter(|record| record.skill == managed_skill)
    {
        ledger.record_projection(record)?;
    }

    Ok(AppResponse::success(command)
        .with_summary(format!(
            "{} {} in {} scope.",
            if enabled { "Enabled" } else { "Disabled" },
            skill,
            managed_skill.scope.as_str()
        ))
        .with_data(json!({
            "skill": skill,
            "scope": managed_skill.scope.as_str(),
            "enabled": enabled,
            "changed": changed,
            "projection": sync_report,
        })))
}

fn rebuild_projection_records_for_scope(
    store: &mut LocalStateStore,
    scope: ManagedScope,
    sync_report: &MaterializationReport,
    generated_at: &str,
) -> Result<Vec<ProjectionRecord>, AppError> {
    let installs_by_skill: BTreeMap<_, _> = store
        .list_install_records()?
        .into_iter()
        .filter(|record| record.skill.scope == scope)
        .map(|record| (record.skill.skill_id.clone(), record))
        .collect();
    let targets_by_root: BTreeMap<_, _> = sync_report
        .plan
        .physical_roots
        .iter()
        .map(|root| (root.path.clone(), root.targets.clone()))
        .collect();
    let mut records = Vec::new();

    for generated_root in &sync_report.generated_roots {
        let Some(targets) = targets_by_root.get(&generated_root.path) else {
            continue;
        };

        for skill_name in &generated_root.materialized {
            let Some(install) = installs_by_skill.get(skill_name) else {
                continue;
            };

            for target in targets {
                records.push(ProjectionRecord {
                    skill: install.skill.clone(),
                    target: *target,
                    generation_mode: ProjectionMode::Copy,
                    physical_root: generated_root.path.clone(),
                    projected_path: skill_name.clone(),
                    effective_version_hash: install.effective_version_hash.clone(),
                    generated_at: generated_at.to_string(),
                });
            }
        }
    }

    store.replace_projection_records_for_scope(scope, &records)?;
    Ok(records)
}

fn record_pruned_projection_history(
    ledger: &mut HistoryLedger<'_>,
    context: &AppContext,
    scope: ManagedScope,
    sync_report: &MaterializationReport,
    occurred_at: &str,
) -> Result<(), AppError> {
    for generated_root in &sync_report.generated_roots {
        let root = planner::resolve_runtime_root_path(context, &generated_root.path)?;
        for skill_name in &generated_root.pruned {
            let skill = ManagedSkillRef::new(scope, skill_name.clone());
            let path = planner::display_path(context, &root.join(skill_name));
            ledger.record_prune(Some(&skill), &path, occurred_at)?;
        }
    }

    Ok(())
}

fn remove_directory_if_exists(path: &Path, action: &'static str) -> Result<bool, AppError> {
    match fs::metadata(path) {
        Ok(metadata) if !metadata.is_dir() => Err(AppError::PathConflict {
            path: path.to_path_buf(),
            expected: "directory",
        }),
        Ok(_) => {
            fs::remove_dir_all(path).map_err(|source| AppError::FilesystemOperation {
                action,
                path: path.to_path_buf(),
                source,
            })?;
            Ok(true)
        }
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(AppError::FilesystemOperation {
            action: "inspect directory",
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn managed_import<'a>(
    manifest: &'a WorkspaceManifest,
    skill: &ManagedSkillRef,
) -> Option<&'a ImportDefinition> {
    manifest
        .imports
        .iter()
        .find(|import| import.id == skill.skill_id && import.scope == manifest_scope(skill.scope))
}

fn manifest_import_index(manifest: &WorkspaceManifest, skill: &ManagedSkillRef) -> Option<usize> {
    manifest.imports.iter().position(|import| {
        import.id == skill.skill_id && import.scope == manifest_scope(skill.scope)
    })
}

fn manifest_scope(scope: ManagedScope) -> ManifestScope {
    match scope {
        ManagedScope::Workspace => ManifestScope::Workspace,
        ManagedScope::User => ManifestScope::User,
    }
}

fn target_scope(scope: ManagedScope) -> TargetScope {
    match scope {
        ManagedScope::Workspace => TargetScope::Workspace,
        ManagedScope::User => TargetScope::User,
    }
}

fn ensure_directory(path: &Path) -> Result<(), AppError> {
    let metadata = fs::metadata(path).map_err(|source| AppError::FilesystemOperation {
        action: "inspect skill directory",
        path: path.to_path_buf(),
        source,
    })?;

    if !metadata.is_dir() {
        return Err(AppError::PathConflict {
            path: path.to_path_buf(),
            expected: "directory",
        });
    }

    Ok(())
}

fn read_skill_manifest(path: &Path) -> Result<String, AppError> {
    match fs::read_to_string(path) {
        Ok(contents) => Ok(contents),
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            Err(skill_validation(path, "SKILL.md does not exist"))
        }
        Err(source) => Err(AppError::FilesystemOperation {
            action: "read skill manifest",
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn split_frontmatter_sections(
    source: &str,
    skill_path: &Path,
) -> Result<(String, String), AppError> {
    let source = source.strip_prefix('\u{feff}').unwrap_or(source);
    let mut lines = source.lines();

    if lines.next() != Some("---") {
        return Err(skill_validation(
            skill_path,
            "SKILL.md must start with a '---' frontmatter delimiter",
        ));
    }

    let mut frontmatter_lines = Vec::new();
    let mut body_lines = Vec::new();
    let mut in_body = false;

    for line in lines {
        if !in_body && line == "---" {
            in_body = true;
            continue;
        }

        if in_body {
            body_lines.push(line);
        } else {
            frontmatter_lines.push(line);
        }
    }

    if !in_body {
        return Err(skill_validation(
            skill_path,
            "SKILL.md frontmatter must end with a closing '---' delimiter",
        ));
    }

    Ok((
        frontmatter_lines.join("\n"),
        body_lines.join("\n").trim().to_string(),
    ))
}

fn parse_frontmatter(
    frontmatter_source: &str,
    skill_path: &Path,
) -> Result<BTreeMap<String, Value>, AppError> {
    let parsed = serde_yaml::from_str::<Value>(frontmatter_source).map_err(|source| {
        AppError::SkillParse {
            path: skill_path.to_path_buf(),
            source,
        }
    })?;

    let mapping = parsed.as_mapping().ok_or_else(|| {
        skill_validation(skill_path, "SKILL.md frontmatter must be a YAML mapping")
    })?;

    let mut fields = BTreeMap::new();
    for (key, value) in mapping {
        let key = key.as_str().ok_or_else(|| {
            skill_validation(skill_path, "SKILL.md frontmatter keys must be strings")
        })?;
        fields.insert(key.to_string(), value.clone());
    }

    Ok(fields)
}

fn require_string_field<'a>(
    fields: &'a BTreeMap<String, Value>,
    field: &str,
    skill_path: &Path,
) -> Result<&'a str, AppError> {
    let value = fields.get(field).ok_or_else(|| {
        skill_validation(
            skill_path,
            format!("SKILL.md frontmatter must define '{field}'"),
        )
    })?;

    value.as_str().ok_or_else(|| {
        skill_validation(
            skill_path,
            format!("SKILL.md field '{field}' must be a string"),
        )
    })
}

fn validate_skill_name(value: &str, skill_path: &Path) -> Result<(), AppError> {
    if value.is_empty() {
        return Err(skill_validation(
            skill_path,
            "SKILL.md field 'name' must not be empty",
        ));
    }
    if value.len() > 64 {
        return Err(skill_validation(
            skill_path,
            format!(
                "SKILL.md field 'name' must be at most 64 characters, found {}",
                value.len()
            ),
        ));
    }
    if value.starts_with('-') || value.ends_with('-') || value.contains("--") {
        return Err(skill_validation(
            skill_path,
            format!(
                "SKILL.md field 'name' must use lowercase letters, digits, and single hyphens: '{value}'"
            ),
        ));
    }
    if !value
        .bytes()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(skill_validation(
            skill_path,
            format!(
                "SKILL.md field 'name' must use lowercase letters, digits, and hyphens only: '{value}'"
            ),
        ));
    }

    Ok(())
}

fn validate_description(value: &str, skill_path: &Path) -> Result<(), AppError> {
    if value.trim().is_empty() {
        return Err(skill_validation(
            skill_path,
            "SKILL.md field 'description' must not be empty",
        ));
    }
    if value.len() > 1_024 {
        return Err(skill_validation(
            skill_path,
            format!(
                "SKILL.md field 'description' must be at most 1024 characters, found {}",
                value.len()
            ),
        ));
    }

    Ok(())
}

fn validate_directory_name(root: &Path, name: &str, skill_path: &Path) -> Result<(), AppError> {
    let directory_name = root
        .file_name()
        .and_then(|segment| segment.to_str())
        .ok_or_else(|| {
            skill_validation(
                skill_path,
                format!(
                    "skill directory '{}' must end in a valid UTF-8 directory name",
                    root.display()
                ),
            )
        })?;

    if directory_name != name {
        return Err(skill_validation(
            skill_path,
            format!(
                "SKILL.md field 'name' must match the parent directory '{}', found '{}'",
                directory_name, name
            ),
        ));
    }

    Ok(())
}

fn validate_optional_standard_fields(
    fields: &BTreeMap<String, Value>,
    skill_path: &Path,
) -> Result<(), AppError> {
    for field in ["license", "compatibility", "allowed-tools"] {
        if let Some(value) = fields.get(field)
            && !value.is_string()
        {
            return Err(skill_validation(
                skill_path,
                format!("SKILL.md field '{field}' must be a string when present"),
            ));
        }
    }

    if let Some(value) = fields.get("metadata") {
        let mapping = value.as_mapping().ok_or_else(|| {
            skill_validation(
                skill_path,
                "SKILL.md field 'metadata' must be a YAML mapping when present",
            )
        })?;

        for key in mapping.keys() {
            if !key.is_string() {
                return Err(skill_validation(
                    skill_path,
                    "SKILL.md field 'metadata' must use string keys",
                ));
            }
        }
    }

    Ok(())
}

fn extract_vendor_frontmatter(fields: &BTreeMap<String, Value>) -> BTreeMap<String, Value> {
    fields
        .iter()
        .filter(|(key, _)| !STANDARD_FRONTMATTER_FIELDS.contains(&key.as_str()))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

fn load_vendor_metadata(root: &Path) -> Result<SkillVendorMetadata, AppError> {
    let mut files = BTreeMap::new();
    let relative_path = PathBuf::from(OPENAI_METADATA_FILE);
    let path = root.join(&relative_path);

    match fs::metadata(&path) {
        Ok(metadata) => {
            if !metadata.is_file() {
                return Err(AppError::PathConflict {
                    path,
                    expected: "file",
                });
            }

            let contents =
                fs::read_to_string(&path).map_err(|source| AppError::FilesystemOperation {
                    action: "read vendor metadata file",
                    path: path.clone(),
                    source,
                })?;
            files.insert(relative_path, contents);
        }
        Err(source) if source.kind() == io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(AppError::FilesystemOperation {
                action: "inspect vendor metadata file",
                path,
                source,
            });
        }
    }

    Ok(SkillVendorMetadata { files })
}

fn skill_validation(path: &Path, message: impl Into<String>) -> AppError {
    AppError::SkillValidation {
        path: path.to_path_buf(),
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        env,
        time::{SystemTime, UNIX_EPOCH},
    };

    const VALID_SKILL: &str = concat!(
        "---\n",
        "name: release-notes\n",
        "description: Summarize a project's release notes.\n",
        "metadata:\n",
        "  owner: docs\n",
        "user-invocable: true\n",
        "context:\n",
        "  - repo\n",
        "hooks:\n",
        "  pre:\n",
        "    - echo prepare\n",
        "---\n",
        "\n",
        "# Release Notes\n",
        "Use this skill to summarize changes.\n",
    );

    #[test]
    fn load_from_dir_parses_skill_frontmatter_and_vendor_metadata() {
        let skill = TestSkillDir::new("release-notes");
        skill.write(SKILL_MANIFEST_FILE, VALID_SKILL);
        skill.write(
            OPENAI_METADATA_FILE,
            concat!(
                "model: gpt-5.4\n",
                "instructions: Keep summaries concise.\n"
            ),
        );

        let parsed = SkillDefinition::load_from_dir(skill.path()).expect("skill parses");

        assert_eq!(parsed.name, SkillName("release-notes".to_string()));
        assert_eq!(parsed.description, "Summarize a project's release notes.");
        assert_eq!(
            parsed.body,
            "# Release Notes\nUse this skill to summarize changes."
        );
        assert_eq!(
            parsed.frontmatter.vendor_fields.get("user-invocable"),
            Some(&Value::Bool(true))
        );
        assert!(parsed.frontmatter.vendor_fields.contains_key("context"));
        assert!(parsed.frontmatter.vendor_fields.contains_key("hooks"));
        assert_eq!(
            parsed
                .vendor_metadata
                .files
                .get(&PathBuf::from(OPENAI_METADATA_FILE)),
            Some(&"model: gpt-5.4\ninstructions: Keep summaries concise.\n".to_string())
        );
    }

    #[test]
    fn load_from_dir_rejects_missing_skill_manifest() {
        let skill = TestSkillDir::new("release-notes");

        let error =
            SkillDefinition::load_from_dir(skill.path()).expect_err("missing SKILL.md is rejected");

        assert!(
            error.to_string().contains("SKILL.md does not exist"),
            "unexpected error: {error}"
        );
        assert_eq!(
            error.exit_status(),
            crate::error::ExitStatus::ValidationFailure
        );
    }

    #[test]
    fn load_from_dir_rejects_invalid_skill_name_format() {
        let skill = TestSkillDir::new("release-notes");
        skill.write(
            SKILL_MANIFEST_FILE,
            concat!(
                "---\n",
                "name: Release_Notes\n",
                "description: Summarize a project's release notes.\n",
                "---\n",
            ),
        );

        let error = SkillDefinition::load_from_dir(skill.path())
            .expect_err("invalid skill name is rejected");

        assert!(
            error
                .to_string()
                .contains("must use lowercase letters, digits, and hyphens only"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn load_from_dir_rejects_directory_mismatch() {
        let skill = TestSkillDir::new("release-notes");
        skill.write(
            SKILL_MANIFEST_FILE,
            concat!(
                "---\n",
                "name: bug-triage\n",
                "description: Summarize a project's release notes.\n",
                "---\n",
            ),
        );

        let error = SkillDefinition::load_from_dir(skill.path())
            .expect_err("mismatched directory name is rejected");

        assert!(
            error
                .to_string()
                .contains("must match the parent directory"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn load_from_dir_rejects_missing_description() {
        let skill = TestSkillDir::new("release-notes");
        skill.write(
            SKILL_MANIFEST_FILE,
            concat!("---\n", "name: release-notes\n", "---\n"),
        );

        let error = SkillDefinition::load_from_dir(skill.path())
            .expect_err("missing description is rejected");

        assert!(
            error.to_string().contains("must define 'description'"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn overlay_relative_paths_reject_path_traversal() {
        let error =
            normalize_overlay_relative_path("../SKILL.md").expect_err("path traversal should fail");

        assert!(
            error
                .to_string()
                .contains("must be relative and must not contain '.' or '..' segments"),
            "unexpected error: {error}"
        );
    }

    struct TestSkillDir {
        path: PathBuf,
        cleanup_root: PathBuf,
    }

    impl TestSkillDir {
        fn new(name: &str) -> Self {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("time moved backwards")
                .as_nanos();
            let cleanup_root = env::temp_dir().join(format!(
                "skillctl-skill-test-{}-{unique}",
                std::process::id()
            ));
            let path = cleanup_root.join(name);
            fs::create_dir_all(&path).expect("skill dir exists");
            Self { path, cleanup_root }
        }

        fn path(&self) -> &Path {
            &self.path
        }

        fn write(&self, relative: &str, contents: &str) {
            let path = self.path.join(relative);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("parent directory exists");
            }
            fs::write(path, contents).expect("fixture written");
        }
    }

    impl Drop for TestSkillDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.cleanup_root);
        }
    }
}
