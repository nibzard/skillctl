//! Bundled `skillctl` management skill lifecycle support.

use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::{
    adapter::{AdapterRegistry, TargetRuntime, TargetScope},
    app::AppContext,
    cli::Scope,
    error::AppError,
    history::{HistoryEventKind, HistoryLedger},
    lifecycle::{self, LifecycleTransaction},
    manifest::ProjectionPolicy,
    materialize::PROJECTION_METADATA_FILE,
    overlay::NO_OVERLAY_HASH,
    planner::{self, ProjectionPlan},
    response::AppResponse,
    source::{SourceKind, compute_effective_version_hash, current_timestamp},
    state::{
        HistoryEntry, HistoryQuery, InstallRecord, LocalStateStore, ManagedScope, ManagedSkillRef,
        PinRecord, ProjectionMode, ProjectionRecord, default_state_database_path,
    },
};

const BUNDLED_SKILL_ID: &str = "skillctl";
const BUNDLED_SOURCE_URL: &str = "builtin://skillctl";
const BUNDLED_SOURCE_SUBPATH: &str = "skillctl";
const BUNDLED_ASSET_NAME: &str = "bundled-skillctl";
const EXPLICIT_REMOVE_REASON: &str = "explicit-remove";
const EXPLICIT_REMOVE_FIX: &str = "run skillctl --scope user enable skillctl";

const BUNDLED_SKILL_MARKDOWN: &str = include_str!("../assets/bundled/skillctl/SKILL.md");

const BUNDLED_FILES: &[BundledFile] = &[BundledFile {
    relative_path: "SKILL.md",
    contents: BUNDLED_SKILL_MARKDOWN,
}];

#[derive(Clone, Copy)]
struct BundledFile {
    relative_path: &'static str,
    contents: &'static str,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum RootStatus {
    Current,
    NeedsWrite,
    Conflict { message: String },
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RootInspection {
    root_display: String,
    root_path: PathBuf,
    targets: Vec<TargetRuntime>,
    projected_path: PathBuf,
    projected_display: String,
    status: RootStatus,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct BundledSkillDiagnostic {
    pub(crate) code: String,
    pub(crate) path: Option<String>,
    pub(crate) message: String,
    pub(crate) fix: Option<String>,
}

pub(crate) fn ensure_bundled_skill(context: &AppContext, force: bool) -> Result<(), AppError> {
    if !force && !bundled_mutation_required(context, force)? {
        return Ok(());
    }

    ensure_bundled_skill_with_transaction(context, force, "bundled-bootstrap")?;
    Ok(())
}

fn ensure_bundled_skill_with_transaction(
    context: &AppContext,
    force: bool,
    operation: &'static str,
) -> Result<bool, AppError> {
    lifecycle::run_transaction(operation, |transaction| {
        transaction.track_state_database()?;
        track_bundled_roots(context, transaction)?;
        let changed = ensure_bundled_skill_inner(context, force)?;
        if changed {
            transaction.checkpoint("after-state")?;
        }
        Ok(changed)
    })
}

fn ensure_bundled_skill_inner(context: &AppContext, force: bool) -> Result<bool, AppError> {
    let skill = bundled_skill_ref();
    let mut store = LocalStateStore::open_default()?;
    let existing = store.install_record(&skill)?;

    if let Some(record) = &existing {
        if !is_bundled_install(record) {
            let removed_paths = prune_owned_projections(context)?;
            return Ok(!removed_paths.is_empty());
        }
    } else if !force && is_explicitly_removed(&store)? {
        return Ok(false);
    }

    let timestamp = current_timestamp();
    let content_hash = bundled_content_hash();
    let effective_version_hash =
        compute_effective_version_hash(env!("CARGO_PKG_VERSION"), &content_hash, NO_OVERLAY_HASH);
    let mut inspections = inspect_roots(context)?;
    let manageable_roots = inspections
        .iter()
        .filter(|inspection| !matches!(inspection.status, RootStatus::Conflict { .. }))
        .count();
    if manageable_roots == 0 {
        return Ok(false);
    }

    let desired_install = install_record_from_state(
        existing.as_ref(),
        &skill,
        &content_hash,
        &effective_version_hash,
        &timestamp,
    );

    let install_changed = existing
        .as_ref()
        .is_none_or(|record| install_record_changed(record, &desired_install));

    let mut wrote_projection = false;
    for inspection in &mut inspections {
        if matches!(inspection.status, RootStatus::NeedsWrite) {
            write_projection(
                &inspection.projected_path,
                &inspection.root_display,
                &timestamp,
            )?;
            inspection.status = RootStatus::Current;
            wrote_projection = true;
        }
    }

    let desired_projections = projection_records_for_roots(&skill, &inspections, &timestamp);
    let current_projections = store.projection_records(Some(&skill))?;
    let projection_state_changed =
        !same_projection_records(&current_projections, &desired_projections);

    if !install_changed && !wrote_projection && !projection_state_changed {
        return Ok(false);
    }

    store.delete_current_skill_state(&skill)?;
    if install_changed {
        let mut ledger = HistoryLedger::new(&mut store);
        match existing.as_ref() {
            Some(record) => {
                ledger.record_update_applied(&record.resolved_revision, &desired_install)?;
            }
            None => {
                ledger.record_install(&desired_install)?;
            }
        }
    } else {
        store.upsert_install_record(&desired_install)?;
    }

    store.upsert_pin_record(&PinRecord {
        skill: skill.clone(),
        requested_reference: env!("CARGO_PKG_VERSION").to_string(),
        resolved_revision: env!("CARGO_PKG_VERSION").to_string(),
        effective_version_hash: Some(effective_version_hash),
        pinned_at: timestamp.clone(),
    })?;

    let mut ledger = HistoryLedger::new(&mut store);
    for projection in &desired_projections {
        ledger.record_projection(projection)?;
    }

    Ok(true)
}

pub(crate) fn handle_remove(context: &AppContext) -> Result<AppResponse, AppError> {
    lifecycle::run_transaction("bundled-remove", |transaction| {
        transaction.track_state_database()?;
        track_bundled_roots(context, transaction)?;

        let skill = bundled_skill_ref();
        let mut store = LocalStateStore::open_default()?;
        let timestamp = current_timestamp();
        let removed_paths = prune_owned_projections(context)?;

        store.delete_current_skill_state(&skill)?;

        let recorded_paths = if removed_paths.is_empty() {
            vec![BUNDLED_SOURCE_URL.to_string()]
        } else {
            removed_paths.clone()
        };
        for path in recorded_paths {
            store.append_history_entry(&HistoryEntry {
                id: None,
                kind: HistoryEventKind::Cleanup,
                scope: Some(skill.scope),
                skill_id: Some(skill.skill_id.clone()),
                target: None,
                occurred_at: timestamp.clone(),
                summary: format!("Removed bundled {} at {}", skill.skill_id, path),
                details: cleanup_details(&path),
            })?;
        }

        transaction.checkpoint("after-state")?;

        Ok(AppResponse::success("remove")
            .with_summary("Removed skillctl from user scope.".to_string())
            .with_data(json!({
                "skill": BUNDLED_SKILL_ID,
                "scope": ManagedScope::User.as_str(),
                "builtin": true,
                "removed_paths": removed_paths,
            })))
    })
}

pub(crate) fn handle_enable(context: &AppContext) -> Result<AppResponse, AppError> {
    let changed = ensure_bundled_skill_with_transaction(context, true, "bundled-enable")?;
    let projections = LocalStateStore::open_default()?
        .projection_records(Some(&bundled_skill_ref()))?
        .into_iter()
        .map(|projection| projection_view(context, &projection))
        .collect::<Result<Vec<_>, _>>()?;

    Ok(AppResponse::success("enable")
        .with_summary("Enabled skillctl in user scope.".to_string())
        .with_data(json!({
            "skill": BUNDLED_SKILL_ID,
            "scope": ManagedScope::User.as_str(),
            "builtin": true,
            "enabled": true,
            "changed": changed,
            "projections": projections,
        })))
}

fn bundled_mutation_required(context: &AppContext, force: bool) -> Result<bool, AppError> {
    let skill = bundled_skill_ref();
    let state_database = default_state_database_path()?;
    let store = match fs::metadata(&state_database) {
        Ok(_) => Some(LocalStateStore::open_at(&state_database)?),
        Err(source) if source.kind() == io::ErrorKind::NotFound => None,
        Err(source) => {
            return Err(AppError::FilesystemOperation {
                action: "inspect local state database",
                path: state_database,
                source,
            });
        }
    };
    let existing = store
        .as_ref()
        .map(|store| store.install_record(&skill))
        .transpose()?
        .flatten();

    if let Some(record) = &existing {
        if !is_bundled_install(record) {
            return Ok(!owned_projection_paths(context)?.is_empty());
        }
    } else if !force
        && let Some(store) = store.as_ref()
        && is_explicitly_removed(store)?
    {
        return Ok(false);
    }

    let content_hash = bundled_content_hash();
    let effective_version_hash =
        compute_effective_version_hash(env!("CARGO_PKG_VERSION"), &content_hash, NO_OVERLAY_HASH);
    let inspections = inspect_roots(context)?;
    let manageable_roots = inspections
        .iter()
        .filter(|inspection| !matches!(inspection.status, RootStatus::Conflict { .. }))
        .count();
    if manageable_roots == 0 {
        return Ok(false);
    }

    let desired_install = install_record_from_state(
        existing.as_ref(),
        &skill,
        &content_hash,
        &effective_version_hash,
        "",
    );
    let install_changed = existing
        .as_ref()
        .is_none_or(|record| install_record_changed(record, &desired_install));
    if inspections
        .iter()
        .any(|inspection| matches!(inspection.status, RootStatus::NeedsWrite))
    {
        return Ok(true);
    }

    let desired_projections = projection_records_for_roots(&skill, &inspections, "");
    let current_projections = store
        .as_ref()
        .map(|store| store.projection_records(Some(&skill)))
        .transpose()?
        .unwrap_or_default();

    Ok(install_changed || !same_projection_records(&current_projections, &desired_projections))
}

pub(crate) fn planned_root_views(context: &AppContext) -> Result<Vec<Value>, AppError> {
    let plan = bundled_projection_plan()?;
    plan.assignments
        .into_iter()
        .map(|assignment| {
            let root = planner::resolve_runtime_root_path(context, &assignment.root)?;
            let path = root.join(BUNDLED_SKILL_ID);
            Ok(json!({
                "target": assignment.target,
                "root": planner::display_path(context, &root),
                "path": planner::display_path(context, &path),
                "source": assignment.source,
            }))
        })
        .collect()
}

pub(crate) fn diagnostics(context: &AppContext) -> Result<Vec<BundledSkillDiagnostic>, AppError> {
    let store = LocalStateStore::open_default()?;
    if store.install_record(&bundled_skill_ref())?.is_none() && is_explicitly_removed(&store)? {
        return Ok(vec![BundledSkillDiagnostic {
            code: "bundled-skill-removed".to_string(),
            path: None,
            message: "the bundled skillctl skill was explicitly removed from user scope"
                .to_string(),
            fix: Some(EXPLICIT_REMOVE_FIX.to_string()),
        }]);
    }

    let mut diagnostics = Vec::new();
    for inspection in inspect_roots(context)? {
        if let RootStatus::Conflict { message } = inspection.status {
            diagnostics.push(BundledSkillDiagnostic {
                code: "bundled-skill-conflict".to_string(),
                path: Some(inspection.projected_display),
                message,
                fix: Some(EXPLICIT_REMOVE_FIX.to_string()),
            });
        }
    }

    Ok(diagnostics)
}

pub(crate) fn is_bundled_install(record: &InstallRecord) -> bool {
    record.skill.scope == ManagedScope::User
        && record.skill.skill_id == BUNDLED_SKILL_ID
        && record.source_url == BUNDLED_SOURCE_URL
}

pub(crate) fn is_bundled_request(skill: &str, scope: Option<Scope>) -> bool {
    skill == BUNDLED_SKILL_ID && !matches!(scope, Some(Scope::Workspace))
}

fn bundled_skill_ref() -> ManagedSkillRef {
    ManagedSkillRef::new(ManagedScope::User, BUNDLED_SKILL_ID)
}

fn bundled_projection_plan() -> Result<ProjectionPlan, AppError> {
    planner::plan_target_roots(
        &AdapterRegistry::new(),
        TargetScope::User,
        ProjectionPolicy::PreferNeutral,
        TargetRuntime::all(),
        &BTreeMap::new(),
    )
}

fn inspect_roots(context: &AppContext) -> Result<Vec<RootInspection>, AppError> {
    let plan = bundled_projection_plan()?;
    let targets_by_root: BTreeMap<_, _> = plan
        .physical_roots
        .into_iter()
        .map(|root| (root.path, root.targets))
        .collect();
    let mut inspections = Vec::new();

    for (root_display, targets) in targets_by_root {
        let root_path = planner::resolve_runtime_root_path(context, &root_display)?;
        let projected_path = root_path.join(BUNDLED_SKILL_ID);
        let projected_display = planner::display_path(context, &projected_path);
        let status = inspect_projection_root(&projected_path)?;
        inspections.push(RootInspection {
            root_display,
            root_path,
            targets,
            projected_path,
            projected_display,
            status,
        });
    }

    inspections.sort_by(|left, right| left.root_display.cmp(&right.root_display));
    Ok(inspections)
}

fn inspect_projection_root(path: &Path) -> Result<RootStatus, AppError> {
    match fs::metadata(path) {
        Ok(metadata) if !metadata.is_dir() => Ok(RootStatus::Conflict {
            message: format!(
                "the bundled skillctl skill could not be projected because '{}' is not a directory",
                path.display()
            ),
        }),
        Ok(_) => match builtin_projection_state(path)? {
            BundledProjectionState::ManagedCurrent => Ok(RootStatus::Current),
            BundledProjectionState::ManagedStale => Ok(RootStatus::NeedsWrite),
            BundledProjectionState::Conflict => Ok(RootStatus::Conflict {
                message: format!(
                    "the bundled skillctl skill is blocked by a hand-authored or user-managed \
skill directory at '{}'",
                    path.display()
                ),
            }),
        },
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(RootStatus::NeedsWrite),
        Err(source) => Err(AppError::FilesystemOperation {
            action: "inspect bundled skill projection",
            path: path.to_path_buf(),
            source,
        }),
    }
}

enum BundledProjectionState {
    ManagedCurrent,
    ManagedStale,
    Conflict,
}

fn builtin_projection_state(path: &Path) -> Result<BundledProjectionState, AppError> {
    let metadata_path = path.join(PROJECTION_METADATA_FILE);
    let contents = match fs::read_to_string(&metadata_path) {
        Ok(contents) => contents,
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            return Ok(BundledProjectionState::Conflict);
        }
        Err(source) => {
            return Err(AppError::FilesystemOperation {
                action: "read bundled projection metadata",
                path: metadata_path,
                source,
            });
        }
    };
    let parsed = match serde_json::from_str::<Value>(&contents) {
        Ok(parsed) => parsed,
        Err(_) => return Ok(BundledProjectionState::Conflict),
    };

    let is_bundled = parsed.get("tool").and_then(Value::as_str) == Some("skillctl")
        && parsed
            .get("source")
            .and_then(|source| source.get("kind"))
            .and_then(Value::as_str)
            == Some("bundled")
        && parsed
            .get("source")
            .and_then(|source| source.get("asset"))
            .and_then(Value::as_str)
            == Some(BUNDLED_SKILL_ID);
    if !is_bundled {
        return Ok(BundledProjectionState::Conflict);
    }

    if parsed
        .get("source")
        .and_then(|source| source.get("version"))
        .and_then(Value::as_str)
        == Some(env!("CARGO_PKG_VERSION"))
        && projected_root_matches_asset(path)?
    {
        Ok(BundledProjectionState::ManagedCurrent)
    } else {
        Ok(BundledProjectionState::ManagedStale)
    }
}

fn projected_root_matches_asset(path: &Path) -> Result<bool, AppError> {
    let mut actual_files = BTreeMap::new();
    collect_projected_files(path, Path::new(""), &mut actual_files)?;
    if actual_files.len() != BUNDLED_FILES.len() {
        return Ok(false);
    }

    for file in BUNDLED_FILES {
        let Some(actual) = actual_files.remove(file.relative_path) else {
            return Ok(false);
        };
        if actual != file.contents.as_bytes() {
            return Ok(false);
        }
    }

    Ok(actual_files.is_empty())
}

fn collect_projected_files(
    root: &Path,
    relative: &Path,
    files: &mut BTreeMap<String, Vec<u8>>,
) -> Result<(), AppError> {
    let mut entries = fs::read_dir(root)
        .map_err(|source| AppError::FilesystemOperation {
            action: "read bundled skill projection root",
            path: root.to_path_buf(),
            source,
        })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| AppError::FilesystemOperation {
            action: "read bundled skill projection entry",
            path: root.to_path_buf(),
            source,
        })?;
    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        let file_name = entry.file_name().to_string_lossy().into_owned();
        if relative.as_os_str().is_empty() && file_name == PROJECTION_METADATA_FILE {
            continue;
        }

        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|source| AppError::FilesystemOperation {
                action: "inspect bundled skill projection entry",
                path: path.clone(),
                source,
            })?;
        let next_relative = if relative.as_os_str().is_empty() {
            PathBuf::from(&file_name)
        } else {
            relative.join(&file_name)
        };

        if file_type.is_symlink() {
            return Ok(());
        }
        if file_type.is_dir() {
            collect_projected_files(&path, &next_relative, files)?;
            continue;
        }

        let bytes = fs::read(&path).map_err(|source| AppError::FilesystemOperation {
            action: "read bundled skill projection file",
            path,
            source,
        })?;
        files.insert(next_relative.to_string_lossy().into_owned(), bytes);
    }

    Ok(())
}

fn write_projection(
    target_dir: &Path,
    root_display: &str,
    generated_at: &str,
) -> Result<(), AppError> {
    match fs::metadata(target_dir) {
        Ok(metadata) if !metadata.is_dir() => {
            return Err(AppError::PathConflict {
                path: target_dir.to_path_buf(),
                expected: "directory",
            });
        }
        Ok(_) => {
            fs::remove_dir_all(target_dir).map_err(|source| AppError::FilesystemOperation {
                action: "replace bundled skill projection",
                path: target_dir.to_path_buf(),
                source,
            })?;
        }
        Err(source) if source.kind() == io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(AppError::FilesystemOperation {
                action: "inspect bundled skill projection",
                path: target_dir.to_path_buf(),
                source,
            });
        }
    }

    fs::create_dir_all(target_dir).map_err(|source| AppError::FilesystemOperation {
        action: "create bundled skill projection",
        path: target_dir.to_path_buf(),
        source,
    })?;

    for file in BUNDLED_FILES {
        let destination = target_dir.join(file.relative_path);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).map_err(|source| AppError::FilesystemOperation {
                action: "create bundled skill projection parent",
                path: parent.to_path_buf(),
                source,
            })?;
        }
        fs::write(&destination, file.contents).map_err(|source| AppError::FilesystemOperation {
            action: "write bundled skill projection file",
            path: destination,
            source,
        })?;
    }

    let metadata = json!({
        "tool": "skillctl",
        "version": env!("CARGO_PKG_VERSION"),
        "generated_at": generated_at,
        "generation_mode": "copy",
        "physical_root": root_display,
        "skill_name": BUNDLED_SKILL_ID,
        "source": {
            "kind": "bundled",
            "asset": BUNDLED_SKILL_ID,
            "version": env!("CARGO_PKG_VERSION"),
            "url": BUNDLED_SOURCE_URL,
        },
    });
    let serialized = serde_json::to_string_pretty(&metadata)?;
    fs::write(
        target_dir.join(PROJECTION_METADATA_FILE),
        format!("{serialized}\n"),
    )
    .map_err(|source| AppError::FilesystemOperation {
        action: "write bundled skill projection metadata",
        path: target_dir.join(PROJECTION_METADATA_FILE),
        source,
    })?;

    Ok(())
}

fn install_record_from_state(
    existing: Option<&InstallRecord>,
    skill: &ManagedSkillRef,
    content_hash: &str,
    effective_version_hash: &str,
    timestamp: &str,
) -> InstallRecord {
    InstallRecord {
        skill: skill.clone(),
        source_kind: SourceKind::LocalPath,
        source_url: BUNDLED_SOURCE_URL.to_string(),
        source_subpath: BUNDLED_SOURCE_SUBPATH.to_string(),
        resolved_revision: env!("CARGO_PKG_VERSION").to_string(),
        upstream_revision: None,
        content_hash: content_hash.to_string(),
        overlay_hash: NO_OVERLAY_HASH.to_string(),
        effective_version_hash: effective_version_hash.to_string(),
        installed_at: existing
            .map(|record| record.installed_at.clone())
            .unwrap_or_else(|| timestamp.to_string()),
        updated_at: timestamp.to_string(),
        detached: false,
        forked: false,
    }
}

fn install_record_changed(current: &InstallRecord, desired: &InstallRecord) -> bool {
    current.source_url != desired.source_url
        || current.source_subpath != desired.source_subpath
        || current.resolved_revision != desired.resolved_revision
        || current.content_hash != desired.content_hash
        || current.overlay_hash != desired.overlay_hash
        || current.effective_version_hash != desired.effective_version_hash
        || current.detached != desired.detached
        || current.forked != desired.forked
}

fn projection_records_for_roots(
    skill: &ManagedSkillRef,
    inspections: &[RootInspection],
    generated_at: &str,
) -> Vec<ProjectionRecord> {
    let mut records = Vec::new();

    for inspection in inspections {
        if !matches!(inspection.status, RootStatus::Current) {
            continue;
        }

        for target in &inspection.targets {
            records.push(ProjectionRecord {
                skill: skill.clone(),
                target: *target,
                generation_mode: ProjectionMode::Copy,
                physical_root: inspection.root_display.clone(),
                projected_path: BUNDLED_SKILL_ID.to_string(),
                effective_version_hash: compute_effective_version_hash(
                    env!("CARGO_PKG_VERSION"),
                    &bundled_content_hash(),
                    NO_OVERLAY_HASH,
                ),
                generated_at: generated_at.to_string(),
            });
        }
    }

    records.sort_by(|left, right| {
        left.target
            .cmp(&right.target)
            .then_with(|| left.physical_root.cmp(&right.physical_root))
    });
    records
}

fn same_projection_records(current: &[ProjectionRecord], desired: &[ProjectionRecord]) -> bool {
    let mut current = current.iter().map(projection_signature).collect::<Vec<_>>();
    let mut desired = desired.iter().map(projection_signature).collect::<Vec<_>>();
    current.sort();
    desired.sort();
    current == desired
}

fn projection_signature(record: &ProjectionRecord) -> (TargetRuntime, String, String, String) {
    (
        record.target,
        record.physical_root.clone(),
        record.projected_path.clone(),
        record.effective_version_hash.clone(),
    )
}

fn track_bundled_roots(
    context: &AppContext,
    transaction: &mut LifecycleTransaction,
) -> Result<(), AppError> {
    for (_, root_path) in all_user_roots(context)? {
        transaction.track_path(root_path)?;
    }

    Ok(())
}

fn prune_owned_projections(context: &AppContext) -> Result<Vec<String>, AppError> {
    let mut removed = owned_projection_paths(context)?
        .into_iter()
        .map(|path| {
            fs::remove_dir_all(&path).map_err(|source| AppError::FilesystemOperation {
                action: "remove bundled skill projection",
                path: path.clone(),
                source,
            })?;
            Ok(planner::display_path(context, &path))
        })
        .collect::<Result<Vec<_>, AppError>>()?;

    removed.sort();
    Ok(removed)
}

fn owned_projection_paths(context: &AppContext) -> Result<Vec<PathBuf>, AppError> {
    let mut owned = Vec::new();

    for (_, root_path) in all_user_roots(context)? {
        let target_dir = root_path.join(BUNDLED_SKILL_ID);
        match builtin_projection_state(&target_dir) {
            Ok(BundledProjectionState::ManagedCurrent | BundledProjectionState::ManagedStale) => {
                owned.push(target_dir);
            }
            Ok(BundledProjectionState::Conflict) => {}
            Err(AppError::FilesystemOperation {
                action: "read bundled projection metadata",
                path: _,
                source,
            }) if source.kind() == io::ErrorKind::NotFound => {}
            Err(AppError::FilesystemOperation {
                action: "inspect bundled skill projection",
                path: _,
                source,
            }) if source.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }

    owned.sort();
    Ok(owned)
}

fn all_user_roots(context: &AppContext) -> Result<Vec<(String, PathBuf)>, AppError> {
    let mut roots = BTreeMap::new();
    for adapter in AdapterRegistry::new().all() {
        for root in adapter.roots_for_scope(TargetScope::User) {
            roots
                .entry(root.path.to_string())
                .or_insert(planner::resolve_runtime_root_path(context, root.path)?);
        }
    }
    Ok(roots.into_iter().collect())
}

fn cleanup_details(path: &str) -> BTreeMap<String, Value> {
    let mut details = BTreeMap::new();
    details.insert("path".to_string(), json!(path));
    details.insert("reason".to_string(), json!(EXPLICIT_REMOVE_REASON));
    details.insert("managed_asset".to_string(), json!(BUNDLED_ASSET_NAME));
    details
}

fn is_explicitly_removed(store: &LocalStateStore) -> Result<bool, AppError> {
    let entries = store.history_entries(&HistoryQuery::for_skill(bundled_skill_ref()))?;
    let Some(entry) = entries.first() else {
        return Ok(false);
    };

    Ok(entry.kind == HistoryEventKind::Cleanup
        && entry.details.get("reason").and_then(Value::as_str) == Some(EXPLICIT_REMOVE_REASON)
        && entry.details.get("managed_asset").and_then(Value::as_str) == Some(BUNDLED_ASSET_NAME))
}

fn bundled_content_hash() -> String {
    let mut hasher = Sha256::new();
    for file in BUNDLED_FILES {
        hasher.update(file.relative_path.as_bytes());
        hasher.update([0]);
        hasher.update(file.contents.as_bytes());
        hasher.update([0]);
    }
    format!("{:x}", hasher.finalize())
}

fn projection_view(context: &AppContext, projection: &ProjectionRecord) -> Result<Value, AppError> {
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
