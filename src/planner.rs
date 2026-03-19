//! Planning domain types and update entry points.

use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs, io,
    path::{Path, PathBuf},
    process::Command,
};

use serde::Serialize;
use serde_json::json;

use crate::{
    adapter::{AdapterRegistry, TargetRuntime, TargetScope},
    app::AppContext,
    error::AppError,
    history::HistoryLedger,
    lockfile::WorkspaceLockfile,
    manifest::{AdapterOverride, AdapterRoot, ProjectionPolicy, WorkspaceManifest},
    materialize::PROJECTION_METADATA_FILE,
    overlay::{NO_OVERLAY_HASH, hash_overlay_root},
    resolver::{self, ResolveWorkspaceRequest, ResolvedSkillCandidate, SkillScope},
    response::AppResponse,
    source::{SourceKind, current_timestamp, imports_store_root},
    state::{
        InstallRecord, LocalModificationKind, LocalModificationRecord, LocalStateStore,
        ManagedScope, ManagedSkillRef, PinRecord, ProjectionRecord, UpdateCheckOutcome,
        UpdateCheckRecord,
    },
    telemetry,
    trust::SkillTrust,
};

/// Reusable projection-root plan shared by sync, doctor, explain, and JSON output.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ProjectionPlan {
    /// Scope being planned.
    pub scope: TargetScope,
    /// Policy that selected among equally compatible roots.
    pub policy: ProjectionPolicy,
    /// Per-target root assignments.
    pub assignments: Vec<TargetRootAssignment>,
    /// Physical roots required by the selected assignments.
    pub physical_roots: Vec<PhysicalRootPlan>,
}

impl ProjectionPlan {
    /// Return the selected root for one runtime, if present in the plan.
    pub fn root_for(&self, target: TargetRuntime) -> Option<&str> {
        self.assignments
            .iter()
            .find(|assignment| assignment.target == target)
            .map(|assignment| assignment.root.as_str())
    }
}

/// Backwards-compatible alias for call sites that want to emphasize root planning.
pub type TargetRootPlan = ProjectionPlan;

/// One target's selected discovery root.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct TargetRootAssignment {
    /// Target runtime receiving this root.
    pub target: TargetRuntime,
    /// Root path chosen for the target.
    pub root: String,
    /// Whether the root came from the planner or an explicit override.
    pub source: RootSelectionSource,
}

/// Group of runtimes that share one physical root.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PhysicalRootPlan {
    /// Shared root path.
    pub path: String,
    /// Targets satisfied by this physical root.
    pub targets: Vec<TargetRuntime>,
}

/// Source of a root selection inside the planner.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RootSelectionSource {
    /// The planner selected a documented root automatically.
    Planner,
    /// The manifest supplied an explicit path override.
    Override,
}

/// Typed request for `skillctl update`.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct UpdateRequest {
    /// Optional managed skill filter.
    pub skill: Option<String>,
}

impl UpdateRequest {
    /// Create an update request from parsed CLI arguments.
    pub fn new(skill: Option<String>) -> Self {
        Self { skill }
    }
}

/// Handle `skillctl update`.
pub fn handle_update(
    context: &AppContext,
    request: UpdateRequest,
) -> Result<AppResponse, AppError> {
    let manifest = WorkspaceManifest::load_from_workspace(&context.working_directory)?;
    let lockfile = WorkspaceLockfile::load_from_workspace(&context.working_directory)?;
    let mut store = LocalStateStore::open_default_for(&context.working_directory)?;
    let selected_skills = select_managed_skills(&store, context, &request)?;
    if selected_skills.is_empty() {
        return Ok(AppResponse::success("update")
            .with_summary("No managed skills are installed.")
            .with_data(json!({ "plans": [] })));
    }

    let candidate_map = imported_candidate_map(context, &manifest, &lockfile)?;
    let checked_at = current_timestamp();
    let mut plans = Vec::with_capacity(selected_skills.len());

    for managed_skill in selected_skills {
        let snapshot = store.skill_snapshot(&managed_skill)?;
        let install = snapshot
            .install
            .as_ref()
            .ok_or_else(|| AppError::ResolutionValidation {
                message: format!(
                    "skill '{}' does not have an installed state record",
                    managed_skill.skill_id
                ),
            })?;
        let prepared = prepare_update_plan(
            context,
            &manifest,
            install,
            snapshot.pin.as_ref(),
            &snapshot.projections,
            candidate_map.get(&(managed_skill.scope, managed_skill.skill_id.clone())),
            &checked_at,
        )?;

        {
            let mut ledger = HistoryLedger::new(&mut store);
            ledger.record_update_check_with_trust(
                &prepared.update_check,
                prepared.plan.trust.as_ref(),
            )?;
            for modification in &prepared.local_modification_records {
                ledger.record_local_modification(modification)?;
            }
        }

        plans.push(prepared.plan);
    }

    let telemetry = telemetry::prepare_update_report(context, &plans)?;
    let mut summary = update_summary(&plans);
    if let Some(notice) = telemetry.notice_message() {
        summary.push('\n');
        summary.push_str(notice);
    }
    let mut response = AppResponse::success("update")
        .with_summary(summary)
        .with_data(json!({
            "plans": plans,
            "telemetry": telemetry,
        }));
    let mut warnings = BTreeSet::new();
    for plan in &plans {
        if let Some(trust) = &plan.trust {
            for warning in &trust.warnings {
                warnings.insert(warning.clone());
            }
        }
    }
    for warning in warnings {
        response = response.with_warning(warning);
    }

    Ok(response)
}

/// Planner recommendation for how to respond to one update result.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum UpdateAction {
    /// Safe to apply the upstream update.
    Apply,
    /// Redirect the user into the managed overlay workflow.
    CreateOverlay,
    /// Detach into local canonical ownership.
    Detach,
    /// Publish the customized variant to a dedicated repository.
    PublishVariant,
    /// Keep the current pinned version and defer action.
    Skip,
}

/// Stable source metadata included in an update plan.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct UpdateSourceSummary {
    /// Installed source category.
    #[serde(rename = "type")]
    pub kind: SourceKind,
    /// Normalized source URL or file URL.
    pub url: String,
    /// Selected relative subpath inside the source.
    pub subpath: String,
}

/// One detected local modification that affects update safety.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PlannedModification {
    /// Classified local change kind.
    pub kind: LocalModificationKind,
    /// Whether the change is represented by a managed workflow.
    pub managed: bool,
    /// Path involved in the drift, when available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Deterministic explanatory detail.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<String>,
}

/// Structured update-check result for one managed skill.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SkillUpdatePlan {
    /// Managed skill identifier.
    pub skill: String,
    /// Owning scope for the skill.
    pub scope: ManagedScope,
    /// Timestamp shared by the update run.
    pub checked_at: String,
    /// Installed source summary.
    pub source: UpdateSourceSummary,
    /// Currently pinned revision evaluated by the check.
    pub pinned_revision: String,
    /// Latest observed upstream revision, when one exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest_revision: Option<String>,
    /// Planner-relevant update outcome.
    pub outcome: UpdateCheckOutcome,
    /// Whether a managed overlay existed during the check.
    pub overlay_detected: bool,
    /// Whether unmanaged local changes were detected.
    pub local_modification_detected: bool,
    /// Next action the user or agent should take.
    pub recommended_action: UpdateAction,
    /// Deterministic set of valid next actions.
    pub available_actions: Vec<UpdateAction>,
    /// Structured local-change details discovered during the check.
    pub modifications: Vec<PlannedModification>,
    /// Trust decision for the effective skill during this update check.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trust: Option<SkillTrust>,
    /// Additional explanatory notes for humans and agents.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

struct PreparedUpdatePlan {
    plan: SkillUpdatePlan,
    update_check: UpdateCheckRecord,
    local_modification_records: Vec<LocalModificationRecord>,
}

#[derive(Clone)]
struct OverlayState {
    path: String,
    changed_since_recorded_state: bool,
}

fn prepare_update_plan(
    context: &AppContext,
    manifest: &WorkspaceManifest,
    install: &InstallRecord,
    pin: Option<&PinRecord>,
    projections: &[ProjectionRecord],
    candidate: Option<&ResolvedSkillCandidate>,
    checked_at: &str,
) -> Result<PreparedUpdatePlan, AppError> {
    let mut modifications = Vec::new();
    let mut local_modification_records = Vec::new();
    let mut notes = Vec::new();
    let mut trust = match candidate {
        Some(candidate) => crate::trust::trust_for_candidate(candidate),
        None => crate::trust::trust_for_install_record(install, false, None)?,
    };

    if install.detached || install.forked {
        let detail = if install.forked {
            "skill is forked into local ownership and no longer tracks upstream updates"
        } else {
            "skill is detached from upstream lifecycle management"
        };
        notes.push(detail.to_string());
        local_modification_records.push(LocalModificationRecord {
            id: None,
            skill: install.skill.clone(),
            detected_at: checked_at.to_string(),
            kind: LocalModificationKind::DetachedFork,
            path: None,
            details: Some(detail.to_string()),
        });
        modifications.push(PlannedModification {
            kind: LocalModificationKind::DetachedFork,
            managed: false,
            path: None,
            details: Some(detail.to_string()),
        });
        let notes = deduplicate_notes(notes);
        let note_text = if notes.is_empty() {
            None
        } else {
            Some(notes.join(" "))
        };

        let update_check = UpdateCheckRecord {
            id: None,
            skill: install.skill.clone(),
            checked_at: checked_at.to_string(),
            pinned_revision: install.resolved_revision.clone(),
            latest_revision: install.upstream_revision.clone(),
            outcome: UpdateCheckOutcome::Detached,
            overlay_detected: false,
            local_modification_detected: true,
            notes: note_text,
        };

        return Ok(PreparedUpdatePlan {
            plan: SkillUpdatePlan {
                skill: install.skill.skill_id.clone(),
                scope: install.skill.scope,
                checked_at: checked_at.to_string(),
                source: UpdateSourceSummary {
                    kind: install.source_kind,
                    url: install.source_url.clone(),
                    subpath: install.source_subpath.clone(),
                },
                pinned_revision: install.resolved_revision.clone(),
                latest_revision: install.upstream_revision.clone(),
                outcome: UpdateCheckOutcome::Detached,
                overlay_detected: false,
                local_modification_detected: true,
                recommended_action: UpdateAction::Skip,
                available_actions: vec![UpdateAction::Skip],
                modifications,
                trust: Some(trust),
                notes,
            },
            update_check,
            local_modification_records,
        });
    }

    let overlay_state = overlay_state(context, manifest, install)?;

    if let Some(overlay_state) = &overlay_state {
        let details = if overlay_state.changed_since_recorded_state {
            "managed overlay changed since the last recorded install state".to_string()
        } else {
            "managed overlay will be preserved during updates".to_string()
        };
        modifications.push(PlannedModification {
            kind: LocalModificationKind::Overlay,
            managed: true,
            path: Some(overlay_state.path.clone()),
            details: Some(details.clone()),
        });
        notes.push(details);
    }

    let pinned_revision = install.resolved_revision.clone();
    let overlay_detected = overlay_state.is_some();
    let latest_revision;
    let outcome;
    let mut recommended_action;
    let mut available_actions;

    match install.source_kind {
        SourceKind::LocalPath | SourceKind::Archive => {
            latest_revision = None;
            outcome = UpdateCheckOutcome::LocalSource;
            recommended_action = UpdateAction::Skip;
            available_actions = vec![UpdateAction::Skip];
            notes.push("local sources do not support upstream update checks".to_string());
        }
        SourceKind::Git => {
            let requested_reference = active_requested_git_reference(install, pin, candidate);
            let upstream_result =
                latest_git_revision(&install.source_url, requested_reference, &pinned_revision);
            match upstream_result {
                Ok(upstream_revision) => {
                    latest_revision = Some(upstream_revision.clone());
                    if overlay_state
                        .as_ref()
                        .is_some_and(|state| state.changed_since_recorded_state)
                    {
                        notes.push(
                            "projection copies may be stale because the managed overlay changed"
                                .to_string(),
                        );
                    } else {
                        let projection_modifications = detect_projected_copy_modifications(
                            context,
                            install,
                            projections,
                            candidate,
                        )?;
                        for modification in projection_modifications {
                            local_modification_records.push(LocalModificationRecord {
                                id: None,
                                skill: install.skill.clone(),
                                detected_at: checked_at.to_string(),
                                kind: modification.kind,
                                path: modification.path.clone(),
                                details: modification.details.clone(),
                            });
                            modifications.push(modification);
                        }
                    }

                    let update_available = upstream_revision != pinned_revision;
                    let local_modification_detected = modifications
                        .iter()
                        .any(|modification| !modification.managed);

                    if update_available && local_modification_detected {
                        outcome = UpdateCheckOutcome::Blocked;
                        recommended_action = UpdateAction::CreateOverlay;
                        available_actions = vec![
                            UpdateAction::CreateOverlay,
                            UpdateAction::Detach,
                            UpdateAction::PublishVariant,
                            UpdateAction::Skip,
                        ];
                        notes.push(format!(
                            "unmanaged projected-copy edits are blocking an update from {} to {}",
                            pinned_revision, upstream_revision
                        ));
                    } else if update_available {
                        outcome = UpdateCheckOutcome::UpdateAvailable;
                        recommended_action = UpdateAction::Apply;
                        available_actions = vec![UpdateAction::Apply, UpdateAction::Skip];
                        notes.push(format!(
                            "upstream revision {} is newer than the pinned revision {}",
                            upstream_revision, pinned_revision
                        ));
                    } else {
                        outcome = UpdateCheckOutcome::UpToDate;
                        if local_modification_detected {
                            recommended_action = UpdateAction::CreateOverlay;
                            available_actions = vec![
                                UpdateAction::CreateOverlay,
                                UpdateAction::Detach,
                                UpdateAction::PublishVariant,
                                UpdateAction::Skip,
                            ];
                            notes.push(
                                    "no upstream update is available, but unmanaged projected-copy edits were detected"
                                        .to_string(),
                                );
                        } else {
                            recommended_action = UpdateAction::Skip;
                            available_actions = vec![UpdateAction::Skip];
                            notes.push("pinned revision already matches upstream".to_string());
                        }
                    }

                    if update_available && !local_modification_detected {
                        trust = trust.block_apply_update(install.skill.skill_id.as_str());
                        if !trust.blocked_actions.is_empty() {
                            recommended_action = UpdateAction::Skip;
                            available_actions = vec![UpdateAction::Skip];
                            notes.push(format!(
                                "trust gate is blocking apply for '{}' until the import is reviewed or forked",
                                install.skill.skill_id
                            ));
                        }
                    }
                }
                Err(message) => {
                    latest_revision = None;
                    outcome = UpdateCheckOutcome::Failed;
                    recommended_action = UpdateAction::Skip;
                    available_actions = vec![UpdateAction::Skip];
                    notes.push(message);
                }
            }
        }
    }

    let local_modification_detected = modifications
        .iter()
        .any(|modification| !modification.managed);
    let notes = deduplicate_notes(notes);
    let note_text = if notes.is_empty() {
        None
    } else {
        Some(notes.join(" "))
    };
    let update_check = UpdateCheckRecord {
        id: None,
        skill: install.skill.clone(),
        checked_at: checked_at.to_string(),
        pinned_revision: pinned_revision.clone(),
        latest_revision: latest_revision.clone(),
        outcome,
        overlay_detected,
        local_modification_detected,
        notes: note_text,
    };

    Ok(PreparedUpdatePlan {
        plan: SkillUpdatePlan {
            skill: install.skill.skill_id.clone(),
            scope: install.skill.scope,
            checked_at: checked_at.to_string(),
            source: UpdateSourceSummary {
                kind: install.source_kind,
                url: install.source_url.clone(),
                subpath: install.source_subpath.clone(),
            },
            pinned_revision,
            latest_revision,
            outcome,
            overlay_detected,
            local_modification_detected,
            recommended_action,
            available_actions,
            modifications,
            trust: Some(trust),
            notes,
        },
        update_check,
        local_modification_records,
    })
}

fn select_managed_skills(
    store: &LocalStateStore,
    context: &AppContext,
    request: &UpdateRequest,
) -> Result<Vec<ManagedSkillRef>, AppError> {
    let requested_skill = match (&request.skill, &context.selector.skill_name) {
        (Some(positional), Some(global)) if positional != global => {
            return Err(AppError::PlannerValidation {
                message: format!(
                    "conflicting skill selectors '{}'(argument) and '{}'(--name)",
                    positional, global
                ),
            });
        }
        (Some(positional), _) => Some(positional.clone()),
        (None, Some(global)) => Some(global.clone()),
        (None, None) => None,
    };
    let requested_scope = context.selector.scope.map(managed_scope);

    let mut installs = store.list_install_records()?;
    installs.retain(|record| requested_scope.is_none_or(|scope| record.skill.scope == scope));

    if let Some(skill_id) = requested_skill {
        let matches: Vec<_> = installs
            .into_iter()
            .filter(|record| record.skill.skill_id == skill_id)
            .map(|record| record.skill)
            .collect();
        return match matches.len() {
            0 => Err(AppError::ResolutionValidation {
                message: format!("skill '{}' is not installed", skill_id),
            }),
            1 => Ok(matches),
            _ => Err(AppError::ResolutionValidation {
                message: format!(
                    "skill '{}' exists in multiple scopes; re-run with --scope",
                    skill_id
                ),
            }),
        };
    }

    Ok(installs.into_iter().map(|record| record.skill).collect())
}

fn imported_candidate_map(
    context: &AppContext,
    manifest: &WorkspaceManifest,
    lockfile: &WorkspaceLockfile,
) -> Result<BTreeMap<(ManagedScope, String), ResolvedSkillCandidate>, AppError> {
    let request = ResolveWorkspaceRequest::new(
        &context.working_directory,
        imports_store_root()?,
        manifest.clone(),
        lockfile.clone(),
    );
    let graph = resolver::build_effective_skill_graph(&request)?;
    let mut candidates = BTreeMap::new();

    for candidate in graph.candidates {
        let Some(import) = candidate.import.as_ref() else {
            continue;
        };
        let scope = managed_scope_from_skill_scope(import.scope);
        candidates.insert((scope, import.id.clone()), candidate);
    }

    Ok(candidates)
}

fn overlay_state(
    context: &AppContext,
    manifest: &WorkspaceManifest,
    install: &InstallRecord,
) -> Result<Option<OverlayState>, AppError> {
    let overlay_path = manifest.overlay_path_for(&install.skill.skill_id);
    let overlay_root = context.working_directory.join(overlay_path.as_str());
    let current_hash = hash_overlay_root(&overlay_root)?;
    if current_hash == NO_OVERLAY_HASH && install.overlay_hash == NO_OVERLAY_HASH {
        return Ok(None);
    }

    Ok(Some(OverlayState {
        path: display_path(context, &overlay_root),
        changed_since_recorded_state: current_hash != install.overlay_hash,
    }))
}

fn active_requested_git_reference<'a>(
    install: &InstallRecord,
    pin: Option<&'a PinRecord>,
    candidate: Option<&'a ResolvedSkillCandidate>,
) -> Option<&'a str> {
    candidate
        .and_then(|candidate| candidate.import.as_ref())
        .filter(|import| import.resolved_revision == install.resolved_revision)
        .map(|import| import.requested_ref.as_str())
        .or_else(|| {
            pin.filter(|pin| pin.resolved_revision == install.resolved_revision)
                .map(|pin| pin.requested_reference.as_str())
        })
}

fn latest_git_revision(
    source_url: &str,
    requested_reference: Option<&str>,
    pinned_revision: &str,
) -> Result<String, String> {
    let reference = requested_reference.unwrap_or("HEAD");
    let output = Command::new("git")
        .args(["ls-remote", source_url, reference])
        .output()
        .map_err(|source| format!("failed to run git ls-remote for '{source_url}': {source}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let detail = if stderr.is_empty() {
            "git ls-remote returned a non-zero exit status".to_string()
        } else {
            stderr
        };
        return Err(format!(
            "failed to check upstream revision for '{source_url}': {detail}"
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_ls_remote_revision(stdout.as_ref())
        .or_else(|| {
            is_pinned_revision_reference(reference, pinned_revision)
                .then(|| pinned_revision.to_string())
        })
        .ok_or_else(|| {
            if reference == "HEAD" {
                format!("upstream revision lookup for '{source_url}' returned no HEAD")
            } else {
                format!(
                    "upstream revision lookup for '{source_url}' returned no revision for '{reference}'"
                )
            }
        })
}

fn parse_ls_remote_revision(stdout: &str) -> Option<String> {
    let mut revision = None;
    let mut peeled_tag_revision = None;

    for line in stdout.lines() {
        let mut parts = line.split_whitespace();
        let candidate_revision = parts.next()?;
        let candidate_ref = parts.next()?;
        if candidate_ref.ends_with("^{}") {
            peeled_tag_revision = Some(candidate_revision.to_string());
        } else if revision.is_none() {
            revision = Some(candidate_revision.to_string());
        }
    }

    peeled_tag_revision.or(revision)
}

fn is_pinned_revision_reference(reference: &str, pinned_revision: &str) -> bool {
    !reference.is_empty()
        && reference.len() <= pinned_revision.len()
        && pinned_revision.starts_with(reference)
        && reference.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn detect_projected_copy_modifications(
    context: &AppContext,
    install: &InstallRecord,
    projections: &[ProjectionRecord],
    candidate: Option<&ResolvedSkillCandidate>,
) -> Result<Vec<PlannedModification>, AppError> {
    let Some(candidate) = candidate else {
        return Ok(Vec::new());
    };

    let mut checked_roots = BTreeSet::new();
    let mut modifications = Vec::new();

    for projection in projections {
        if projection.effective_version_hash != install.effective_version_hash {
            continue;
        }

        let projection_root = resolve_runtime_root_path(context, &projection.physical_root)?
            .join(&projection.projected_path);
        if !checked_roots.insert(projection_root.clone()) {
            continue;
        }

        if let Some(path) = first_projection_difference(context, &projection_root, candidate)? {
            modifications.push(PlannedModification {
                kind: LocalModificationKind::ProjectedCopy,
                managed: false,
                path: Some(path.clone()),
                details: Some(
                    "projected runtime copy differs from the recorded effective skill".to_string(),
                ),
            });
        }
    }

    Ok(modifications)
}

fn first_projection_difference(
    context: &AppContext,
    projection_root: &Path,
    candidate: &ResolvedSkillCandidate,
) -> Result<Option<String>, AppError> {
    let mut expected = BTreeMap::new();
    for file in &candidate.files {
        if file.relative_path == Path::new(PROJECTION_METADATA_FILE) {
            continue;
        }
        expected.insert(file.relative_path.clone(), file.source_path.clone());
    }

    let metadata = match fs::metadata(projection_root) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            return Ok(Some(display_path(context, projection_root)));
        }
        Err(source) => {
            return Err(AppError::FilesystemOperation {
                action: "inspect projected skill directory",
                path: projection_root.to_path_buf(),
                source,
            });
        }
    };
    if !metadata.is_dir() {
        return Ok(Some(display_path(context, projection_root)));
    }

    let mut actual = Vec::new();
    collect_projection_files(projection_root, projection_root, &mut actual)?;
    actual.sort();

    for relative_path in &actual {
        if relative_path == Path::new(PROJECTION_METADATA_FILE) {
            continue;
        }

        let actual_path = projection_root.join(relative_path);
        let Some(expected_path) = expected.remove(relative_path) else {
            return Ok(Some(display_path(context, &actual_path)));
        };

        if !file_contents_equal(&actual_path, &expected_path)? {
            return Ok(Some(display_path(context, &actual_path)));
        }
    }

    if let Some((relative_path, _)) = expected.into_iter().next() {
        return Ok(Some(display_path(
            context,
            &projection_root.join(relative_path),
        )));
    }

    Ok(None)
}

fn collect_projection_files(
    root: &Path,
    current: &Path,
    files: &mut Vec<PathBuf>,
) -> Result<(), AppError> {
    let mut entries = fs::read_dir(current)
        .map_err(|source| AppError::FilesystemOperation {
            action: "read projected skill directory",
            path: current.to_path_buf(),
            source,
        })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| AppError::FilesystemOperation {
            action: "read projected skill entry",
            path: current.to_path_buf(),
            source,
        })?;
    entries.sort_by_key(std::fs::DirEntry::path);

    for entry in entries {
        let path = entry.path();
        let metadata =
            fs::symlink_metadata(&path).map_err(|source| AppError::FilesystemOperation {
                action: "inspect projected skill path",
                path: path.clone(),
                source,
            })?;

        if metadata.is_dir() {
            collect_projection_files(root, &path, files)?;
            continue;
        }

        if !metadata.is_file() {
            files.push(
                path.strip_prefix(root)
                    .expect("projection entry remains under the root")
                    .to_path_buf(),
            );
            continue;
        }

        files.push(
            path.strip_prefix(root)
                .expect("projection entry remains under the root")
                .to_path_buf(),
        );
    }

    Ok(())
}

fn file_contents_equal(left: &Path, right: &Path) -> Result<bool, AppError> {
    let left_contents = fs::read(left).map_err(|source| AppError::FilesystemOperation {
        action: "read projected file",
        path: left.to_path_buf(),
        source,
    })?;
    let right_contents = fs::read(right).map_err(|source| AppError::FilesystemOperation {
        action: "read effective source file",
        path: right.to_path_buf(),
        source,
    })?;
    Ok(left_contents == right_contents)
}

pub(crate) fn resolve_runtime_root_path(
    context: &AppContext,
    root: &str,
) -> Result<PathBuf, AppError> {
    if let Some(suffix) = root.strip_prefix("~/") {
        return Ok(home_directory()?.join(suffix));
    }
    if root == "~" {
        return home_directory();
    }

    let path = PathBuf::from(root);
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(context.working_directory.join(path))
    }
}

fn home_directory() -> Result<PathBuf, AppError> {
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .ok_or(AppError::HomeDirectoryUnavailable)
}

pub(crate) fn display_path(context: &AppContext, path: &Path) -> String {
    path.strip_prefix(&context.working_directory).map_or_else(
        |_| path.display().to_string(),
        |relative| relative.display().to_string(),
    )
}

fn deduplicate_notes(notes: Vec<String>) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut deduplicated = Vec::new();
    for note in notes {
        if seen.insert(note.clone()) {
            deduplicated.push(note);
        }
    }
    deduplicated
}

fn managed_scope(scope: crate::cli::Scope) -> ManagedScope {
    match scope {
        crate::cli::Scope::Workspace => ManagedScope::Workspace,
        crate::cli::Scope::User => ManagedScope::User,
    }
}

fn managed_scope_from_skill_scope(scope: SkillScope) -> ManagedScope {
    match scope {
        SkillScope::Workspace => ManagedScope::Workspace,
        SkillScope::User => ManagedScope::User,
    }
}

fn update_summary(plans: &[SkillUpdatePlan]) -> String {
    let update_available = plans
        .iter()
        .filter(|plan| plan.outcome == UpdateCheckOutcome::UpdateAvailable)
        .count();
    let blocked = plans
        .iter()
        .filter(|plan| plan.outcome == UpdateCheckOutcome::Blocked)
        .count();
    let up_to_date = plans
        .iter()
        .filter(|plan| plan.outcome == UpdateCheckOutcome::UpToDate)
        .count();
    let detached = plans
        .iter()
        .filter(|plan| plan.outcome == UpdateCheckOutcome::Detached)
        .count();
    let local = plans
        .iter()
        .filter(|plan| plan.outcome == UpdateCheckOutcome::LocalSource)
        .count();
    let failed = plans
        .iter()
        .filter(|plan| plan.outcome == UpdateCheckOutcome::Failed)
        .count();

    format!(
        "Checked {} managed skill{}. {} update available, {} blocked, {} up to date, {} detached, {} local source, {} failed.",
        plans.len(),
        plural_suffix(plans.len()),
        update_available,
        blocked,
        up_to_date,
        detached,
        local,
        failed,
    )
}

fn plural_suffix(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

/// Compute a deterministic projection-root plan for the selected runtimes.
pub fn plan_target_roots(
    registry: &AdapterRegistry,
    scope: TargetScope,
    policy: ProjectionPolicy,
    targets: &[TargetRuntime],
    overrides: &BTreeMap<TargetRuntime, AdapterOverride>,
) -> Result<TargetRootPlan, AppError> {
    let normalized_targets = normalize_targets(targets)?;
    let candidates = normalized_targets
        .iter()
        .map(|target| candidate_roots_for_target(registry, *target, scope, policy, overrides))
        .collect::<Result<Vec<_>, _>>()?;

    let mut current = Vec::with_capacity(candidates.len());
    let mut best: Option<(PlanScore, Vec<CandidateAssignment>)> = None;
    enumerate_candidate_plans(&candidates, 0, &mut current, &mut best);

    let Some((_, assignments)) = best else {
        return Err(AppError::PlannerValidation {
            message: format!("no documented roots support scope '{}'", scope.as_str()),
        });
    };

    Ok(build_projection_plan(scope, policy, assignments))
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CandidateAssignment {
    target: TargetRuntime,
    root: String,
    source: RootSelectionSource,
    rank: u16,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct PlanScore {
    root_count: usize,
    policy_rank: usize,
    unique_roots: Vec<String>,
    assignment_roots: Vec<String>,
}

fn normalize_targets(targets: &[TargetRuntime]) -> Result<Vec<TargetRuntime>, AppError> {
    if targets.is_empty() {
        return Err(AppError::PlannerValidation {
            message: "at least one runtime is required".into(),
        });
    }

    let mut seen = BTreeSet::new();
    let mut normalized = Vec::with_capacity(targets.len());
    for target in targets {
        if !seen.insert(*target) {
            return Err(AppError::PlannerValidation {
                message: format!("duplicate runtime '{}'", target.as_str()),
            });
        }
        normalized.push(*target);
    }

    normalized.sort_unstable();
    Ok(normalized)
}

fn candidate_roots_for_target(
    registry: &AdapterRegistry,
    target: TargetRuntime,
    scope: TargetScope,
    policy: ProjectionPolicy,
    overrides: &BTreeMap<TargetRuntime, AdapterOverride>,
) -> Result<Vec<CandidateAssignment>, AppError> {
    let adapter = registry.get(target);
    if !adapter.supports_scope(scope) {
        return Err(AppError::PlannerValidation {
            message: format!(
                "runtime '{}' does not support scope '{}'",
                target.as_str(),
                scope.as_str()
            ),
        });
    }

    if let Some(root) = override_for_scope(overrides.get(&target), scope) {
        return Ok(vec![CandidateAssignment {
            target,
            root,
            source: RootSelectionSource::Override,
            rank: 0,
        }]);
    }

    let mut candidates: Vec<_> = adapter
        .roots_for_scope(scope)
        .into_iter()
        .map(|root| CandidateAssignment {
            target,
            root: root.path.to_string(),
            source: RootSelectionSource::Planner,
            rank: rank_for_policy(root.prefer_neutral_rank, root.prefer_native_rank, policy),
        })
        .collect();

    candidates.sort_by(|left, right| {
        left.rank
            .cmp(&right.rank)
            .then_with(|| left.root.cmp(&right.root))
    });

    if candidates.is_empty() {
        return Err(AppError::PlannerValidation {
            message: format!(
                "runtime '{}' does not document any roots for scope '{}'",
                target.as_str(),
                scope.as_str()
            ),
        });
    }

    Ok(candidates)
}

fn override_for_scope(
    override_config: Option<&AdapterOverride>,
    scope: TargetScope,
) -> Option<String> {
    let configured_root = match scope {
        TargetScope::Workspace => override_config.and_then(|config| config.workspace_root.as_ref()),
        TargetScope::User => override_config.and_then(|config| config.user_root.as_ref()),
    }?;

    match configured_root {
        AdapterRoot::Auto => None,
        AdapterRoot::Path(path) => Some(path.clone()),
    }
}

fn rank_for_policy(neutral_rank: u8, native_rank: u8, policy: ProjectionPolicy) -> u16 {
    match policy {
        ProjectionPolicy::MinimizeNoise | ProjectionPolicy::PreferNeutral => {
            u16::from(neutral_rank)
        }
        ProjectionPolicy::PreferNative => u16::from(native_rank),
    }
}

fn enumerate_candidate_plans(
    candidates: &[Vec<CandidateAssignment>],
    index: usize,
    current: &mut Vec<CandidateAssignment>,
    best: &mut Option<(PlanScore, Vec<CandidateAssignment>)>,
) {
    if index == candidates.len() {
        let score = score_assignments(current);
        let should_replace = best
            .as_ref()
            .is_none_or(|(best_score, _)| score < *best_score);
        if should_replace {
            *best = Some((score, current.clone()));
        }
        return;
    }

    for candidate in &candidates[index] {
        current.push(candidate.clone());
        enumerate_candidate_plans(candidates, index + 1, current, best);
        current.pop();
    }
}

fn score_assignments(assignments: &[CandidateAssignment]) -> PlanScore {
    let unique_roots: Vec<_> = assignments
        .iter()
        .map(|assignment| assignment.root.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let assignment_roots = assignments
        .iter()
        .map(|assignment| assignment.root.clone())
        .collect();

    PlanScore {
        root_count: unique_roots.len(),
        policy_rank: assignments
            .iter()
            .map(|assignment| usize::from(assignment.rank))
            .sum(),
        unique_roots,
        assignment_roots,
    }
}

fn build_projection_plan(
    scope: TargetScope,
    policy: ProjectionPolicy,
    assignments: Vec<CandidateAssignment>,
) -> TargetRootPlan {
    let assignments: Vec<_> = assignments
        .into_iter()
        .map(|assignment| TargetRootAssignment {
            target: assignment.target,
            root: assignment.root,
            source: assignment.source,
        })
        .collect();

    let mut grouped = BTreeMap::<String, Vec<TargetRuntime>>::new();
    for assignment in &assignments {
        grouped
            .entry(assignment.root.clone())
            .or_default()
            .push(assignment.target);
    }
    let physical_roots = grouped
        .into_iter()
        .map(|(path, mut targets)| {
            targets.sort_unstable();
            PhysicalRootPlan { path, targets }
        })
        .collect();

    ProjectionPlan {
        scope,
        policy,
        assignments,
        physical_roots,
    }
}

impl TargetScope {
    fn as_str(self) -> &'static str {
        match self {
            Self::Workspace => "workspace",
            Self::User => "user",
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::{
        adapter::{AdapterRegistry, TargetRuntime, TargetScope},
        manifest::{AdapterOverride, AdapterRoot, ProjectionPolicy},
    };

    #[test]
    fn workspace_planner_prefers_shared_agents_root_for_neutral_targets() {
        let registry = AdapterRegistry::new();

        let plan = plan_target_roots(
            &registry,
            TargetScope::Workspace,
            ProjectionPolicy::PreferNeutral,
            &[
                TargetRuntime::Codex,
                TargetRuntime::GeminiCli,
                TargetRuntime::Amp,
                TargetRuntime::Opencode,
            ],
            &BTreeMap::new(),
        )
        .expect("plan succeeds");

        assert_eq!(
            plan.physical_roots,
            vec![PhysicalRootPlan {
                path: ".agents/skills".into(),
                targets: vec![
                    TargetRuntime::Codex,
                    TargetRuntime::GeminiCli,
                    TargetRuntime::Amp,
                    TargetRuntime::Opencode,
                ],
            }]
        );
        assert_eq!(
            assignment_roots(&plan),
            vec![
                (
                    TargetRuntime::Codex,
                    ".agents/skills",
                    RootSelectionSource::Planner
                ),
                (
                    TargetRuntime::GeminiCli,
                    ".agents/skills",
                    RootSelectionSource::Planner,
                ),
                (
                    TargetRuntime::Amp,
                    ".agents/skills",
                    RootSelectionSource::Planner
                ),
                (
                    TargetRuntime::Opencode,
                    ".agents/skills",
                    RootSelectionSource::Planner,
                ),
            ]
        );
    }

    #[test]
    fn workspace_planner_switches_opencode_root_with_policy() {
        let registry = AdapterRegistry::new();

        let neutral = plan_target_roots(
            &registry,
            TargetScope::Workspace,
            ProjectionPolicy::PreferNeutral,
            &[TargetRuntime::Opencode],
            &BTreeMap::new(),
        )
        .expect("neutral plan succeeds");
        let native = plan_target_roots(
            &registry,
            TargetScope::Workspace,
            ProjectionPolicy::PreferNative,
            &[TargetRuntime::Opencode],
            &BTreeMap::new(),
        )
        .expect("native plan succeeds");

        assert_eq!(
            assignment_roots(&neutral),
            vec![(
                TargetRuntime::Opencode,
                ".agents/skills",
                RootSelectionSource::Planner,
            )]
        );
        assert_eq!(
            assignment_roots(&native),
            vec![(
                TargetRuntime::Opencode,
                ".opencode/skills",
                RootSelectionSource::Planner,
            )]
        );
    }

    #[test]
    fn user_scope_planner_prefers_claude_shared_root_for_claude_and_github() {
        let registry = AdapterRegistry::new();

        let plan = plan_target_roots(
            &registry,
            TargetScope::User,
            ProjectionPolicy::PreferNative,
            &[TargetRuntime::ClaudeCode, TargetRuntime::GithubCopilot],
            &BTreeMap::new(),
        )
        .expect("plan succeeds");

        assert_eq!(
            plan.physical_roots,
            vec![PhysicalRootPlan {
                path: "~/.claude/skills".into(),
                targets: vec![TargetRuntime::ClaudeCode, TargetRuntime::GithubCopilot],
            }]
        );
    }

    #[test]
    fn explicit_adapter_overrides_replace_registry_roots_for_the_selected_scope() {
        let registry = AdapterRegistry::new();
        let mut overrides = BTreeMap::new();
        overrides.insert(
            TargetRuntime::GithubCopilot,
            AdapterOverride {
                workspace_root: Some(AdapterRoot::Path("custom/copilot/skills".into())),
                user_root: None,
            },
        );

        let plan = plan_target_roots(
            &registry,
            TargetScope::Workspace,
            ProjectionPolicy::PreferNeutral,
            &[TargetRuntime::ClaudeCode, TargetRuntime::GithubCopilot],
            &overrides,
        )
        .expect("plan succeeds");

        assert_eq!(
            assignment_roots(&plan),
            vec![
                (
                    TargetRuntime::ClaudeCode,
                    ".claude/skills",
                    RootSelectionSource::Planner,
                ),
                (
                    TargetRuntime::GithubCopilot,
                    "custom/copilot/skills",
                    RootSelectionSource::Override,
                ),
            ]
        );
    }

    #[test]
    fn planner_rejects_duplicate_targets() {
        let registry = AdapterRegistry::new();
        let error = plan_target_roots(
            &registry,
            TargetScope::Workspace,
            ProjectionPolicy::PreferNeutral,
            &[TargetRuntime::Codex, TargetRuntime::Codex],
            &BTreeMap::new(),
        )
        .expect_err("duplicate targets are rejected");

        assert_eq!(
            error.to_string(),
            "invalid projection plan: duplicate runtime 'codex'"
        );
    }

    fn assignment_roots(plan: &TargetRootPlan) -> Vec<(TargetRuntime, &str, RootSelectionSource)> {
        plan.assignments
            .iter()
            .map(|assignment| {
                (
                    assignment.target,
                    assignment.root.as_str(),
                    assignment.source,
                )
            })
            .collect()
    }
}
