//! Projection materialization and cleanup domain entry points.

use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs, io,
    path::{Component, Path, PathBuf},
};

use serde::Serialize;
use serde_json::json;

use crate::{
    adapter::{AdapterRegistry, InstallModeRisk, TargetRuntime, TargetScope},
    app::AppContext,
    cli::Scope,
    error::AppError,
    history::HistoryLedger,
    lockfile::WorkspaceLockfile,
    manifest::{AdapterRoot, ProjectionMode, WorkspaceManifest},
    overlay::{NO_OVERLAY_HASH, hash_overlay_root},
    planner::{self, ProjectionPlan},
    resolver::{self, InternalSkillId, ProjectionOutcome, ResolvedSkillCandidate, SkillScope},
    response::AppResponse,
    source::{current_timestamp, imports_scope_root, stored_import_root},
    state::{
        LocalStateStore, ManagedScope, ManagedSkillRef, ProjectionMode as RecordedProjectionMode,
        ProjectionRecord,
    },
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
    /// Non-fatal warnings emitted while materializing projections.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    /// Physical roots reused as canonical sources instead of generated copies.
    pub canonical_roots: Vec<String>,
    /// Generated roots that received copied projections.
    pub generated_roots: Vec<GeneratedRootReport>,
    /// Reused canonical roots whose stale generated copies were pruned.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pruned_canonical_roots: Vec<GeneratedRootReport>,
    /// Documented roots the plan stopped including whose skillctl-generated
    /// copies were pruned.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pruned_unplanned_roots: Vec<GeneratedRootReport>,
    /// Generated copies left untouched because they hold unrecorded edits.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub preserved: Vec<PreservedProjection>,
    /// Generated copies replaced after a durable backup was taken.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub discarded: Vec<DiscardedProjection>,
    /// Total number of generated skill directories materialized across all roots.
    pub materialized_skills: usize,
    /// Total number of generated copies that already matched the plan and were
    /// left untouched.
    pub unchanged_skills: usize,
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
    /// Skill names whose generated copies already matched the plan, so this
    /// run left them byte-identical.
    pub unchanged: Vec<String>,
    /// Stale skill names pruned from this root.
    pub pruned: Vec<String>,
    /// Skill names left untouched in this root because they hold edits.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub preserved: Vec<String>,
}

/// One generated copy that was left untouched because it holds local edits.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PreservedProjection {
    /// Display path of the preserved copy.
    pub path: String,
    /// Skill the copy belongs to.
    pub skill: String,
    /// Physical root that holds the copy.
    pub root: String,
    /// Stable identifier of the preservation reason.
    pub reason: String,
    /// Human-readable explanation.
    pub detail: String,
    /// Actionable repair action.
    pub repair: String,
}

impl PreservedProjection {
    /// Warning line that names the copy and how to repair it.
    fn warning(&self) -> String {
        format!(
            "preserved '{}' with unrecorded edits ({}); {}",
            self.path, self.detail, self.repair
        )
    }
}

/// One generated copy replaced after a durable backup was taken.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DiscardedProjection {
    /// Display path of the discarded copy.
    pub path: String,
    /// Skill the copy belonged to.
    pub skill: String,
    /// Physical root that held the copy.
    pub root: String,
    /// Durable backup that holds the discarded content.
    pub backup: String,
}

/// How a projection change treats unrecorded local edits.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DriftPolicy {
    /// Replace edited copies after a durable backup instead of preserving them.
    pub discard: bool,
    /// Store used for durable pre-discard backups.
    pub store: Option<crate::apply::DiscardStore>,
    /// Restrict discarding to one skill, so a targeted apply never drops the
    /// edits of an unrelated skill that shares the root.
    pub discard_filter: Option<String>,
}

impl DriftPolicy {
    /// Preserve unrecorded edits and report them.
    pub(crate) fn preserve() -> Self {
        Self {
            discard: false,
            store: None,
            discard_filter: None,
        }
    }

    /// Back the edits of every generated copy up durably, then replace them.
    pub(crate) fn discard() -> Result<Self, AppError> {
        Ok(Self {
            discard: true,
            store: Some(crate::apply::DiscardStore::open_default()?),
            discard_filter: None,
        })
    }
}

/// Typed request for `skillctl sync`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SyncRequest {
    /// Back edited generated copies up durably, then replace them.
    pub discard: bool,
}

/// Typed request for `skillctl clean`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CleanRequest;

impl MaterializationReport {
    /// Return the recorded generation mode used by projection history and state.
    pub const fn recorded_generation_mode(&self) -> RecordedProjectionMode {
        match self.mode {
            ProjectionMode::Copy => RecordedProjectionMode::Copy,
            ProjectionMode::Symlink => RecordedProjectionMode::Symlink,
        }
    }

    /// Rewrite the repair advice for one skill this report can no longer repair.
    ///
    /// `remove` runs its final sync before it deletes the install record, so a
    /// preserved copy of the removed skill carries advice that names an update
    /// exiting with 3 the moment the command ends. Only the explicit discard
    /// repairs a copy whose skill no longer has a record, so the warning built
    /// from that advice is rebuilt too.
    pub(crate) fn advise_discard_only_for(&mut self, skill: &str, scope: ManagedScope) {
        for index in 0..self.preserved.len() {
            if self.preserved[index].skill != skill {
                continue;
            }
            let Some(reason) = crate::apply::DriftReason::parse(&self.preserved[index].reason)
            else {
                continue;
            };
            self.preserved[index].repair =
                reason.repair(skill, crate::apply::RepairCommands::DiscardOnly, scope);
            let warning = self.preserved[index].warning();
            let path = self.preserved[index].path.clone();
            for entry in &mut self.warnings {
                if entry.starts_with(&format!("preserved '{path}'")) {
                    *entry = warning.clone();
                }
            }
        }
    }
}

/// Compute the materialization report for the current workspace inputs.
pub fn sync_workspace(context: &AppContext) -> Result<MaterializationReport, AppError> {
    sync_workspace_with_policy(context, &DriftPolicy::preserve())
}

/// Compute and apply a materialization with an explicit drift policy.
pub(crate) fn sync_workspace_with_policy(
    context: &AppContext,
    policy: &DriftPolicy,
) -> Result<MaterializationReport, AppError> {
    let manifest = load_manifest_or_default(&context.working_directory)?;
    let lockfile = load_lockfile_or_default(&context.working_directory)?;
    let scope = sync_scope(context);
    let targets = selected_targets(context, &manifest)?;
    let registry = AdapterRegistry::new();
    let mut warnings = symlink_risk_warnings(&registry, &targets, &manifest)?;
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
    )
    .with_store_divergence_policy(store_divergence_policy(&manifest));
    let graph = resolver::build_effective_skill_graph(&request)?;

    ensure_scope_conflict_free(&graph, scope)?;
    warnings.extend(unpinned_overlay_warnings(&graph, skill_scope(scope))?);

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
    // The lockfile pins imports by name without a scope, so a name can resolve
    // at both scopes. Keying by scope and name keeps a divergence reported for
    // one scope from preserving the copy of the other.
    let diverged_stores: BTreeMap<(SkillScope, &str), &resolver::ImportStoreDivergence> = graph
        .store_divergences
        .iter()
        .map(|divergence| {
            (
                (divergence.scope, divergence.import_id.as_str()),
                divergence,
            )
        })
        .collect();

    let canonical_root = canonical_root_path(scope, context, &manifest)?;
    let mut canonical_roots = Vec::new();
    let mut generated_roots = Vec::new();
    let mut pruned_canonical_roots = Vec::new();
    let mut pruned_unplanned_roots = Vec::new();
    let mut preserved = Vec::new();
    let mut discarded = Vec::new();
    let mut materialized_skills = 0usize;
    let mut unchanged_skills = 0usize;
    let mut pruned_skills = 0usize;

    for root in &plan.physical_roots {
        let resolved_root = normalize_path(&resolve_runtime_root_path(context, &root.path)?);
        let reuses_canonical_root = canonical_root.as_ref() == Some(&resolved_root);
        let root_winners: Vec<_> = if reuses_canonical_root {
            winners
                .iter()
                .copied()
                .filter(|winner| winner.import.is_some())
                .collect()
        } else {
            winners.clone()
        };

        if root_winners.is_empty() && reuses_canonical_root {
            canonical_roots.push(root.path.clone());
            // Canonical local skills already live in this root, so nothing is
            // generated here. Generated copies of imports that stopped
            // resolving must still be pruned: every enabled runtime keeps
            // reading this root.
            if manifest.projection.prune {
                let pruned = prune_stale_projections(
                    &resolved_root,
                    &root.path,
                    &BTreeSet::new(),
                    &mut DriftReport {
                        context,
                        scope: managed_scope(scope),
                        policy,
                        preserved: &mut preserved,
                        discarded: &mut discarded,
                        warnings: &mut warnings,
                    },
                )?;
                pruned_skills += pruned.len();
                if !pruned.is_empty() {
                    pruned_canonical_roots.push(GeneratedRootReport {
                        path: root.path.clone(),
                        materialized: Vec::new(),
                        unchanged: Vec::new(),
                        pruned,
                        preserved: Vec::new(),
                    });
                }
            }
            continue;
        }

        let report = materialize_generated_root(
            &resolved_root,
            &root.path,
            &root_winners,
            manifest.projection.mode,
            manifest.projection.prune,
            &diverged_stores,
            &mut DriftReport {
                context,
                scope: managed_scope(scope),
                policy,
                preserved: &mut preserved,
                discarded: &mut discarded,
                warnings: &mut warnings,
            },
        )?;
        materialized_skills += report.materialized.len();
        unchanged_skills += report.unchanged.len();
        pruned_skills += report.pruned.len();
        generated_roots.push(report);
    }

    // A target change leaves the old physical root outside the plan. Every
    // runtime that still documents the root keeps loading whatever skillctl
    // left there, so the prune pass covers documented roots the plan no longer
    // includes, not only the roots it plans.
    if manifest.projection.prune {
        for (root_display, resolved_root, _) in
            unplanned_documented_roots(context, &manifest, scope, &plan)?
        {
            // A documented path that is missing or not a plain directory
            // holds no generated copies to prune. Planned roots keep their
            // own path-conflict reporting.
            match fs::symlink_metadata(&resolved_root) {
                Ok(metadata) if metadata.is_dir() => {}
                Ok(_) => continue,
                Err(source) if source.kind() == io::ErrorKind::NotFound => continue,
                Err(source) => {
                    return Err(AppError::FilesystemOperation {
                        action: "inspect projection root",
                        path: resolved_root,
                        source,
                    });
                }
            }

            let pruned = prune_stale_projections(
                &resolved_root,
                &root_display,
                &BTreeSet::new(),
                &mut DriftReport {
                    context,
                    scope: managed_scope(scope),
                    policy,
                    preserved: &mut preserved,
                    discarded: &mut discarded,
                    warnings: &mut warnings,
                },
            )?;
            if pruned.is_empty() {
                continue;
            }
            pruned_skills += pruned.len();
            pruned_unplanned_roots.push(GeneratedRootReport {
                path: root_display,
                materialized: Vec::new(),
                unchanged: Vec::new(),
                pruned,
                preserved: Vec::new(),
            });
        }
    }

    for entry in &preserved {
        warnings.push(entry.warning());
    }

    Ok(MaterializationReport {
        plan,
        mode: manifest.projection.mode,
        warnings,
        canonical_roots,
        generated_roots,
        pruned_canonical_roots,
        pruned_unplanned_roots,
        preserved,
        discarded,
        materialized_skills,
        unchanged_skills,
        pruned_skills,
    })
}

/// Warn about overlays this sync projects although they no longer hash to the
/// digest the lockfile pins.
///
/// The lockfile pin, not the overlay directory, is the contract a teammate's
/// `restore --frozen` verifies. An overlay edit that is projected but not
/// re-pinned leaves a committed pair that cannot be restored anywhere else, so
/// every projection rebuild reports it. An import whose overlay stopped
/// resolving never reaches the graph and fails resolution on its own.
fn unpinned_overlay_warnings(
    graph: &resolver::EffectiveSkillGraph,
    scope: SkillScope,
) -> Result<Vec<String>, AppError> {
    let mut warnings = Vec::new();
    let mut reported = BTreeSet::new();
    for candidate in &graph.candidates {
        let Some(import) = candidate.import.as_ref() else {
            continue;
        };
        if import.scope != scope || !reported.insert(import.id.clone()) {
            continue;
        }
        let Some(overlay) = candidate.overlay.as_ref() else {
            continue;
        };
        // A pin of `sha256:none` records that this import owns no overlay
        // content, so there is no recorded digest to drift from.
        if import.overlay_hash == NO_OVERLAY_HASH {
            continue;
        }
        let observed = hash_overlay_root(&overlay.root)?;
        if observed == import.overlay_hash {
            continue;
        }
        warnings.push(format!(
            "import '{}' has an overlay that hashes to {observed} instead of the pinned {}, so \
             this sync projects overlay content the lockfile does not pin and a frozen restore \
             refuses the pair. Run 'skillctl override {}' to re-pin the change after reviewing \
             it, then commit the lockfile together with the overlay",
            import.id, import.overlay_hash, import.id
        ));
    }
    Ok(warnings)
}

pub(crate) fn planned_physical_root_paths(
    context: &AppContext,
    manifest: &WorkspaceManifest,
    scope: TargetScope,
) -> Result<Vec<PathBuf>, AppError> {
    planned_physical_roots(context, manifest, scope)?
        .into_iter()
        .map(|root| resolve_runtime_root_path(context, &root.path))
        .collect()
}

/// Physical roots the manifest plans for one scope, with the targets each
/// serves.
///
/// The plan, not the recorded projection state, is the source of truth for
/// which destinations a check or a diagnosis expects: `explain`, `doctor`, and
/// the update check all read these assignments, so they never disagree about
/// whether a root should hold a projection. Callers that need only the paths
/// use [`planned_physical_root_paths`].
pub(crate) fn planned_physical_roots(
    context: &AppContext,
    manifest: &WorkspaceManifest,
    scope: TargetScope,
) -> Result<Vec<planner::PhysicalRootPlan>, AppError> {
    if manifest.targets.is_empty() {
        return Ok(Vec::new());
    }

    let registry = AdapterRegistry::new();
    let targets = selected_targets(context, manifest)?;
    let plan = planner::plan_target_roots(
        &registry,
        scope,
        manifest.projection.policy,
        &targets,
        &manifest.adapters,
    )?;

    Ok(plan.physical_roots)
}

/// Filesystem roots one sync of this scope may change.
///
/// Every root the current plan materializes is included, plus the documented
/// roots whose skillctl-generated output the prune pass may remove. Mutating
/// commands declare these as their filesystem targets so the lifecycle
/// journal covers the unplanned-root prune.
pub(crate) fn sync_mutation_root_paths(
    context: &AppContext,
    manifest: &WorkspaceManifest,
    scope: TargetScope,
) -> Result<Vec<PathBuf>, AppError> {
    let mut roots = planned_physical_root_paths(context, manifest, scope)?;
    roots.extend(unplanned_documented_root_paths(context, manifest, scope)?);
    Ok(roots)
}

/// Roots an edit repair of one scope scans for generated copies.
///
/// The repair resolves exactly the copies a sync may preserve, so it scans the
/// same universe a sync changes: every planned physical root plus every
/// documented root the prune pass covers. A narrower scan hides a copy in a root
/// the plan dropped from the engine, so the repair the preserved-copy advice
/// names changes nothing.
pub(crate) fn edit_repair_root_paths(
    context: &AppContext,
    manifest: &WorkspaceManifest,
    scope: TargetScope,
) -> Result<Vec<PathBuf>, AppError> {
    sync_mutation_root_paths(context, manifest, scope)
}

/// Documented roots for one scope whose skillctl-generated output a sync's
/// prune pass may remove although the current plan does not include them.
pub(crate) fn unplanned_documented_root_paths(
    context: &AppContext,
    manifest: &WorkspaceManifest,
    scope: TargetScope,
) -> Result<Vec<PathBuf>, AppError> {
    if manifest.targets.is_empty() {
        return Ok(Vec::new());
    }

    let registry = AdapterRegistry::new();
    let targets = selected_targets(context, manifest)?;
    let plan = planner::plan_target_roots(
        &registry,
        scope,
        manifest.projection.policy,
        &targets,
        &manifest.adapters,
    )?;

    Ok(unplanned_documented_roots(context, manifest, scope, &plan)?
        .into_iter()
        .map(|(_, resolved, _)| resolved)
        .collect())
}

/// Documented discovery roots for one scope that the plan does not include.
///
/// Sync scans the same universe `clean` scans: every adapter discovery root
/// documented for the scope, plus the manifest adapter overrides for it.
/// Prior projection records cannot drive this set, because every sync
/// replaces the scope's records, so after one sync with the new target set no
/// record points at the abandoned root any more. The on-disk projection
/// metadata stays the ownership signal for what may be pruned.
///
/// Each entry carries the display path, the resolved path, and the runtimes
/// that document the root, which diagnostics use to explain who still reads
/// it.
pub(crate) fn unplanned_documented_roots(
    context: &AppContext,
    manifest: &WorkspaceManifest,
    scope: TargetScope,
    plan: &ProjectionPlan,
) -> Result<Vec<(String, PathBuf, Vec<TargetRuntime>)>, AppError> {
    let mut planned = BTreeSet::new();
    for root in &plan.physical_roots {
        planned.insert(resolve_runtime_root_path(context, &root.path)?);
    }

    let registry = AdapterRegistry::new();
    let mut roots: BTreeMap<PathBuf, (String, BTreeSet<TargetRuntime>)> = BTreeMap::new();
    for adapter in registry.all() {
        for discovery_root in adapter.discovery_roots {
            if discovery_root.scope != scope {
                continue;
            }
            let resolved = resolve_runtime_root_path(context, discovery_root.path)?;
            roots
                .entry(resolved)
                .or_insert_with(|| (discovery_root.path.to_string(), BTreeSet::new()))
                .1
                .insert(adapter.target);
        }
    }
    for override_config in manifest.adapters.values() {
        let configured = match scope {
            TargetScope::Workspace => &override_config.workspace_root,
            TargetScope::User => &override_config.user_root,
        };
        if let Some(AdapterRoot::Path(path)) = configured {
            let resolved = resolve_runtime_root_path(context, path)?;
            roots
                .entry(resolved)
                .or_insert_with(|| (path.clone(), BTreeSet::new()));
        }
    }

    Ok(roots
        .into_iter()
        .filter(|(resolved, _)| !planned.contains(resolved))
        .map(|(resolved, (display, targets))| (display, resolved, targets.into_iter().collect()))
        .collect())
}

/// Enabled imports this checkout has not reconstructed into its import cache.
///
/// Resolution reads every enabled import, so these names are the ones that
/// stop a projection rebuild from running at all, which is the documented
/// state of a fresh clone until `restore` runs.
pub(crate) fn unrestored_imports(
    context: &AppContext,
    manifest: &WorkspaceManifest,
) -> Result<Vec<String>, AppError> {
    let mut names = Vec::new();
    for import in manifest.imports.iter().filter(|import| import.enabled) {
        let scope = match import.scope {
            crate::manifest::ManifestScope::Workspace => ManagedScope::Workspace,
            crate::manifest::ManifestScope::User => ManagedScope::User,
        };
        if !stored_import_root(scope, &context.working_directory, &import.id)?.is_dir() {
            names.push(import.id.clone());
        }
    }
    Ok(names)
}

/// Rebuild the projection records one scope's sync leaves behind.
///
/// The records the scope held before the sync are read here, so a caller that
/// does not track them itself still keeps the timestamps of unchanged copies.
pub(crate) fn rebuild_projection_records_for_scope(
    store: &mut LocalStateStore,
    scope: ManagedScope,
    sync_report: &MaterializationReport,
    generated_at: &str,
) -> Result<Vec<ProjectionRecord>, AppError> {
    let previous_records: Vec<_> = store
        .projection_records(None)?
        .into_iter()
        .filter(|record| record.skill.scope == scope)
        .collect();
    rebuild_projection_records_with_previous(
        store,
        scope,
        sync_report,
        generated_at,
        &previous_records,
    )
}

/// Rebuild the projection records one scope's sync leaves behind.
///
/// A generated copy that already matched the plan keeps the `generated_at`
/// value of the record that already describes it, so a repeated sync over an
/// unchanged repository writes the same rows instead of restamping them.
pub(crate) fn rebuild_projection_records_with_previous(
    store: &mut LocalStateStore,
    scope: ManagedScope,
    sync_report: &MaterializationReport,
    generated_at: &str,
    previous_records: &[ProjectionRecord],
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
    let generation_mode = sync_report.recorded_generation_mode();

    for generated_root in &sync_report.generated_roots {
        let Some(targets) = targets_by_root.get(&generated_root.path) else {
            continue;
        };

        for skill_name in generated_root
            .materialized
            .iter()
            .chain(generated_root.unchanged.iter())
        {
            let Some(install) = installs_by_skill.get(skill_name) else {
                continue;
            };
            let unchanged = generated_root.unchanged.contains(skill_name);

            for target in targets {
                let generated_at = if unchanged {
                    previous_record_generated_at(
                        previous_records,
                        &install.skill,
                        *target,
                        &generated_root.path,
                    )
                    .unwrap_or_else(|| generated_at.to_string())
                } else {
                    generated_at.to_string()
                };
                records.push(ProjectionRecord {
                    skill: install.skill.clone(),
                    target: *target,
                    generation_mode,
                    physical_root: generated_root.path.clone(),
                    projected_path: skill_name.clone(),
                    effective_version_hash: install.effective_version_hash.clone(),
                    generated_at,
                });
            }
        }
    }

    store.replace_projection_records_for_scope(scope, &records)?;
    Ok(records)
}

/// Timestamp of the record that already describes one unchanged projection.
fn previous_record_generated_at(
    previous_records: &[ProjectionRecord],
    skill: &ManagedSkillRef,
    target: TargetRuntime,
    physical_root: &str,
) -> Option<String> {
    previous_records
        .iter()
        .find(|record| {
            record.skill == *skill
                && record.target == target
                && record.physical_root == physical_root
        })
        .map(|record| record.generated_at.clone())
}

pub(crate) fn record_pruned_projection_history(
    ledger: &mut HistoryLedger<'_>,
    context: &AppContext,
    scope: ManagedScope,
    sync_report: &MaterializationReport,
    occurred_at: &str,
) -> Result<(), AppError> {
    for generated_root in &sync_report.generated_roots {
        record_pruned_root_history(ledger, context, scope, generated_root, occurred_at)?;
    }
    for canonical_root in &sync_report.pruned_canonical_roots {
        record_pruned_root_history(ledger, context, scope, canonical_root, occurred_at)?;
    }
    for unplanned_root in &sync_report.pruned_unplanned_roots {
        record_pruned_root_history(ledger, context, scope, unplanned_root, occurred_at)?;
    }

    Ok(())
}

/// Record one prune history event for every skill removed from one root.
fn record_pruned_root_history(
    ledger: &mut HistoryLedger<'_>,
    context: &AppContext,
    scope: ManagedScope,
    root: &GeneratedRootReport,
    occurred_at: &str,
) -> Result<(), AppError> {
    let resolved_root = planner::resolve_runtime_root_path(context, &root.path)?;
    for skill_name in &root.pruned {
        let skill = ManagedSkillRef::new(scope, skill_name.clone());
        let path = planner::display_path(context, &resolved_root.join(skill_name));
        ledger.record_prune(Some(&skill), &path, occurred_at)?;
    }

    Ok(())
}

/// Refresh the projection state one scope's sync leaves behind.
///
/// A record that already describes the projection byte for byte keeps its
/// history: an unchanged root must not append projection events on every run.
pub(crate) fn refresh_projection_state_for_scope(
    store: &mut LocalStateStore,
    context: &AppContext,
    scope: ManagedScope,
    sync_report: &MaterializationReport,
    occurred_at: &str,
    record_additional_history: impl FnOnce(&mut HistoryLedger<'_>) -> Result<(), AppError>,
) -> Result<Vec<ProjectionRecord>, AppError> {
    let previous_records: Vec<_> = store
        .projection_records(None)?
        .into_iter()
        .filter(|record| record.skill.scope == scope)
        .collect();
    let projection_records = rebuild_projection_records_with_previous(
        store,
        scope,
        sync_report,
        occurred_at,
        &previous_records,
    )?;
    let mut ledger = HistoryLedger::new(store);
    record_pruned_projection_history(&mut ledger, context, scope, sync_report, occurred_at)?;
    record_additional_history(&mut ledger)?;
    for record in &projection_records {
        if previous_records.contains(record) {
            continue;
        }
        ledger.record_projection(record)?;
    }

    Ok(projection_records)
}

/// Handle `skillctl sync`.
pub fn handle_sync(context: &AppContext, request: SyncRequest) -> Result<AppResponse, AppError> {
    let (manifest, manifest_from_defaults) =
        WorkspaceManifest::load_from_workspace_or_default(&context.working_directory)?;
    let scope = sync_scope(context);
    let policy = if request.discard {
        DriftPolicy::discard()?
    } else {
        DriftPolicy::preserve()
    };

    let mut builder = crate::apply::MutationPlanBuilder::new(
        crate::apply::MutationOperation::Sync,
        &context.working_directory,
    )
    .expect_imports(crate::apply::capture_expected_imports(
        context,
        Some(managed_scope(scope)),
    )?)
    .targets(
        planned_physical_root_paths(context, &manifest, scope)?,
        crate::apply::TargetAction::Replace,
    )
    .targets(
        unplanned_documented_root_paths(context, &manifest, scope)?,
        crate::apply::TargetAction::Remove,
    );
    if let Some(store) = &policy.store {
        builder = builder.discard_root(store.root().display().to_string());
    }
    let plan = builder.seal();

    crate::apply::run_guarded(&plan, context, |transaction| {
        transaction.track_state_database()?;

        let report = sync_workspace_with_policy(context, &policy)?;
        let timestamp = current_timestamp();
        let mut store = LocalStateStore::open_default_for(&context.working_directory)?;
        refresh_projection_state_for_scope(
            &mut store,
            context,
            managed_scope(scope),
            &report,
            &timestamp,
            |ledger| {
                for discarded in &report.discarded {
                    let skill = ManagedSkillRef::new(managed_scope(scope), discarded.skill.clone());
                    ledger.record_cleanup(
                        Some(&skill),
                        &format!("{} (backed up at {})", discarded.path, discarded.backup),
                        &timestamp,
                    )?;
                }
                Ok(())
            },
        )?;

        let summary = sync_summary(&report);
        let mut response = AppResponse::success("sync")
            .with_summary(summary)
            .with_data(serde_json::to_value(&report)?);
        for warning in &report.warnings {
            response = response.with_warning(warning.clone());
        }
        if manifest_from_defaults {
            response = response.with_warning(missing_manifest_warning(context, &manifest.path));
        }

        Ok(response)
    })
}

/// Handle `skillctl clean`.
pub fn handle_clean(context: &AppContext, _request: CleanRequest) -> Result<AppResponse, AppError> {
    let manifest = load_manifest_or_default(&context.working_directory)?;
    let lockfile = load_lockfile_or_default(&context.working_directory)?;
    let roots = cleanup_candidate_roots(context, &manifest)?;

    let plan = crate::apply::MutationPlanBuilder::new(
        crate::apply::MutationOperation::Clean,
        &context.working_directory,
    )
    .expect_imports(crate::apply::capture_expected_imports(context, None)?)
    .targets(
        roots.iter().map(|(_, root)| root.clone()),
        crate::apply::TargetAction::Remove,
    )
    .target(
        imports_scope_root(ManagedScope::Workspace, &context.working_directory)?,
        crate::apply::TargetAction::Remove,
    )
    .target(
        imports_scope_root(ManagedScope::User, &context.working_directory)?,
        crate::apply::TargetAction::Remove,
    )
    .target(
        context
            .working_directory
            .join(UNUSED_IMPORTS_PLACEHOLDER_DIR),
        crate::apply::TargetAction::Remove,
    )
    .seal();

    crate::apply::run_guarded(&plan, context, |transaction| {
        transaction.track_state_database()?;

        let mut store = LocalStateStore::open_default_for(&context.working_directory)?;
        let installs = store.list_install_records()?;
        let had_projection_records = !store.projection_records(None)?.is_empty();
        let timestamp = current_timestamp();
        let mut cleaned_projections = Vec::new();
        let mut cleaned_state = Vec::new();
        let mut preserved = Vec::new();

        for (scope, root) in &roots {
            let (cleaned, preserved_here) = clean_generated_projections_at(context, *scope, root)?;
            cleaned_projections.extend(cleaned);
            preserved.extend(preserved_here);
        }

        cleaned_state.extend(clean_unused_import_state(context, &manifest, &lockfile)?);

        store.clear_projection_records()?;

        {
            let installs = installs
                .into_iter()
                .map(|record| {
                    (
                        (record.skill.scope, record.skill.skill_id.clone()),
                        record.skill,
                    )
                })
                .collect::<std::collections::BTreeMap<_, _>>();
            let mut ledger = HistoryLedger::new(&mut store);

            for cleaned in &cleaned_projections {
                let skill = match (cleaned.scope, cleaned.skill.as_ref()) {
                    (Some(scope), Some(skill_id)) => installs
                        .get(&(scope, skill_id.clone()))
                        .cloned()
                        .or_else(|| Some(ManagedSkillRef::new(scope, skill_id))),
                    _ => None,
                };
                ledger.record_cleanup(skill.as_ref(), &cleaned.path, &timestamp)?;
            }

            for cleaned in &cleaned_state {
                ledger.record_cleanup(None, &cleaned.path, &timestamp)?;
            }
        }

        if had_projection_records || !cleaned_projections.is_empty() || !cleaned_state.is_empty() {
            transaction.checkpoint("after-state")?;
        }

        let summary = if cleaned_projections.is_empty() && cleaned_state.is_empty() {
            "No generated projections or cached state needed cleanup.".to_string()
        } else {
            format!(
                "Removed {} generated projection{} and {} generated state entr{}.",
                cleaned_projections.len(),
                if cleaned_projections.len() == 1 {
                    ""
                } else {
                    "s"
                },
                cleaned_state.len(),
                if cleaned_state.len() == 1 { "y" } else { "ies" },
            )
        };
        let summary = if preserved.is_empty() {
            summary
        } else {
            format!(
                "{summary} Preserved {} generated cop{} with unrecorded edits.",
                preserved.len(),
                if preserved.len() == 1 { "y" } else { "ies" }
            )
        };

        let mut response = AppResponse::success("clean")
            .with_summary(summary)
            .with_data(json!({
                "cleaned_projections": cleaned_projections,
                "cleaned_state": cleaned_state,
                "preserved": preserved,
            }));
        for entry in &preserved {
            response = response.with_warning(entry.warning());
        }

        Ok(response)
    })
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct CleanedPath {
    #[serde(skip_serializing_if = "Option::is_none")]
    scope: Option<ManagedScope>,
    #[serde(skip_serializing_if = "Option::is_none")]
    skill: Option<String>,
    path: String,
}

fn load_manifest_or_default(working_directory: &Path) -> Result<WorkspaceManifest, AppError> {
    let (manifest, _) = WorkspaceManifest::load_from_workspace_or_default(working_directory)?;
    Ok(manifest)
}

/// Warning that one sync ran on the documented defaults because the workspace
/// holds no manifest.
///
/// The defaults keep the simplest local-only flow working (spec section 11.1),
/// but the user never chose them, so the warning names the command that makes
/// the workspace explicit.
fn missing_manifest_warning(context: &AppContext, manifest_path: &Path) -> String {
    format!(
        "the workspace has no {}, so this sync used the documented defaults; run 'skillctl \
         init' to create the manifest and choose targets explicitly",
        planner::display_path(context, manifest_path)
    )
}

fn cleanup_candidate_roots(
    context: &AppContext,
    manifest: &WorkspaceManifest,
) -> Result<Vec<(ManagedScope, PathBuf)>, AppError> {
    let registry = AdapterRegistry::new();
    let mut roots = BTreeSet::new();

    for adapter in registry.all() {
        for root in adapter.discovery_roots {
            roots.insert((
                managed_scope(root.scope),
                planner::resolve_runtime_root_path(context, root.path)?,
            ));
        }
    }

    for (target, override_config) in &manifest.adapters {
        let _ = target;
        if let Some(AdapterRoot::Path(path)) = &override_config.workspace_root {
            roots.insert((
                ManagedScope::Workspace,
                planner::resolve_runtime_root_path(context, path)?,
            ));
        }
        if let Some(AdapterRoot::Path(path)) = &override_config.user_root {
            roots.insert((
                ManagedScope::User,
                planner::resolve_runtime_root_path(context, path)?,
            ));
        }
    }

    Ok(roots.into_iter().collect())
}

fn clean_generated_projections_at(
    context: &AppContext,
    scope: ManagedScope,
    root: &Path,
) -> Result<(Vec<CleanedPath>, Vec<PreservedProjection>), AppError> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            return Ok((Vec::new(), Vec::new()));
        }
        Err(source) => {
            return Err(AppError::FilesystemOperation {
                action: "read projection root",
                path: root.to_path_buf(),
                source,
            });
        }
    };

    let mut cleaned = Vec::new();
    let mut preserved = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| AppError::FilesystemOperation {
            action: "read projection root entry",
            path: root.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        let metadata = entry
            .file_type()
            .map_err(|source| AppError::FilesystemOperation {
                action: "inspect projection root entry",
                path: path.clone(),
                source,
            })?;
        if !metadata.is_dir() || !is_skillctl_generated_directory(&path)? {
            continue;
        }
        let skill = entry.file_name().to_string_lossy().into_owned();

        // A generated copy that no longer matches its recorded content holds
        // edits skillctl never made. Clean keeps it and says so.
        if let crate::apply::ProjectionState::Drift(drift) =
            crate::apply::inspect_projection(&path, None)?
        {
            let repair = drift_repair_advice(context, scope, &drift, &skill, None)?;
            preserved.push(preserved_entry(
                context,
                &drift,
                &skill,
                &root.display().to_string(),
                repair,
            ));
            continue;
        }

        fs::remove_dir_all(&path).map_err(|source| AppError::FilesystemOperation {
            action: "remove generated projection",
            path: path.clone(),
            source,
        })?;
        cleaned.push(CleanedPath {
            scope: Some(scope),
            skill: Some(skill),
            path: planner::display_path(context, &path),
        });
    }

    cleaned.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| left.skill.cmp(&right.skill))
    });
    preserved.sort_by(|left, right| left.path.cmp(&right.path));
    Ok((cleaned, preserved))
}

fn clean_unused_import_state(
    context: &AppContext,
    manifest: &WorkspaceManifest,
    lockfile: &WorkspaceLockfile,
) -> Result<Vec<CleanedPath>, AppError> {
    let mut cleaned = Vec::new();
    let workspace_referenced: BTreeSet<_> = manifest
        .imports
        .iter()
        .filter(|import| import.scope == crate::manifest::ManifestScope::Workspace)
        .map(|import| import.id.clone())
        .chain(
            lockfile
                .imports
                .keys()
                .filter(|id| {
                    manifest
                        .imports
                        .iter()
                        .find(|import| &import.id == *id)
                        .is_some_and(|import| {
                            import.scope == crate::manifest::ManifestScope::Workspace
                        })
                })
                .cloned(),
        )
        .collect();
    let user_referenced: BTreeSet<_> = manifest
        .imports
        .iter()
        .filter(|import| import.scope == crate::manifest::ManifestScope::User)
        .map(|import| import.id.clone())
        .chain(
            lockfile
                .imports
                .keys()
                .filter(|id| {
                    manifest
                        .imports
                        .iter()
                        .find(|import| &import.id == *id)
                        .is_some_and(|import| import.scope == crate::manifest::ManifestScope::User)
                })
                .cloned(),
        )
        .collect();

    for (root, referenced) in [
        (
            imports_scope_root(ManagedScope::Workspace, &context.working_directory)?,
            workspace_referenced,
        ),
        (
            imports_scope_root(ManagedScope::User, &context.working_directory)?,
            user_referenced,
        ),
    ] {
        let entries = match fs::read_dir(&root) {
            Ok(entries) => entries,
            Err(source) if source.kind() == io::ErrorKind::NotFound => continue,
            Err(source) => {
                return Err(AppError::FilesystemOperation {
                    action: "read imports store root",
                    path: root,
                    source,
                });
            }
        };

        for entry in entries {
            let entry = entry.map_err(|source| AppError::FilesystemOperation {
                action: "read imports store entry",
                path: root.clone(),
                source,
            })?;
            let path = entry.path();
            let metadata = entry
                .file_type()
                .map_err(|source| AppError::FilesystemOperation {
                    action: "inspect imports store entry",
                    path: path.clone(),
                    source,
                })?;
            if !metadata.is_dir() {
                continue;
            }

            let name = entry.file_name().to_string_lossy().into_owned();
            if referenced.contains(&name) {
                continue;
            }

            fs::remove_dir_all(&path).map_err(|source| AppError::FilesystemOperation {
                action: "remove unused import state",
                path: path.clone(),
                source,
            })?;
            cleaned.push(CleanedPath {
                scope: None,
                skill: None,
                path: path.display().to_string(),
            });
        }
    }

    let placeholder = context
        .working_directory
        .join(UNUSED_IMPORTS_PLACEHOLDER_DIR);
    if remove_directory_if_exists(&placeholder, "remove unused imports placeholder")? {
        cleaned.push(CleanedPath {
            scope: None,
            skill: None,
            path: planner::display_path(context, &placeholder),
        });
    }

    cleaned.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(cleaned)
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

fn managed_scope(scope: TargetScope) -> ManagedScope {
    match scope {
        TargetScope::Workspace => ManagedScope::Workspace,
        TargetScope::User => ManagedScope::User,
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
    mode: ProjectionMode,
    prune: bool,
    diverged_stores: &BTreeMap<(SkillScope, &str), &resolver::ImportStoreDivergence>,
    drift: &mut DriftReport<'_>,
) -> Result<GeneratedRootReport, AppError> {
    let mut report = GeneratedRootReport {
        path: root_display.to_string(),
        materialized: Vec::new(),
        unchanged: Vec::new(),
        pruned: Vec::new(),
        preserved: Vec::new(),
    };

    let mut desired_names = BTreeSet::new();
    for winner in winners {
        ensure_directory_parent(root_path)?;
        let skill_name = winner.skill.name.as_str().to_string();
        let target_dir = root_path.join(&skill_name);

        // A store that drifted from its pin is a projection-level event in
        // symlink mode: the copy's own links are unchanged, so every other
        // check would call the copy clean while the bytes behind it are the
        // ones the lockfile no longer pins. No policy may regenerate the copy
        // from that store, because relinking serves the same diverged bytes.
        if let Some(divergence) = winner
            .import
            .as_ref()
            .and_then(|import| diverged_stores.get(&(import.scope, import.id.as_str())))
        {
            if drift.preserve_diverged_store(divergence, &skill_name, root_display, &target_dir) {
                report.preserved.push(skill_name.clone());
            }
            desired_names.insert(skill_name);
            continue;
        }

        let desired =
            crate::apply::desired_projection_digest(winner, mode == ProjectionMode::Symlink)?;
        let state = crate::apply::inspect_projection(&target_dir, Some(desired.as_str()))?;

        let preserved_here = match &state {
            crate::apply::ProjectionState::Drift(found) => {
                drift.handle(found, &skill_name, root_display, &target_dir, Some(winner))?
            }
            _ => false,
        };
        if preserved_here {
            report.preserved.push(skill_name.clone());
            desired_names.insert(skill_name);
            continue;
        }

        // A copy that already matches the content and the metadata this run
        // would record is left byte-identical, so an unchanged repository
        // stays clean across repeated syncs.
        if projection_is_current(&state, &target_dir, root_display, winner, mode, &desired)? {
            report.unchanged.push(skill_name.clone());
            desired_names.insert(skill_name);
            continue;
        }

        materialize_projection(root_path, root_display, winner, mode)?;
        report.materialized.push(skill_name.clone());
        desired_names.insert(skill_name);
    }

    if prune {
        report.pruned = prune_stale_projections(root_path, root_display, &desired_names, drift)?;
    }

    Ok(report)
}

/// Mutable report state shared by every root materialized in one sync.
struct DriftReport<'a> {
    context: &'a AppContext,
    scope: ManagedScope,
    policy: &'a DriftPolicy,
    preserved: &'a mut Vec<PreservedProjection>,
    discarded: &'a mut Vec<DiscardedProjection>,
    /// Warnings this run collected outside a preserved copy.
    warnings: &'a mut Vec<String>,
}

impl DriftReport<'_> {
    /// Report one import whose store no longer matches the lockfile pin.
    ///
    /// Returns `true` when a generated copy exists, because that copy is
    /// preserved untouched: its links are exactly what skillctl last wrote, so
    /// replacing them relinks to the same diverged bytes, and the drift
    /// policies for edited copies do not apply. Without a copy there is
    /// nothing to preserve and the divergence is still reported as a warning,
    /// so the user learns the store needs a repair before anything projects.
    fn preserve_diverged_store(
        &mut self,
        divergence: &resolver::ImportStoreDivergence,
        skill_name: &str,
        root_display: &str,
        target_dir: &Path,
    ) -> bool {
        let reason = crate::apply::DriftReason::DivergedImportStore;
        // The command classification is irrelevant for this reason: every
        // update strategy resolves through the store it would have to repair.
        let repair = reason.repair(
            skill_name,
            crate::apply::RepairCommands::DiscardOnly,
            self.scope,
        );
        let copy_exists = target_dir.is_dir();
        let digests = format!(
            "the lockfile pins {} but the store holds {}",
            divergence.pinned_content_hash, divergence.observed_content_hash
        );

        if copy_exists {
            self.preserved.push(PreservedProjection {
                path: planner::display_path(self.context, target_dir),
                skill: skill_name.to_string(),
                root: root_display.to_string(),
                reason: reason.as_str().to_string(),
                detail: format!("{}; {digests}", reason.detail()),
                repair,
            });
            return true;
        }

        self.warnings.push(format!(
            "import '{}' no longer matches its pinned content, so no projection was written \
             for '{}'; {digests}; {repair}",
            divergence.import_id, skill_name
        ));

        false
    }

    /// Handle one drifted copy. Returns `true` when it was preserved.
    ///
    /// `candidate` is the winner the copy belongs to, when one still resolves.
    /// It decides whether the trust gate refuses the update strategies that
    /// would otherwise repair the edits.
    fn handle(
        &mut self,
        drift: &crate::apply::ProjectionDrift,
        skill_name: &str,
        root_display: &str,
        target_dir: &Path,
        candidate: Option<&ResolvedSkillCandidate>,
    ) -> Result<bool, AppError> {
        let discard_allowed = self.policy.discard
            && self
                .policy
                .discard_filter
                .as_ref()
                .is_none_or(|filter| filter == skill_name);
        if !discard_allowed {
            let repair =
                drift_repair_advice(self.context, self.scope, drift, skill_name, candidate)?;
            self.preserved.push(preserved_entry(
                self.context,
                drift,
                skill_name,
                root_display,
                repair,
            ));
            return Ok(true);
        }

        let store =
            self.policy
                .store
                .as_ref()
                .ok_or_else(|| AppError::MaterializationValidation {
                    message: format!(
                        "discarding edits at '{}' requires a discard store",
                        target_dir.display()
                    ),
                })?;
        let backup = store.retain(&discard_label(root_display, skill_name), target_dir)?;
        self.discarded.push(DiscardedProjection {
            path: planner::display_path(self.context, target_dir),
            skill: skill_name.to_string(),
            root: root_display.to_string(),
            backup: backup.display().to_string(),
        });
        Ok(false)
    }
}

/// Stable label for one durable pre-discard backup.
fn discard_label(root_display: &str, skill_name: &str) -> String {
    use sha2::Digest as _;

    let mut hasher = sha2::Sha256::new();
    hasher.update(root_display.as_bytes());
    format!(
        "{skill_name}-{:016x}",
        u128::from_be_bytes(
            hasher.finalize()[..16]
                .try_into()
                .expect("digest slice fits"),
        ) & 0xffff_ffff_ffff_ffff
    )
}

fn preserved_entry(
    context: &AppContext,
    drift: &crate::apply::ProjectionDrift,
    skill_name: &str,
    root_display: &str,
    repair: String,
) -> PreservedProjection {
    PreservedProjection {
        path: planner::display_path(context, &drift.path),
        skill: skill_name.to_string(),
        root: root_display.to_string(),
        reason: drift.reason.as_str().to_string(),
        detail: drift.reason.detail().to_string(),
        repair,
    }
}

/// Repair advice for one drifted copy, aware of the trust gate.
///
/// The gate refuses the update strategies that resolve unrecorded edits for an
/// unreviewed script-bearing Git import while a revision move is waiting, so for
/// that skill the advice names the fork and the explicit discard instead. A
/// gated import whose local state shows no waiting revision keeps the update
/// strategies, because their repair keeps the pinned revision and the engine
/// lets it through. A skill no install record holds — a removed import, or a
/// canonical local skill — gets the discard alone, because `update` refuses the
/// name outright. See [`crate::trust::drift_repair_commands`].
fn drift_repair_advice(
    context: &AppContext,
    scope: ManagedScope,
    drift: &crate::apply::ProjectionDrift,
    skill_name: &str,
    candidate: Option<&ResolvedSkillCandidate>,
) -> Result<String, AppError> {
    let store = LocalStateStore::open_default_for(&context.working_directory)?;
    let skill = ManagedSkillRef::new(scope, skill_name);
    let commands =
        crate::trust::drift_repair_commands(&store, &skill, candidate, &context.working_directory)?;
    Ok(drift.reason.repair(skill_name, commands, scope))
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
    mode: ProjectionMode,
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

        materialize_projection_file(&file.source_path, &destination, mode)?;
    }

    // The digest covers exactly the files written above, so a later run can
    // tell a skillctl-owned copy from one that holds unrecorded local edits.
    let content_digest = crate::apply::hash_projection_directory(&target_dir)?;
    write_projection_metadata(&target_dir, root_display, winner, mode, &content_digest)
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
    root_display: &str,
    desired_names: &BTreeSet<String>,
    drift: &mut DriftReport<'_>,
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
        // The bundled skill follows its own projection plan, which covers
        // every documented user root regardless of manifest targets. Its
        // copies are never stale output of this workspace's plan, and only
        // the explicit bundled removal path may delete them.
        if is_bundled_projection_directory(&path)? {
            continue;
        }

        // Generated status proves ownership, not permission to delete content
        // skillctl never recorded. No winner resolves to the copy any more, so
        // the gate is decided from the install record alone.
        if let crate::apply::ProjectionState::Drift(found) =
            crate::apply::inspect_projection(&path, None)?
            && drift.handle(&found, &name, root_display, &path, None)?
        {
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

/// Whether one skillctl-generated directory holds the bundled `skillctl`
/// skill.
///
/// The bundled skill's copies record `source.kind: "bundled"` in their
/// projection metadata, which stays readable even where no install record
/// exists.
pub(crate) fn is_bundled_projection_directory(path: &Path) -> Result<bool, AppError> {
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

    Ok(parsed["source"]["kind"] == "bundled")
}

fn write_projection_metadata(
    target_dir: &Path,
    root_display: &str,
    winner: &ResolvedSkillCandidate,
    mode: ProjectionMode,
    content_digest: &str,
) -> Result<(), AppError> {
    let metadata_path = target_dir.join(PROJECTION_METADATA_FILE);
    let metadata = projection_metadata(root_display, winner, mode, content_digest);
    let serialized = serde_json::to_string_pretty(&metadata)?;

    fs::write(&metadata_path, format!("{serialized}\n")).map_err(|source| {
        AppError::FilesystemOperation {
            action: "write projection metadata",
            path: metadata_path,
            source,
        }
    })
}

/// Metadata one materialization records for a generated copy.
///
/// `generated_at` is the only field that changes between two runs over the
/// same content by one build, and `version` is the only one that changes
/// across a release, so [`projection_is_current`] compares the metadata
/// without either. `generated_at` is RFC3339, the format
/// [`ProjectionRecord::generated_at`] already uses, so one field name never
/// carries two timestamp formats.
fn projection_metadata(
    root_display: &str,
    winner: &ResolvedSkillCandidate,
    mode: ProjectionMode,
    content_digest: &str,
) -> serde_json::Value {
    json!({
        "tool": "skillctl",
        "version": env!("CARGO_PKG_VERSION"),
        "generated_at": current_timestamp(),
        "generation_mode": projection_mode_label(mode),
        "content_digest": content_digest,
        "physical_root": root_display,
        "skill_name": winner.skill.name.as_str(),
        "internal_id": internal_id_label(&winner.internal_id),
        "source": projection_source(winner),
    })
}

/// Whether one generated copy already equals the projection this run would
/// write.
///
/// A copy counts as current only when `state` reports it clean, which proves
/// the bytes on disk still hash to the digest its metadata records, and every
/// projection metadata field except `generated_at` and `version` matches what
/// this run would record. Dropping those two keeps a committed copy current
/// across an upgrade: releases before the RFC3339 unification stored unix
/// seconds in `generated_at`, every release records its own `version`, and a
/// version bump alone must not rewrite a committed projection. A drifted copy
/// never qualifies, including one the drift policy just backed up for a
/// discard: it must be regenerated.
fn projection_is_current(
    state: &crate::apply::ProjectionState,
    target_dir: &Path,
    root_display: &str,
    winner: &ResolvedSkillCandidate,
    mode: ProjectionMode,
    desired_digest: &str,
) -> Result<bool, AppError> {
    if !matches!(state, crate::apply::ProjectionState::Clean) {
        return Ok(false);
    }

    let metadata_path = target_dir.join(PROJECTION_METADATA_FILE);
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

    let mut recorded = serde_json::from_str::<serde_json::Value>(&contents).map_err(|source| {
        AppError::MaterializationValidation {
            message: format!(
                "projection metadata '{}' is invalid JSON: {source}",
                metadata_path.display()
            ),
        }
    })?;

    let mut expected = projection_metadata(root_display, winner, mode, desired_digest);
    remove_provenance_fields(&mut recorded);
    remove_provenance_fields(&mut expected);

    Ok(recorded == expected)
}

/// Metadata fields that record which build wrote a copy and when, rather than
/// what the copy holds.
const PROJECTION_METADATA_PROVENANCE_FIELDS: &[&str] = &["generated_at", "version"];

/// Drop the provenance fields from one metadata object.
///
/// Both are dropped rather than compared because neither carries a currency
/// signal and because each varies between releases a committed copy survives:
/// `generated_at` exists in two formats in the wild, RFC3339 from current
/// releases and unix seconds from releases before the unification, and
/// `version` follows the build that last wrote the copy. Currency comes from
/// the content digest the remaining fields record.
fn remove_provenance_fields(metadata: &mut serde_json::Value) {
    if let serde_json::Value::Object(fields) = metadata {
        for field in PROJECTION_METADATA_PROVENANCE_FIELDS {
            fields.remove(*field);
        }
    }
}

fn materialize_projection_file(
    source_path: &Path,
    destination: &Path,
    mode: ProjectionMode,
) -> Result<(), AppError> {
    match mode {
        ProjectionMode::Copy => {
            fs::copy(source_path, destination).map_err(|source| AppError::FilesystemOperation {
                action: "copy projected file",
                path: destination.to_path_buf(),
                source,
            })?;
        }
        ProjectionMode::Symlink => {
            create_projection_symlink(source_path, destination)?;
        }
    }

    Ok(())
}

#[cfg(unix)]
fn create_projection_symlink(source_path: &Path, destination: &Path) -> Result<(), AppError> {
    std::os::unix::fs::symlink(source_path, destination).map_err(|source| {
        AppError::FilesystemOperation {
            action: "create projected symlink",
            path: destination.to_path_buf(),
            source,
        }
    })
}

#[cfg(windows)]
fn create_projection_symlink(source_path: &Path, destination: &Path) -> Result<(), AppError> {
    std::os::windows::fs::symlink_file(source_path, destination).map_err(|source| {
        AppError::FilesystemOperation {
            action: "create projected symlink",
            path: destination.to_path_buf(),
            source,
        }
    })
}

/// How a projection rebuild treats a stored import that no longer hashes to
/// its pin.
///
/// Copy mode must refuse: a rebuild would copy unverified bytes into every
/// runtime root, so resolution fails and names a repair. Symlink mode cannot
/// refuse the same way, because the divergence usually arrives *through* the
/// projection: a projected file links straight into the import store, so an
/// ordinary edit of the runtime copy writes the stored bytes the lockfile
/// pins. The rebuild reports that as a projection-level event and preserves
/// the copy instead of failing the whole workspace.
pub(crate) fn store_divergence_policy(
    manifest: &WorkspaceManifest,
) -> resolver::StoreDivergencePolicy {
    match manifest.projection.mode {
        ProjectionMode::Symlink => resolver::StoreDivergencePolicy::Report,
        ProjectionMode::Copy => resolver::StoreDivergencePolicy::Fail,
    }
}

fn symlink_risk_warnings(
    registry: &AdapterRegistry,
    targets: &[TargetRuntime],
    manifest: &WorkspaceManifest,
) -> Result<Vec<String>, AppError> {
    if manifest.projection.mode != ProjectionMode::Symlink {
        return Ok(Vec::new());
    }

    let mut unstable_targets = targets
        .iter()
        .copied()
        .filter(|target| {
            registry.get(*target).install_mode_risk == InstallModeRisk::SymlinkUnstable
        })
        .collect::<Vec<_>>();
    unstable_targets.sort_unstable();

    if unstable_targets.is_empty() {
        return Ok(Vec::new());
    }

    let acknowledged: BTreeSet<_> = manifest
        .projection
        .allow_unsafe_targets
        .iter()
        .copied()
        .collect();
    let missing = unstable_targets
        .iter()
        .copied()
        .filter(|target| !acknowledged.contains(target))
        .collect::<Vec<_>>();

    if !missing.is_empty() {
        return Err(AppError::MaterializationValidation {
            message: format!(
                "projection.mode 'symlink' requires explicit projection.allow_unsafe_targets entries for unstable target{} {}; use 'copy' or acknowledge each target",
                plural_suffix(missing.len()),
                missing
                    .iter()
                    .map(|target| format!("'{}'", target.as_str()))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        });
    }

    Ok(unstable_targets
        .into_iter()
        .map(|target| {
            format!(
                "target '{}' documents unstable symlink behavior; projection.allow_unsafe_targets explicitly enabled symlink mode and copy mode remains safer",
                target.as_str()
            )
        })
        .collect())
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
            "requested_ref": serde_json::Value::Null,
            "resolved_revision": serde_json::Value::Null,
            "upstream_revision": serde_json::Value::Null,
            "content_hash": serde_json::Value::Null,
            "overlay_hash": serde_json::Value::Null,
            "effective_version_hash": serde_json::Value::Null,
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
            "requested_ref": import.requested_ref,
            "resolved_revision": import.resolved_revision,
            "upstream_revision": import.upstream_revision,
            "content_hash": import.content_hash,
            "overlay_hash": import.overlay_hash,
            "effective_version_hash": import.effective_version_hash,
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

fn projection_mode_label(mode: ProjectionMode) -> &'static str {
    match mode {
        ProjectionMode::Copy => "copy",
        ProjectionMode::Symlink => "symlink",
    }
}

fn sync_summary(report: &MaterializationReport) -> String {
    let generated = report.materialized_skills;
    let unchanged = report.unchanged_skills;
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
    let unchanged_suffix = if unchanged == 0 {
        String::new()
    } else {
        format!(
            " {unchanged} generated projection{} already matched the plan.",
            plural_suffix(unchanged)
        )
    };

    if generated == 0 {
        let mut summary = format!("No generated projections were required.{unchanged_suffix}");
        if pruned > 0 {
            summary.push_str(&format!(
                " Pruned {pruned} stale projection{}.",
                plural_suffix(pruned),
            ));
        }
        summary.push_str(&canonical_suffix);
        summary
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
        summary.push_str(&unchanged_suffix);
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
