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
    doctor::ProjectionObservation,
    error::AppError,
    history::HistoryLedger,
    lockfile::WorkspaceLockfile,
    manifest::{AdapterOverride, AdapterRoot, ProjectionMode, ProjectionPolicy, WorkspaceManifest},
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
    /// Apply the planned updates through the guarded engine.
    pub apply: bool,
    /// Strategy an apply uses for unmanaged local edits.
    pub strategy: Option<crate::apply::ApplyStrategy>,
    /// Write a plan document to this path and exit without applying.
    pub plan: Option<PathBuf>,
    /// Apply exactly this plan document.
    pub plan_file: Option<PathBuf>,
}

impl UpdateRequest {
    /// Create an update request from parsed CLI arguments.
    pub fn new(skill: Option<String>) -> Self {
        Self {
            skill,
            ..Self::default()
        }
    }

    /// Effective strategy for an apply.
    pub fn effective_strategy(&self) -> crate::apply::ApplyStrategy {
        self.strategy.unwrap_or_default()
    }
}

/// Handle `skillctl update`.
pub fn handle_update(
    context: &AppContext,
    request: UpdateRequest,
) -> Result<AppResponse, AppError> {
    if request.apply
        && let Some(path) = request.plan_file.as_deref()
    {
        return apply_plan_document(context, &request, path);
    }

    let manifest = WorkspaceManifest::load_required_from_workspace(context, "update")?;
    // The check compares installed state against the pins the lockfile holds,
    // so a missing lockfile is a refusal that names its repairs rather than a
    // report that every import is new.
    let lockfile = WorkspaceLockfile::load_required_from_workspace(context, "update")?;
    let mut store = LocalStateStore::open_default_for(&context.working_directory)?;
    let selected_skills = select_managed_skills(&store, context, &request)?;
    if selected_skills.is_empty() {
        return Ok(AppResponse::success("update")
            .with_summary("No managed skills are installed.")
            .with_data(json!({ "plans": [] })));
    }

    let imported = imported_candidate_map(context, &manifest, &lockfile, &store)?;
    // A run that names the diverged skill is a check of that skill's baseline,
    // and the baseline is what broke. The refusal holds under every strategy:
    // a fork apply rides on the same check, so `--strategy fork` refuses the
    // same way, and the resolver's error names the standalone `skillctl fork
    // <skill>` that reads the stored bytes directly. A whole-workspace run
    // plans around the divergence, so unrelated skills keep updating.
    let named_skill = request
        .skill
        .as_deref()
        .or(context.selector.skill_name.as_deref());
    if let Some(skill_id) = named_skill
        && let Some(managed_skill) = selected_skills
            .iter()
            .find(|skill| skill.skill_id == skill_id)
        && let Some(divergence) = imported.store_divergence(managed_skill)
    {
        return Err(AppError::ResolutionValidation {
            message: resolver::store_divergence_error(
                &divergence.import_id,
                &divergence.pinned_content_hash,
                &divergence.observed_content_hash,
            ),
        });
    }

    let strategy = request.effective_strategy();
    let check = UpdateCheckInputs {
        candidates: &imported.candidates,
        diverged_stores: &imported.diverged_stores,
        reviews: &crate::trust::ReviewIndex::load(&store)?,
        lockfile: &lockfile,
        checked_at: &current_timestamp(),
    };
    let mut plans = Vec::with_capacity(selected_skills.len());
    let mut targets = Vec::new();
    let mut skipped_by_plan: Vec<crate::apply::SkippedUpdate> = Vec::new();

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
            check.candidate(&managed_skill),
            &check,
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

        let contributes_target = apply_requested(&request, &prepared.plan, strategy);
        if !contributes_target && (request.apply || request.plan.is_some()) {
            skipped_by_plan.push(crate::apply::SkippedUpdate {
                skill: managed_skill.skill_id.clone(),
                scope: managed_skill.scope,
                reason: skip_reason(&prepared.plan, strategy, request.plan.is_some()),
            });
        }

        if contributes_target {
            targets.push(crate::apply::UpdateApplyTarget {
                skill: managed_skill.clone(),
                source_url: install.source_url.clone(),
                target_revision: prepared
                    .plan
                    .latest_revision
                    .clone()
                    .unwrap_or_else(|| install.resolved_revision.clone()),
                expected: crate::apply::capture_expected_import(context, install)?,
                summary: prepared.plan.clone(),
            });
        }

        plans.push(prepared.plan);
    }

    if let Some(path) = request.plan.as_deref() {
        return write_plan_document(context, &request, &plans, &targets, &skipped_by_plan, path);
    }

    let mut apply_report = crate::apply::UpdateApplyReport {
        strategy: strategy.as_str().to_string(),
        ..crate::apply::UpdateApplyReport::default()
    };
    if request.apply && strategy == crate::apply::ApplyStrategy::Clean {
        // A clean apply must refuse loudly instead of quietly skipping a skill
        // whose edits block it.
        if let Some(plan) = plans.iter().find(|plan| blocks_clean_apply(plan)) {
            // The managed overlay is listed as a modification too, so the path
            // the refusal names is read from the unmanaged edit that blocks
            // the apply, which is the edit a strategy has to resolve.
            let path = plan
                .modifications
                .iter()
                .find(|modification| !modification.managed)
                .and_then(|modification| modification.path.clone())
                .or_else(|| {
                    plan.modifications
                        .iter()
                        .find_map(|modification| modification.path.clone())
                })
                .unwrap_or_else(|| plan.skill.clone());
            return Err(AppError::DirtyContent {
                path: context.working_directory.join(path),
                message: format!(
                    "'{}' has unmanaged local edits; re-run with '--strategy overlay' to keep \
                     them, '--strategy discard' to back them up and drop them, or '--strategy \
                     fork' to take local ownership",
                    plan.skill
                ),
            });
        }
    }

    if request.apply {
        apply_report = crate::apply::run_update_apply(context, targets, strategy)?;
        apply_report.skipped.append(&mut skipped_by_plan);
    }

    let telemetry = telemetry::prepare_update_report(context, &plans)?;
    let mut summary = update_summary(&plans);
    if let Some(notice) = telemetry.notice_message() {
        summary.push('\n');
        summary.push_str(notice);
    }
    if request.apply {
        summary.push('\n');
        summary.push_str(&apply_summary(&apply_report));
    }
    let mut response = AppResponse::success("update")
        .with_summary(summary)
        .with_data(json!({
            "plans": plans,
            "telemetry": telemetry,
            "applied": apply_report.applied,
            "apply_skipped": apply_report.skipped,
            "preserved": apply_report.preserved,
            "discarded": apply_report.discarded,
            "strategy": strategy.as_str(),
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
    // An applied skill gets its projections rebuilt by the apply itself, so the
    // sync advice is only issued for a skill the run left behind, including a
    // run that only reports.
    let applied_skills: BTreeSet<&str> = apply_report
        .applied
        .iter()
        .map(|applied| applied.skill.as_str())
        .collect();
    for plan in &plans {
        if applied_skills.contains(plan.skill.as_str()) {
            continue;
        }
        for missing in &plan.missing_projections {
            response =
                response.with_warning(missing_projection_warning(&plan.skill, missing, plan.scope));
        }
        // The overlay drift is a warning the check records in its notes only,
        // so a human run would leave the reader with a count of outcomes and
        // no reason why a copy may be stale.
        if plan.overlay_changed_since_recorded_state {
            response = response.with_warning(overlay_drift_warning(plan));
        }
    }
    if request.apply {
        for warning in noop_apply_warnings(&apply_report, strategy) {
            response = response.with_warning(warning);
        }
    }

    Ok(response)
}

/// Warnings for a requested apply that changed nothing.
///
/// A run that reports success while skipping every skill hides the fact that
/// the requested repair never happened, so each skip is surfaced as a warning.
/// The skip strategy is exempt because it is an explicit request to change
/// nothing.
fn noop_apply_warnings(
    report: &crate::apply::UpdateApplyReport,
    strategy: crate::apply::ApplyStrategy,
) -> Vec<String> {
    if strategy == crate::apply::ApplyStrategy::Skip
        || !report.applied.is_empty()
        || report.skipped.is_empty()
    {
        return Vec::new();
    }

    report
        .skipped
        .iter()
        .map(|skipped| {
            format!(
                "the requested apply changed nothing; '{}' was skipped: {}",
                skipped.skill, skipped.reason
            )
        })
        .collect()
}

/// Warning for a plan document that leaves an apply nothing to do.
///
/// A document with no plans, or one whose plans name no managed skill,
/// contributes neither a target nor a skip. Reporting a bare success hides
/// that the requested repair never happened, so the run names the document
/// instead. The workspace itself is fine, which is why this stays a warning.
fn empty_plan_document_warning(
    document: &crate::apply::PlanDocument,
    document_holds_no_targets: bool,
    document_skipped_nothing: bool,
) -> Option<String> {
    if !document_holds_no_targets || !document_skipped_nothing {
        return None;
    }

    let reason = if document.plans.is_empty() {
        "the plan document contains no plans"
    } else {
        "no plan in the document names a managed skill"
    };
    Some(format!(
        "{reason}, so the requested apply changed nothing; rebuild it with 'skillctl update \
         --plan'"
    ))
}

/// Whether one check plan contributes a target to the requested run.
///
/// A blocked check still becomes a target when the chosen strategy can
/// resolve the blocking edits itself. A local source becomes a target for the
/// same reason: it has no upstream revision to move to, but its generated
/// copies can hold unrecorded edits that only a repair strategy can resolve.
/// An up-to-date check that finds such edits joins it, because the strategies
/// that resolve the edits work without moving the pinned revision. A plan
/// document is wider than a direct apply, because the apply that runs the
/// document later picks its own strategy: a blocked check, and an up-to-date
/// check with edits, contribute to the document under every strategy, and that
/// apply refuses loudly when its strategy cannot resolve the edits. Detached
/// and failed checks are reported instead of turned into mutations.
fn apply_requested(
    request: &UpdateRequest,
    plan: &SkillUpdatePlan,
    strategy: crate::apply::ApplyStrategy,
) -> bool {
    let for_document = request.plan.is_some();
    if !(for_document || request.apply) {
        return false;
    }
    // A diverged store is repaired in the store, not by an apply: every
    // strategy resolves through the store it would have to repair, and the
    // fork strategy rides on the same broken baseline, so the run reports the
    // plan and leaves the skill behind under every strategy. The skip reason
    // names the standalone fork that keeps the stored bytes.
    if plan.store_diverged {
        return false;
    }
    match plan.outcome {
        UpdateCheckOutcome::UpdateAvailable => true,
        UpdateCheckOutcome::Blocked => {
            for_document
                || matches!(
                    strategy,
                    crate::apply::ApplyStrategy::Overlay
                        | crate::apply::ApplyStrategy::Discard
                        | crate::apply::ApplyStrategy::Fork
                )
                // The gate refuses every strategy that moves the revision, so
                // a gated skill becomes a target under clean too and the
                // engine returns the documented trust-gate refusal.
                || (strategy == crate::apply::ApplyStrategy::Clean && gate_blocks_apply(plan))
        }
        UpdateCheckOutcome::LocalSource | UpdateCheckOutcome::CommitPinned => {
            plan.local_modification_detected
        }
        UpdateCheckOutcome::UpToDate => {
            // Nothing upstream moves, so the skill contributes a target only
            // for the edits a repair strategy can resolve. A clean apply is
            // refused earlier in the run, because it must not guess.
            plan.local_modification_detected
                && (for_document
                    || matches!(
                        strategy,
                        crate::apply::ApplyStrategy::Overlay
                            | crate::apply::ApplyStrategy::Discard
                            | crate::apply::ApplyStrategy::Fork
                    ))
        }
        UpdateCheckOutcome::Detached | UpdateCheckOutcome::Failed => false,
    }
}

/// Whether the trust gate refuses an apply that moves this skill upstream.
fn gate_blocks_apply(plan: &SkillUpdatePlan) -> bool {
    plan.trust
        .as_ref()
        .is_some_and(|trust| trust.blocks(crate::trust::TrustBlockedAction::ApplyUpdate))
}

/// Whether one check plan refuses a clean apply before the engine runs.
///
/// A detached skill never refuses: its ownership state is not an edit, it
/// contributes no apply target under any strategy, and the skip warning names
/// it instead. A gated skill does not refuse here either, because the engine's
/// trust gate covers the clean strategy too and names the fork the gate allows.
fn blocks_clean_apply(plan: &SkillUpdatePlan) -> bool {
    plan.outcome != UpdateCheckOutcome::Detached
        && plan.local_modification_detected
        && !gate_blocks_apply(plan)
}

/// Skip reason for a local source whose edits need a repair strategy.
const LOCAL_SOURCE_STRATEGY_HINT: &str = "local sources track no upstream revision to apply; \
                                           re-run with '--strategy overlay' to keep the edits, \
                                           '--strategy discard' to back them up and drop them, or \
                                           '--strategy fork' to take local ownership";

/// Why a local source reports no update check.
const NO_UPSTREAM_CHECK_NOTE: &str = "local sources do not support upstream update checks";

/// Why one skill contributes no target to a run.
///
/// `for_document` says the run writes a plan document, so the skill contributes
/// no plan instead of being left out of an apply.
fn skip_reason(
    plan: &SkillUpdatePlan,
    strategy: crate::apply::ApplyStrategy,
    for_document: bool,
) -> String {
    if plan.store_diverged {
        return format!(
            "the import store behind '{}' no longer hashes to the pinned content, so no strategy \
             may apply it; {}",
            plan.skill,
            crate::apply::DriftReason::DivergedImportStore.repair(
                &plan.skill,
                crate::apply::RepairCommands::DiscardOnly,
                plan.scope,
            )
        );
    }
    match plan.outcome {
        UpdateCheckOutcome::LocalSource if plan.local_modification_detected => {
            LOCAL_SOURCE_STRATEGY_HINT.to_string()
        }
        UpdateCheckOutcome::LocalSource => format!(
            "local sources do not support upstream update checks, and '{}' has no unrecorded \
             edits to repair with the {} strategy",
            plan.skill,
            strategy.as_str()
        ),
        UpdateCheckOutcome::CommitPinned if plan.local_modification_detected => {
            format!(
                "'{}' is pinned to a commit, so upstream tracking is unavailable and only its \
                 unrecorded edits can be repaired; re-run with '--strategy overlay' to keep \
                 them, '--strategy discard' to back them up and drop them, or '--strategy \
                 fork' to take local ownership",
                plan.skill
            )
        }
        UpdateCheckOutcome::CommitPinned => format!(
            "'{}' is pinned to a commit, so upstream tracking is unavailable and no upstream \
             revision can move with the {} strategy",
            plan.skill,
            strategy.as_str()
        ),
        UpdateCheckOutcome::UpToDate
        | UpdateCheckOutcome::Detached
        | UpdateCheckOutcome::Blocked
        | UpdateCheckOutcome::Failed
        | UpdateCheckOutcome::UpdateAvailable => format!(
            "check outcome was '{}', so nothing was {}",
            outcome_label(plan.outcome),
            if for_document { "planned" } else { "applied" }
        ),
    }
}

/// Stable label for one update-check outcome.
fn outcome_label(outcome: UpdateCheckOutcome) -> &'static str {
    match outcome {
        UpdateCheckOutcome::UpdateAvailable => "update-available",
        UpdateCheckOutcome::UpToDate => "up-to-date",
        UpdateCheckOutcome::Blocked => "blocked",
        UpdateCheckOutcome::Detached => "detached",
        UpdateCheckOutcome::LocalSource => "local-source",
        UpdateCheckOutcome::CommitPinned => "commit-pinned",
        UpdateCheckOutcome::Failed => "failed",
    }
}

/// Human-readable summary of one apply run.
fn apply_summary(report: &crate::apply::UpdateApplyReport) -> String {
    let mut summary = format!(
        "Applied {} update{} with the {} strategy, skipped {}.",
        report.applied.len(),
        if report.applied.len() == 1 { "" } else { "s" },
        report.strategy,
        report.skipped.len()
    );
    if !report.preserved.is_empty() {
        summary.push_str(&format!(
            " Preserved {} local edit{}.",
            report.preserved.len(),
            if report.preserved.len() == 1 { "" } else { "s" }
        ));
    }
    if !report.discarded.is_empty() {
        summary.push_str(&format!(
            " Backed up and discarded {} local edit{}.",
            report.discarded.len(),
            if report.discarded.len() == 1 { "" } else { "s" }
        ));
    }
    summary
}

/// Write the immutable plan document for the selected targets.
///
/// `checks` carries every check plan the run produced, not only the plans of
/// the skills that contribute a target, because the missing-projection advice
/// must reach a detached skill or an up-to-date import the document leaves out.
fn write_plan_document(
    context: &AppContext,
    request: &UpdateRequest,
    checks: &[SkillUpdatePlan],
    targets: &[crate::apply::UpdateApplyTarget],
    skipped: &[crate::apply::SkippedUpdate],
    path: &Path,
) -> Result<AppResponse, AppError> {
    let strategy = request.effective_strategy();
    let mut plans = Vec::with_capacity(targets.len());
    for target in targets {
        // A target that repairs edits instead of moving a revision gets the
        // plan that describes that repair: a local source tracks no upstream
        // revision, and an up-to-date check leaves a git import on its pin.
        plans.push(if crate::apply::repairs_unrecorded_edits(target) {
            crate::apply::build_edit_repair_mutation_plan(context, target, strategy)?
        } else {
            crate::apply::build_update_mutation_plan(context, target, strategy)?
        });
    }

    let document = crate::apply::PlanDocument::new(
        &context.working_directory,
        strategy.as_str(),
        plans.clone(),
    );
    let contents = document.to_json_string()?;
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).map_err(|source| AppError::FilesystemOperation {
            action: "create plan document parent directory",
            path: parent.to_path_buf(),
            source,
        })?;
    }
    fs::write(path, contents).map_err(|source| AppError::FilesystemOperation {
        action: "write plan document",
        path: path.to_path_buf(),
        source,
    })?;

    let digests: Vec<&str> = plans.iter().map(|plan| plan.digest.as_str()).collect();
    let mut response = AppResponse::success("update")
        .with_summary(format!(
            "Wrote {} plan{} to {} without changing anything.",
            plans.len(),
            if plans.len() == 1 { "" } else { "s" },
            planner_display_path(context, path)
        ))
        .with_data(json!({
            "plan_document": planner_display_path(context, path),
            "strategy": strategy.as_str(),
            "plans": plans,
            "plan_digests": digests,
            "plan_skipped": skipped,
        }));
    // A written plan is what a later `--plan-file` apply runs, so the trust
    // gate decision travels with the same warnings the check reports.
    for target in targets {
        if let Some(trust) = target.summary.trust.as_ref() {
            for warning in &trust.warnings {
                response = response.with_warning(warning);
            }
        }
    }
    // A document records the revision move, and the apply that runs it picks
    // its own strategy, so a blocked skill still contributes a plan. The
    // refusal a non-repair strategy meets is never silent: the warning names
    // the strategies that resolve the edits and that the gate allows.
    for target in targets {
        if edits_block_strategy(target, strategy) {
            response = response.with_warning(unresolvable_edits_warning(target, strategy));
        }
    }
    // The document changes nothing and rebuilds no projection, so a projection
    // the workspace is missing stays missing until a sync or a later apply
    // rebuilds it. Every check plan warns, because a skill that contributes no
    // target, such as a detached import or an up-to-date one without edits,
    // leaves the same missing projection behind as the plain check the run
    // replaces.
    for plan in checks {
        for missing in &plan.missing_projections {
            response =
                response.with_warning(missing_projection_warning(&plan.skill, missing, plan.scope));
        }
    }

    Ok(response)
}

/// Whether the recorded strategy cannot resolve a target's unmanaged edits.
///
/// `clean` refuses to guess and `skip` changes nothing, so a plan for a skill
/// with unmanaged edits only applies after one of the repair strategies is
/// chosen at apply time.
fn edits_block_strategy(
    target: &crate::apply::UpdateApplyTarget,
    strategy: crate::apply::ApplyStrategy,
) -> bool {
    target.summary.local_modification_detected
        && matches!(
            strategy,
            crate::apply::ApplyStrategy::Clean | crate::apply::ApplyStrategy::Skip
        )
}

/// Warning for a written plan the recorded strategy cannot apply as-is.
fn unresolvable_edits_warning(
    target: &crate::apply::UpdateApplyTarget,
    strategy: crate::apply::ApplyStrategy,
) -> String {
    let skill = target.skill.skill_id.as_str();
    // The plan targets a skill the store records, so only the gate decides
    // which strategies the advice may name.
    let commands = if gate_blocks_apply(&target.summary) {
        crate::apply::RepairCommands::GatedUpdateStrategies
    } else {
        crate::apply::RepairCommands::UpdateStrategies
    };

    format!(
        "'{}' holds unmanaged local edits that the {} strategy cannot resolve; {}",
        skill,
        strategy.as_str(),
        crate::apply::DriftReason::DirtyProjection.repair(skill, commands, target.skill.scope)
    )
}

/// Apply exactly one written plan document.
fn apply_plan_document(
    context: &AppContext,
    request: &UpdateRequest,
    path: &Path,
) -> Result<AppResponse, AppError> {
    let document = crate::apply::PlanDocument::load_from_path(path)?;
    let strategy = request.strategy.unwrap_or_else(|| {
        crate::apply::ApplyStrategy::parse(&document.strategy).unwrap_or_default()
    });

    let manifest = WorkspaceManifest::load_required_from_workspace(context, "update")?;
    // An apply answers from the same pins the check read, so a missing
    // lockfile refuses with the same repairs instead of an empty baseline.
    let lockfile = WorkspaceLockfile::load_required_from_workspace(context, "update")?;
    let store = LocalStateStore::open_default_for(&context.working_directory)?;
    let candidate_map = imported_candidate_map(context, &manifest, &lockfile, &store)?;
    let mut targets = Vec::with_capacity(document.plans.len());
    let mut skipped = Vec::new();

    for plan in &document.plans {
        let Some(skill) = plan.skill.clone() else {
            continue;
        };
        let Some(proposed) = plan
            .proposed
            .iter()
            .find(|proposed| proposed.import_id == skill.skill_id)
        else {
            skipped.push(crate::apply::SkippedUpdate {
                skill: skill.skill_id.clone(),
                scope: skill.scope,
                reason: "the plan proposes no change for this skill".to_string(),
            });
            continue;
        };
        let Some(entry) = lockfile.imports.get(&skill.skill_id) else {
            return Err(AppError::StalePlan {
                plan_id: plan.id.clone(),
                message: format!(
                    "imports.{} is missing from the lockfile, so the plan cannot be applied",
                    skill.skill_id
                ),
            });
        };
        // The document was written against the import's pinned pre-state. A
        // store that diverged since would be rebuilt by the apply, which drops
        // the edit the divergence carries, so the document is refused for that
        // import with the same repairs a direct check names.
        if let Some(divergence) = candidate_map.store_divergence(&skill) {
            return Err(AppError::StalePlan {
                plan_id: plan.id.clone(),
                message: resolver::store_divergence_error(
                    &divergence.import_id,
                    &divergence.pinned_content_hash,
                    &divergence.observed_content_hash,
                ),
            });
        }
        // A plan that proposes no revision repairs the unrecorded edits of a
        // pinned import instead of moving it upstream: a local source tracks no
        // revision to move to, and an up-to-date check leaves a git import on
        // the revision it pins.
        let edit_repair = proposed.revision.is_none();
        let target_revision = match proposed.revision.clone() {
            Some(revision) => revision,
            None => entry.revision.resolved.clone(),
        };

        // The document was written against a pre-state the workspace may have
        // left behind, and no review or strategy can make an outdated document
        // applicable again. The staleness refusal therefore runs before the
        // trust gate is consulted, because a gate that fires first names a
        // review that cannot unblock this apply.
        plan.verify_against(context)?;

        // A plan document carries no trust decision, so the gate is
        // recomputed from the current install state here and enforced by the
        // apply engine. A repair keeps the pinned revision, and the gate only
        // holds back applies that move an import upstream, so it has no
        // decision to make for one.
        let trust = if edit_repair {
            None
        } else {
            Some(update_apply_gate_trust(
                &store,
                &candidate_map.candidates,
                &skill,
                &target_revision,
            )?)
        };

        targets.push(crate::apply::UpdateApplyTarget {
            skill,
            source_url: entry.source.url.clone(),
            target_revision: target_revision.clone(),
            expected: plan
                .expected
                .iter()
                .find(|expected| expected.import_id == proposed.import_id)
                .cloned()
                .ok_or_else(|| AppError::TamperedPlan {
                    plan_id: plan.id.clone(),
                    message: format!(
                        "the plan records no expected state for '{}'",
                        proposed.import_id
                    ),
                })?,
            summary: SkillUpdatePlan {
                skill: proposed.import_id.clone(),
                scope: proposed_scope(&proposed.import_id, &store)?,
                checked_at: plan.created_at.clone(),
                source: UpdateSourceSummary {
                    kind: entry.source.kind,
                    url: entry.source.url.clone(),
                    subpath: entry.source.subpath.as_str().to_string(),
                },
                pinned_revision: entry.revision.resolved.clone(),
                latest_revision: if edit_repair {
                    None
                } else {
                    Some(target_revision)
                },
                outcome: if edit_repair {
                    repair_outcome(entry.source.kind)
                } else {
                    UpdateCheckOutcome::UpdateAvailable
                },
                overlay_detected: false,
                overlay_changed_since_recorded_state: false,
                local_modification_detected: false,
                recommended_action: UpdateAction::Apply,
                available_actions: vec![UpdateAction::Apply, UpdateAction::Skip],
                modifications: Vec::new(),
                missing_projections: Vec::new(),
                trust,
                notes: Vec::new(),
                store_diverged: false,
            },
        });
    }

    // What the document itself contributed decides the no-op warning below. The
    // skip strategy can empty the report even though the document named skills,
    // so both facts are read before the apply consumes the targets.
    let document_holds_no_targets = targets.is_empty();
    let document_skipped_nothing = skipped.is_empty();
    let mut report = crate::apply::run_update_apply(context, targets, strategy)?;
    report.skipped.append(&mut skipped);

    let mut response = AppResponse::success("update")
        .with_summary(apply_summary(&report))
        .with_data(json!({
            "plan_document": planner_display_path(context, path),
            "strategy": strategy.as_str(),
            "applied": report.applied,
            "apply_skipped": report.skipped,
            "preserved": report.preserved,
            "discarded": report.discarded,
        }));
    for preserved in &report.preserved {
        response = response.with_warning(format!(
            "preserved local edits at '{}'; {}",
            preserved.path, preserved.repair
        ));
    }
    for warning in noop_apply_warnings(&report, strategy) {
        response = response.with_warning(warning);
    }
    if let Some(warning) = empty_plan_document_warning(
        &document,
        document_holds_no_targets,
        document_skipped_nothing,
    ) {
        response = response.with_warning(warning);
    }

    Ok(response)
}

/// Check outcome a plan document's edit-repair plan reconstructs for one source.
///
/// The document records no outcome, and the apply routes a target by it, so a
/// repair plan reports the outcome whose check contributes an edit repair: a
/// local source by definition, and an up-to-date check for a git import.
fn repair_outcome(kind: SourceKind) -> UpdateCheckOutcome {
    if kind == SourceKind::Git {
        UpdateCheckOutcome::UpToDate
    } else {
        UpdateCheckOutcome::LocalSource
    }
}

fn proposed_scope(skill_id: &str, store: &LocalStateStore) -> Result<ManagedScope, AppError> {
    // Repair advice classifies a skill with the same rule, so a copy `update`
    // refuses never names an update command as its repair.
    crate::trust::install_record_scope(store, skill_id)?.ok_or_else(|| {
        AppError::ResolutionValidation {
            message: format!("skill '{skill_id}' is not installed"),
        }
    })
}

fn planner_display_path(context: &AppContext, path: &Path) -> String {
    display_path(context, path)
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

/// One projection destination the manifest plan expects that is absent.
///
/// An absent projection is not a local edit: nothing inside a generated copy
/// changed, the copy simply is not there. `skillctl sync` rebuilds it, so the
/// update report names the destination instead of blocking the update.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct MissingProjection {
    /// Runtime targets that expect the projection at this path.
    pub targets: Vec<String>,
    /// Physical root that misses the projection.
    pub root: String,
    /// Projected path that is absent.
    pub path: String,
}

impl MissingProjection {
    /// Stable comma-separated label for the targets that expect this path.
    fn target_label(&self) -> String {
        self.targets.join(", ")
    }
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
    /// Whether the managed overlay changed since the state the install records.
    ///
    /// The overlay moved ahead of the recorded pin, so the effective content no
    /// longer names what a generated copy should hold. The copy audit reads the
    /// content skillctl wrote for each copy instead, which is the baseline the
    /// apply engine's edit repair reads, so a repair this check recommends is a
    /// repair the engine performs.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub overlay_changed_since_recorded_state: bool,
    /// Whether unmanaged local changes were detected.
    pub local_modification_detected: bool,
    /// Next action the user or agent should take.
    pub recommended_action: UpdateAction,
    /// Deterministic set of valid next actions.
    pub available_actions: Vec<UpdateAction>,
    /// Structured local-change details discovered during the check.
    pub modifications: Vec<PlannedModification>,
    /// Projection destinations the manifest plan expects that are absent.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing_projections: Vec<MissingProjection>,
    /// Trust decision for the effective skill during this update check.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trust: Option<SkillTrust>,
    /// Additional explanatory notes for humans and agents.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    /// Whether the import store no longer hashes to the content this check's
    /// pin records.
    ///
    /// A diverged store cannot serve as the baseline of any apply: every
    /// update strategy resolves through the store it would have to repair, and
    /// the fork strategy is no exception, because the check it rides on still
    /// reads the broken baseline. The plan reports the divergence and
    /// contributes no target while the unrelated skills of the same run keep
    /// updating, and the repair advice names the standalone `skillctl fork
    /// <skill>` that copies the stored bytes out first.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub store_diverged: bool,
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

/// Inputs one update check shares across every skill it plans.
struct UpdateCheckInputs<'a> {
    /// Resolved effective-skill candidates by managed skill.
    candidates: &'a BTreeMap<(ManagedScope, String), ResolvedSkillCandidate>,
    /// Imports whose store no longer hashes to the pin, by managed skill.
    diverged_stores: &'a BTreeMap<(ManagedScope, String), resolver::ImportStoreDivergence>,
    /// Recorded local review decisions, which decide `imported-reviewed`.
    reviews: &'a crate::trust::ReviewIndex,
    /// The pins the check compares against, which also name the discovery
    /// entry a well-known import was installed from.
    lockfile: &'a WorkspaceLockfile,
    /// Timestamp shared by every check in the run.
    checked_at: &'a str,
}

impl UpdateCheckInputs<'_> {
    /// The resolved effective candidate for one managed skill, when one exists.
    fn candidate(&self, skill: &ManagedSkillRef) -> Option<&ResolvedSkillCandidate> {
        self.candidates.get(&(skill.scope, skill.skill_id.clone()))
    }

    /// The store divergence recorded for one managed skill, when one exists.
    fn store_divergence(
        &self,
        skill: &ManagedSkillRef,
    ) -> Option<&resolver::ImportStoreDivergence> {
        self.diverged_stores
            .get(&(skill.scope, skill.skill_id.clone()))
    }
}

/// Trust decision for one managed import during an update check.
///
/// The resolved effective candidate decides when the workspace can produce
/// one, because it carries the file list the script detection needs. Anything
/// else falls back to the recorded install state. The recorded reviews decide
/// whether an approved import resolves as `imported-reviewed`, which the gate
/// leaves alone.
fn trust_decision(
    install: &InstallRecord,
    candidate: Option<&ResolvedSkillCandidate>,
    reviews: &crate::trust::ReviewIndex,
) -> Result<SkillTrust, AppError> {
    match candidate {
        Some(candidate) => Ok(crate::trust::trust_for_candidate(
            candidate,
            reviews.approved_for_candidate(candidate),
        )),
        None => crate::trust::trust_for_install_record(
            install,
            false,
            None,
            reviews.approved_for_install(install),
        ),
    }
}

/// Recompute the update-apply trust gate for one managed import.
///
/// A plan document carries no trust decision, so an apply that runs from one
/// rebuilds the gate from the current install state instead of trusting the
/// document. Only a Git import can move to a new upstream revision, so only
/// its gate is evaluated.
///
/// The gate holds back revision moves only. A document that proposes the
/// revision the install already holds describes no move, so it meets no gate.
/// That is the same allowance the engine's own predicate makes in
/// [`crate::apply::ensure_revision_apply_is_allowed`], and the same one a
/// fresh check makes by evaluating the block only while upstream moved. The
/// stale-plan check owns the refusal for such a document instead, because the
/// pre-state it was built against no longer exists.
fn update_apply_gate_trust(
    store: &LocalStateStore,
    candidates: &BTreeMap<(ManagedScope, String), ResolvedSkillCandidate>,
    skill: &ManagedSkillRef,
    target_revision: &str,
) -> Result<SkillTrust, AppError> {
    let install = store
        .install_record(skill)?
        .ok_or_else(|| AppError::ResolutionValidation {
            message: format!(
                "skill '{}' does not have an installed state record",
                skill.skill_id
            ),
        })?;
    let reviews = crate::trust::ReviewIndex::load(store)?;
    let trust = trust_decision(
        &install,
        candidates.get(&(skill.scope, skill.skill_id.clone())),
        &reviews,
    )?;

    if target_revision == install.resolved_revision {
        return Ok(trust);
    }

    Ok(trust.block_apply_update(skill.skill_id.as_str()))
}

fn prepare_update_plan(
    context: &AppContext,
    manifest: &WorkspaceManifest,
    install: &InstallRecord,
    pin: Option<&PinRecord>,
    projections: &[ProjectionRecord],
    candidate: Option<&ResolvedSkillCandidate>,
    check: &UpdateCheckInputs<'_>,
) -> Result<PreparedUpdatePlan, AppError> {
    let checked_at = check.checked_at;
    let mut modifications = Vec::new();
    let mut local_modification_records = Vec::new();
    let mut notes = Vec::new();
    let mut trust = trust_decision(install, candidate, check.reviews)?;

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
        // Ownership moved, but projection health did not: a sync materializes
        // the local copy into every planned destination exactly as it did while
        // the skill was an import, so the check names every destination the
        // workspace lacks the same way `explain` and `doctor` do.
        let missing_projections =
            missing_projection_destinations(context, manifest, install, candidate)?;
        notes.extend(missing_projection_notes(
            &missing_projections,
            install.skill.scope,
        ));
        let notes = deduplicate_notes(notes);
        let note_text = if notes.is_empty() {
            None
        } else {
            Some(notes.join(" "))
        };

        // Ownership moved, but the recorded state may still carry a value an
        // older build wrote while moving to an immutable reference. That value
        // was never observed, so it is withheld instead of being reported as
        // the latest upstream revision.
        let latest_revision = crate::source::reportable_upstream_revision(
            install.upstream_revision.as_ref(),
            &install.resolved_revision,
            active_requested_git_reference(install, pin, candidate),
        )
        .cloned();

        let update_check = UpdateCheckRecord {
            id: None,
            skill: install.skill.clone(),
            checked_at: checked_at.to_string(),
            pinned_revision: install.resolved_revision.clone(),
            latest_revision: latest_revision.clone(),
            outcome: UpdateCheckOutcome::Detached,
            overlay_detected: false,
            local_modification_detected: true,
            // A detached import tracks no revision the gate could hold back.
            apply_blocked_by: None,
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
                latest_revision,
                outcome: UpdateCheckOutcome::Detached,
                overlay_detected: false,
                overlay_changed_since_recorded_state: false,
                local_modification_detected: true,
                recommended_action: UpdateAction::Skip,
                available_actions: vec![UpdateAction::Skip],
                modifications,
                missing_projections,
                trust: Some(trust),
                notes,
                store_diverged: false,
            },
            update_check,
            local_modification_records,
        });
    }

    let overlay_state = overlay_state(context, manifest, install)?;

    // A store that no longer matches its pin is not the baseline the import
    // was installed from, so no check that reads it can answer for the import.
    // Every apply strategy resolves through the store it would have to repair,
    // so the plan reports the divergence instead of proposing a move, and the
    // run's other skills keep updating.
    if let Some(divergence) = check.store_divergence(&install.skill) {
        let missing_projections =
            missing_projection_destinations(context, manifest, install, candidate)?;
        notes.extend(missing_projection_notes(
            &missing_projections,
            install.skill.scope,
        ));
        notes.push(store_divergence_note(divergence, install.skill.scope));
        let notes = deduplicate_notes(notes);
        let note_text = if notes.is_empty() {
            None
        } else {
            Some(notes.join(" "))
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
                latest_revision: None,
                outcome: UpdateCheckOutcome::Blocked,
                overlay_detected: false,
                overlay_changed_since_recorded_state: false,
                local_modification_detected: false,
                recommended_action: UpdateAction::Skip,
                available_actions: vec![UpdateAction::Skip],
                modifications: Vec::new(),
                missing_projections,
                trust: Some(trust),
                notes,
                store_diverged: true,
            },
            update_check: UpdateCheckRecord {
                id: None,
                skill: install.skill.clone(),
                checked_at: checked_at.to_string(),
                pinned_revision: install.resolved_revision.clone(),
                latest_revision: None,
                outcome: UpdateCheckOutcome::Blocked,
                overlay_detected: false,
                local_modification_detected: false,
                apply_blocked_by: None,
                notes: note_text,
            },
            local_modification_records: Vec::new(),
        });
    }

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
    let overlay_changed_since_recorded_state = overlay_state
        .as_ref()
        .is_some_and(|state| state.changed_since_recorded_state);
    let latest_revision;
    let outcome;
    let mut recommended_action;
    let mut available_actions;
    // Why the gate below held back the apply, when it did. The recorded row is
    // the only place the check's look at the upstream tree survives, so a later
    // drift report can tell a refused repair from an allowed one.
    let mut apply_blocked_by: Option<crate::state::UpdateCheckBlockReason> = None;
    // Absence is not an edit and does not depend on the recorded content, so
    // every planned destination the workspace lacks is named even when the
    // overlay changed and the copy audit below is skipped.
    let missing_projections =
        missing_projection_destinations(context, manifest, install, candidate)?;
    notes.extend(missing_projection_notes(
        &missing_projections,
        install.skill.scope,
    ));

    match install.source_kind {
        SourceKind::LocalPath | SourceKind::Archive => {
            latest_revision = None;
            outcome = UpdateCheckOutcome::LocalSource;
            notes.push(NO_UPSTREAM_CHECK_NOTE.to_string());

            // There is no upstream to compare against, but the generated
            // copies of the import can still hold unrecorded edits. Those
            // edits are repairable through the same apply strategies a git
            // import uses, so they are detected and reported the same way.
            let audit = audit_generated_copies(
                context,
                manifest,
                install,
                projections,
                candidate,
                overlay_state.as_ref(),
                checked_at,
            )?;
            modifications.extend(audit.modifications);
            local_modification_records.extend(audit.records);
            notes.extend(audit.notes);

            let (action, actions, advice) = recommendation_without_upstream(&modifications);
            recommended_action = action;
            available_actions = actions;
            notes.extend(advice);
        }
        SourceKind::WellKnown => {
            // The lockfile names the entry the import was installed from, and
            // the check re-reads the index to learn the digest it declares
            // now. A changed digest is an update, exactly the way a moved
            // remote revision is one for a Git import.
            let recorded = check
                .lockfile
                .imports
                .get(&install.skill.skill_id)
                .and_then(|import| import.discovery.as_ref());
            match well_known_index_lookup(install, recorded) {
                WellKnownLookup::Declared { entry, origin_note } => {
                    latest_revision = Some(entry.digest.as_str().to_string());
                    if let Some(note) = origin_note {
                        notes.push(note);
                    }

                    let audit = audit_generated_copies(
                        context,
                        manifest,
                        install,
                        projections,
                        candidate,
                        overlay_state.as_ref(),
                        checked_at,
                    )?;
                    modifications.extend(audit.modifications);
                    local_modification_records.extend(audit.records);
                    notes.extend(audit.notes);

                    let update_available = entry.digest.as_str() != pinned_revision;
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
                            "unmanaged projected-copy edits are blocking an update from digest {} \
                             to digest {}",
                            pinned_revision,
                            entry.digest.as_str()
                        ));
                    } else if update_available {
                        outcome = UpdateCheckOutcome::UpdateAvailable;
                        recommended_action = UpdateAction::Apply;
                        available_actions = vec![UpdateAction::Apply, UpdateAction::Skip];
                        notes.push(format!(
                            "the discovery index now declares the digest {}, which differs from \
                             the pinned digest {}",
                            entry.digest.as_str(),
                            pinned_revision
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
                                "no upstream artifact update is available, but unmanaged \
                                 projected-copy edits were detected"
                                    .to_string(),
                            );
                        } else {
                            recommended_action = UpdateAction::Skip;
                            available_actions = vec![UpdateAction::Skip];
                            notes.push(
                                "the pinned artifact digest already matches the discovery index"
                                    .to_string(),
                            );
                        }
                    }

                    if update_available {
                        // The gate holds back every apply that moves the import
                        // to the new digest, including the repair strategies
                        // that resolve unmanaged edits first. The artifact the
                        // move would install is not downloaded during a check,
                        // so the incoming tree cannot be inspected the way a
                        // Git checkout can; the apply engine derives that half
                        // of the gate from the artifact it stages itself.
                        trust = trust.block_apply_update(install.skill.skill_id.as_str());
                        if trust.blocks(crate::trust::TrustBlockedAction::ApplyUpdate) {
                            apply_blocked_by =
                                Some(crate::state::UpdateCheckBlockReason::InstalledScripts);
                        }
                        if !trust.blocked_actions.is_empty() {
                            recommended_action = UpdateAction::Skip;
                            available_actions = if local_modification_detected {
                                vec![
                                    UpdateAction::Detach,
                                    UpdateAction::PublishVariant,
                                    UpdateAction::Skip,
                                ]
                            } else {
                                vec![UpdateAction::Skip]
                            };
                            notes.push(format!(
                                "trust gate is blocking apply for '{}' until the import is \
                                 reviewed or forked; record a local review with 'skillctl \
                                 review {} --approve'",
                                install.skill.skill_id, install.skill.skill_id
                            ));
                        }
                    }
                }
                WellKnownLookup::NotDeclared => {
                    latest_revision = None;
                    outcome = UpdateCheckOutcome::Failed;
                    recommended_action = UpdateAction::Skip;
                    available_actions = vec![UpdateAction::Skip];
                    notes.push(format!(
                        "the discovery index at {} no longer declares '{}', so no upstream \
                         artifact can be observed",
                        install.source_url, install.skill.skill_id
                    ));
                }
                WellKnownLookup::Failed(message) => {
                    latest_revision = None;
                    outcome = UpdateCheckOutcome::Failed;
                    recommended_action = UpdateAction::Skip;
                    available_actions = vec![UpdateAction::Skip];
                    notes.push(message);
                }
            }
        }
        SourceKind::Git => {
            let requested_reference = active_requested_git_reference(install, pin, candidate);
            let reference = requested_reference.unwrap_or("HEAD");
            let lookup = match latest_git_revision(&install.source_url, reference) {
                Ok(Some(revision)) => UpstreamLookup::Observed(revision),
                Ok(None)
                    if crate::source::reference_pins_immutable_revision(
                        reference,
                        &pinned_revision,
                    ) =>
                {
                    UpstreamLookup::CommitPinned
                }
                Ok(None) => {
                    UpstreamLookup::Failed(no_revision_note(&install.source_url, reference))
                }
                Err(message) => UpstreamLookup::Failed(message),
            };
            match lookup {
                UpstreamLookup::Observed(upstream_revision) => {
                    latest_revision = Some(upstream_revision.clone());
                    let audit = audit_generated_copies(
                        context,
                        manifest,
                        install,
                        projections,
                        candidate,
                        overlay_state.as_ref(),
                        checked_at,
                    )?;
                    modifications.extend(audit.modifications);
                    local_modification_records.extend(audit.records);
                    notes.extend(audit.notes);

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

                    if update_available {
                        // The gate holds back every apply that moves the import
                        // to the new revision, including the repair strategies
                        // that resolve unmanaged edits first, so it is
                        // evaluated for a blocked check too.
                        trust = trust.block_apply_update(install.skill.skill_id.as_str());
                        if trust.blocks(crate::trust::TrustBlockedAction::ApplyUpdate) {
                            apply_blocked_by =
                                Some(crate::state::UpdateCheckBlockReason::InstalledScripts);
                        } else if trust.effective_state
                            == crate::trust::TrustState::ImportedUnreviewed
                            && crate::trust::upstream_revision_contains_scripts(
                                &install.source_url,
                                &upstream_revision,
                                &install.source_subpath,
                            )
                        {
                            // The installed content is script-free, so the
                            // block above cannot fire, while the revision the
                            // apply would move to introduces executable content
                            // no recorded review covers. Only an unreviewed
                            // import is held back, because a recorded approval
                            // licenses the move.
                            trust = trust.block_apply_update_for_incoming_scripts(
                                install.skill.skill_id.as_str(),
                                &upstream_revision,
                            );
                            apply_blocked_by =
                                Some(crate::state::UpdateCheckBlockReason::IncomingScripts);
                        }
                        if !trust.blocked_actions.is_empty() {
                            recommended_action = UpdateAction::Skip;
                            // Only the actions that move the revision are
                            // gated. Resolving edits by taking local ownership
                            // or publishing a variant stays open.
                            available_actions = if local_modification_detected {
                                vec![
                                    UpdateAction::Detach,
                                    UpdateAction::PublishVariant,
                                    UpdateAction::Skip,
                                ]
                            } else {
                                vec![UpdateAction::Skip]
                            };
                            notes.push(format!(
                                "trust gate is blocking apply for '{}' until the import is \
                                 reviewed or forked; record a local review with 'skillctl \
                                 review {} --approve'",
                                install.skill.skill_id, install.skill.skill_id
                            ));
                        }
                    }
                }
                UpstreamLookup::CommitPinned => {
                    // The active reference names an immutable revision, so the
                    // source can never answer it with a newer one. The check
                    // reports that tracking is unavailable instead of dressing
                    // the pinned revision up as an upstream observation.
                    latest_revision = None;
                    outcome = UpdateCheckOutcome::CommitPinned;
                    notes.push(format!(
                        "import '{}' is pinned to the immutable revision {}, which names no \
                         upstream ref to observe; upstream tracking is unavailable while the pin \
                         names a commit or a recorded version. Run 'skillctl pin {} \
                         <branch-or-tag>' to resume upstream tracking",
                        install.skill.skill_id, pinned_revision, install.skill.skill_id
                    ));

                    // No upstream can move, but the generated copies of the
                    // import can still hold unrecorded edits, so the check
                    // keeps reading projection health the way a local source
                    // does.
                    let audit = audit_generated_copies(
                        context,
                        manifest,
                        install,
                        projections,
                        candidate,
                        overlay_state.as_ref(),
                        checked_at,
                    )?;
                    modifications.extend(audit.modifications);
                    local_modification_records.extend(audit.records);
                    notes.extend(audit.notes);

                    let (action, actions, advice) = recommendation_without_upstream(&modifications);
                    recommended_action = action;
                    available_actions = actions;
                    notes.extend(advice);
                }
                UpstreamLookup::Failed(message) => {
                    latest_revision = None;
                    outcome = UpdateCheckOutcome::Failed;
                    recommended_action = UpdateAction::Skip;
                    available_actions = vec![UpdateAction::Skip];
                    notes.push(message);
                }
            }
        }
    }

    // A disabled import resolves to no candidate, so nothing projects it and
    // every generated copy it still holds is a leftover. Naming that state
    // keeps the repair advice above honest: the update strategies resolve the
    // edits while the import stays disabled.
    if candidate.is_none()
        && modifications
            .iter()
            .any(|modification| !modification.managed)
        && manifest_disables_import(manifest, install)
    {
        notes.push(format!(
            "import '{}' is disabled, so a sync no longer projects it and every generated copy \
             it still holds is a leftover",
            install.skill.skill_id
        ));
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
        apply_blocked_by,
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
            overlay_changed_since_recorded_state,
            local_modification_detected,
            recommended_action,
            available_actions,
            modifications,
            missing_projections,
            trust: Some(trust),
            notes,
            store_diverged: false,
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

/// Resolved candidates one update run reads, plus the diverged stores it
/// resolved around.
///
/// The graph builds with the store-divergence policy the drift guard uses, so
/// in symlink mode one import whose store no longer matches its pin resolves
/// from its stored bytes instead of failing the whole run. The divergence
/// stays attached to the import it belongs to: a check that names the diverged
/// import still refuses, while the run's other skills keep updating.
struct ImportedCandidates {
    /// Resolved effective-skill candidates by managed skill.
    candidates: BTreeMap<(ManagedScope, String), ResolvedSkillCandidate>,
    /// Imports whose store no longer hashes to the pin, by managed skill.
    diverged_stores: BTreeMap<(ManagedScope, String), resolver::ImportStoreDivergence>,
}

impl ImportedCandidates {
    /// The store divergence recorded for one managed skill, when one exists.
    fn store_divergence(
        &self,
        skill: &ManagedSkillRef,
    ) -> Option<&resolver::ImportStoreDivergence> {
        self.diverged_stores
            .get(&(skill.scope, skill.skill_id.clone()))
    }
}

/// Resolved effective-skill candidates one update check reads.
///
/// A managed import resolves to its imported candidate, because the import
/// decides both the effective content and the upstream the check compares
/// against. A detached or forked install has no import left to resolve: the
/// manifest no longer declares it, and the workspace resolves the projected
/// name to the canonical local copy the fork wrote. That winner is what a sync
/// materializes into every planned destination, so the check needs it to judge
/// projection health. Only a canonical local winner stands in, because an
/// imported winner belongs to a different install and would report this one
/// with an import's trust state.
fn imported_candidate_map(
    context: &AppContext,
    manifest: &WorkspaceManifest,
    lockfile: &WorkspaceLockfile,
    store: &LocalStateStore,
) -> Result<ImportedCandidates, AppError> {
    let request = ResolveWorkspaceRequest::new(
        &context.working_directory,
        imports_store_root()?,
        manifest.clone(),
        lockfile.clone(),
    )
    .with_store_divergence_policy(crate::materialize::store_divergence_policy(manifest));
    let graph = resolver::build_effective_skill_graph(&request)?;
    let mut candidates = BTreeMap::new();

    for candidate in &graph.candidates {
        let Some(import) = candidate.import.as_ref() else {
            continue;
        };
        let scope = managed_scope_from_skill_scope(import.scope);
        candidates.insert((scope, import.id.clone()), candidate.clone());
    }

    let diverged_stores = graph
        .store_divergences
        .iter()
        .map(|divergence| {
            (
                (
                    managed_scope_from_skill_scope(divergence.scope),
                    divergence.import_id.clone(),
                ),
                divergence.clone(),
            )
        })
        .collect();

    for install in store.list_install_records()? {
        if !install.detached && !install.forked {
            continue;
        }

        let key = (install.skill.scope, install.skill.skill_id.clone());
        if candidates.contains_key(&key) {
            continue;
        }

        let Some(winner) = graph
            .projection_for(&install.skill.skill_id)
            .and_then(|projection| projection.winner())
        else {
            continue;
        };
        if winner.import.is_some()
            || managed_scope_from_skill_scope(winner.scope) != install.skill.scope
        {
            continue;
        }

        candidates.insert(key, winner.clone());
    }

    Ok(ImportedCandidates {
        candidates,
        diverged_stores,
    })
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

/// The Git reference an import currently tracks, when one is recorded.
///
/// The manifest import wins while its resolved revision still matches the
/// install, because the workspace declares it; otherwise a recorded pin names
/// the reference the user asked to follow. `None` means the import names no
/// reference, which a check answers through the default `HEAD`.
pub(crate) fn active_requested_git_reference<'a>(
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

/// What one upstream lookup observed for the reference a check tracks.
enum UpstreamLookup {
    /// The source named this revision for the reference.
    Observed(String),
    /// The reference names the pinned commit itself, so no upstream ref can
    /// answer for it.
    CommitPinned,
    /// The lookup produced no observation. The message names why.
    Failed(String),
}

/// What one re-read of the discovery index observed for a well-known import.
enum WellKnownLookup {
    /// The index declares this entry for the skill the import pins.
    Declared {
        entry: crate::discovery::DiscoveryEntry,
        /// Origin-trust note the re-read picked up, when one applies.
        origin_note: Option<String>,
    },
    /// The index no longer declares the skill.
    NotDeclared,
    /// The re-read was refused or failed, with the reason.
    Failed(String),
}

/// Re-read the discovery index a well-known import was installed from.
///
/// The recorded entry names the skill the import selects. The origin-trust
/// policy gates the re-read first, so an origin the policy refuses is never
/// contacted, and a successful re-read from an origin the policy did not verify
/// carries the warning note instead of passing silently.
fn well_known_index_lookup(
    install: &InstallRecord,
    recorded: Option<&crate::lockfile::LockedDiscoveryEntry>,
) -> WellKnownLookup {
    let Some(recorded) = recorded else {
        return WellKnownLookup::Failed(format!(
            "the lockfile records no discovery entry for '{}', so the update check cannot name \
             the index entry it would re-read; a well-known import must record the entry it was \
             installed from",
            install.skill.skill_id
        ));
    };

    let origin_trust =
        match crate::trust::ensure_discovery_origin_permits_acquisition(&install.source_url) {
            Ok(trust) => trust,
            Err(error) => return WellKnownLookup::Failed(error.to_string()),
        };

    match crate::discovery::lookup_index_entry(
        &install.source_url,
        &recorded.skill,
        &crate::discovery::HttpTransport::default(),
    ) {
        Ok(crate::discovery::IndexLookup::Declared(entry)) => WellKnownLookup::Declared {
            entry,
            origin_note: crate::trust::discovery_origin_warning(origin_trust, &install.source_url),
        },
        Ok(crate::discovery::IndexLookup::NotDeclared) => WellKnownLookup::NotDeclared,
        Err(error) => WellKnownLookup::Failed(error.to_string()),
    }
}

/// Ask one git source which revision `reference` names.
///
/// `Ok(None)` says the source answered but named no revision for the reference,
/// which stays distinct from the transport and command failures `Err` carries.
fn latest_git_revision(source_url: &str, reference: &str) -> Result<Option<String>, String> {
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
    Ok(parse_ls_remote_revision(stdout.as_ref()))
}

/// Note for a lookup the source answered with no revision at all.
fn no_revision_note(source_url: &str, reference: &str) -> String {
    if reference == "HEAD" {
        format!("upstream revision lookup for '{source_url}' returned no HEAD")
    } else {
        format!(
            "upstream revision lookup for '{source_url}' returned no revision for '{reference}'"
        )
    }
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

/// Every projection destination the manifest plan expects that is absent.
///
/// The plan assignments decide which destinations a check expects, exactly as
/// `explain` and `doctor` read them, so a root the target set added before a
/// `sync` ran is named here even though no projection record covers it. A root
/// the plan dropped is not named, because `sync` prunes that root instead of
/// rebuilding it. Records cannot drive this set: a destination without a record
/// is precisely the case that has to be reported.
///
/// A skill the resolver no longer selects, for example a disabled import, has
/// nothing to materialize, so its destinations are not missing.
fn missing_projection_destinations(
    context: &AppContext,
    manifest: &WorkspaceManifest,
    install: &InstallRecord,
    candidate: Option<&ResolvedSkillCandidate>,
) -> Result<Vec<MissingProjection>, AppError> {
    if candidate.is_none() {
        return Ok(Vec::new());
    }

    let mut missing = Vec::new();
    for root in crate::materialize::planned_physical_roots(
        context,
        manifest,
        target_scope_for(install.skill.scope),
    )? {
        let destination =
            resolve_runtime_root_path(context, &root.path)?.join(&install.skill.skill_id);
        if ProjectionObservation::observe(&destination) != ProjectionObservation::Missing {
            continue;
        }

        // Targets that share one physical root are merged into a single
        // destination, because `sync` rebuilds the copy once for all of them.
        missing.push(MissingProjection {
            targets: root
                .targets
                .iter()
                .map(|target| target.as_str().to_string())
                .collect(),
            root: root.path.clone(),
            path: display_path(context, &destination),
        });
    }

    Ok(missing)
}

/// Target scope that plans and materializes one managed scope.
fn target_scope_for(scope: ManagedScope) -> TargetScope {
    match scope {
        ManagedScope::Workspace => TargetScope::Workspace,
        ManagedScope::User => TargetScope::User,
    }
}

/// Whether the manifest declares one install's import as disabled.
///
/// A disabled declaration is the one state that leaves an install record
/// without a resolved candidate, because the resolver reads enabled imports
/// only. An import the manifest no longer declares at all keeps that silence,
/// so the note stays away from it.
fn manifest_disables_import(manifest: &WorkspaceManifest, install: &InstallRecord) -> bool {
    let scope = match install.skill.scope {
        ManagedScope::Workspace => crate::manifest::ManifestScope::Workspace,
        ManagedScope::User => crate::manifest::ManifestScope::User,
    };
    manifest.imports.iter().any(|import| {
        import.id == install.skill.skill_id && import.scope == scope && !import.enabled
    })
}

/// Advice that names every absent projection destination of one check.
///
/// The advice names the `sync` command qualified for `scope`, because a plain
/// run cannot materialize a root another scope plans.
fn missing_projection_notes(missing: &[MissingProjection], scope: ManagedScope) -> Vec<String> {
    let sync = crate::apply::scoped_sync_command(scope);
    missing
        .iter()
        .map(|entry| {
            format!(
                "no projection exists at '{}' for {}; run '{sync}' to materialize the \
                 missing projection",
                entry.path,
                entry.target_label()
            )
        })
        .collect()
}

/// Run-level warning for one absent projection destination of one skill.
fn missing_projection_warning(
    skill: &str,
    missing: &MissingProjection,
    scope: ManagedScope,
) -> String {
    format!(
        "skill '{skill}' has no projection at '{}' for {}; run '{}' to materialize \
         the missing projection",
        missing.path,
        missing.target_label(),
        crate::apply::scoped_sync_command(scope)
    )
}

/// Run-level warning for one skill whose overlay moved past its recorded pin.
///
/// The re-pin advice matches the warning `sync` reports for the same state, so
/// one overlay change keeps one command across every surface that names it.
fn overlay_drift_warning(plan: &SkillUpdatePlan) -> String {
    format!(
        "skill '{}' has a managed overlay that changed since the recorded state; run 'skillctl \
         override {}' to re-pin the change after reviewing it, then commit the lockfile together \
         with the overlay",
        plan.skill, plan.skill
    )
}

/// Plan note for one import whose store no longer hashes to its pin.
///
/// The note carries the same repair the drift guard reports, so one state
/// keeps one command list across `sync`, `doctor`, and `update`.
fn store_divergence_note(
    divergence: &resolver::ImportStoreDivergence,
    scope: ManagedScope,
) -> String {
    format!(
        "the import store behind '{}' no longer hashes to the pinned content: the lockfile pins \
         {} but the store holds {}; {}",
        divergence.import_id,
        divergence.pinned_content_hash,
        divergence.observed_content_hash,
        crate::apply::DriftReason::DivergedImportStore.repair(
            &divergence.import_id,
            crate::apply::RepairCommands::DiscardOnly,
            scope,
        )
    )
}

/// Unrecorded edits one check found in the generated copies of an import.
struct CopyAudit {
    modifications: Vec<PlannedModification>,
    records: Vec<LocalModificationRecord>,
    notes: Vec<String>,
}

/// Audit the generated copies of one import for unrecorded edits.
///
/// While the managed overlay changed since the recorded state, the audit reads
/// the content skillctl wrote for each copy instead of the effective content,
/// because the overlay moved that content ahead of what a copy was written
/// from. `checked_at` stamps every local modification record the audit writes.
fn audit_generated_copies(
    context: &AppContext,
    manifest: &WorkspaceManifest,
    install: &InstallRecord,
    projections: &[ProjectionRecord],
    candidate: Option<&ResolvedSkillCandidate>,
    overlay_state: Option<&OverlayState>,
    checked_at: &str,
) -> Result<CopyAudit, AppError> {
    if overlay_state.is_some_and(|state| state.changed_since_recorded_state) {
        return audit_copies_against_written_content(
            context, manifest, install, candidate, checked_at,
        );
    }

    let mut modifications = Vec::new();
    let mut records = Vec::new();
    for modification in
        detect_projected_copy_modifications(context, manifest, install, projections, candidate)?
    {
        records.push(LocalModificationRecord {
            id: None,
            skill: install.skill.clone(),
            detected_at: checked_at.to_string(),
            kind: modification.kind,
            path: modification.path.clone(),
            details: modification.details.clone(),
        });
        modifications.push(modification);
    }

    Ok(CopyAudit {
        modifications,
        records,
        notes: Vec::new(),
    })
}

/// Note the overlay-drift audit adds to every plan it runs on.
const OVERLAY_DRIFT_AUDIT_NOTE: &str = "generated copies are judged against the content skillctl \
                                        wrote for them, not the effective content, while the \
                                        managed overlay changed since the recorded state";

/// Detail the overlay-drift audit records for one edited copy.
const OVERLAY_DRIFT_COPY_DETAIL: &str = "projected runtime copy differs from the content skillctl \
                                         recorded for it";

/// Audit the generated copies of one import against the content skillctl wrote.
///
/// The managed overlay moved ahead of the state the install records, so the
/// effective content no longer names what a copy should hold: a copy that was
/// never edited may simply predate the overlay change, and reading the
/// effective content would report that copy as an edit it does not hold.
///
/// Each copy records the bytes it was written from, so that content is its own
/// baseline. Every documented root is therefore judged the way the apply
/// engine's edit repair judges it: [`crate::apply::inspect_projection`] with
/// the content the current winner would materialize, which only a copy that
/// records no digest of its own consults. A check that reports an edit this
/// way names a repair the engine performs, so no drift report advises a
/// command that changes nothing.
fn audit_copies_against_written_content(
    context: &AppContext,
    manifest: &WorkspaceManifest,
    install: &InstallRecord,
    candidate: Option<&ResolvedSkillCandidate>,
    checked_at: &str,
) -> Result<CopyAudit, AppError> {
    // The digest must be computed the way a materialization of the current
    // mode records it, because a symlink projection hashes its link targets
    // instead of its content.
    let desired = candidate
        .map(|candidate| {
            crate::apply::desired_projection_digest(
                candidate,
                manifest.projection.mode == ProjectionMode::Symlink,
            )
        })
        .transpose()?;

    let scope = target_scope_for(install.skill.scope);
    let mut modifications = Vec::new();
    let mut records = Vec::new();
    for root in crate::materialize::edit_repair_root_paths(context, manifest, scope)? {
        let dir = root.join(&install.skill.skill_id);
        // The bundled skill owns its own copies in every documented user root,
        // so the repair engine must not be told to resolve them.
        if crate::materialize::is_bundled_projection_directory(&dir)? {
            continue;
        }
        if let crate::apply::ProjectionState::Drift(drift) =
            crate::apply::inspect_projection(&dir, desired.as_deref())?
        {
            records.push(LocalModificationRecord {
                id: None,
                skill: install.skill.clone(),
                detected_at: checked_at.to_string(),
                kind: LocalModificationKind::ProjectedCopy,
                path: Some(display_path(context, &drift.path)),
                details: Some(OVERLAY_DRIFT_COPY_DETAIL.to_string()),
            });
            modifications.push(PlannedModification {
                kind: LocalModificationKind::ProjectedCopy,
                managed: false,
                path: Some(display_path(context, &drift.path)),
                details: Some(OVERLAY_DRIFT_COPY_DETAIL.to_string()),
            });
        }
    }

    Ok(CopyAudit {
        modifications,
        records,
        notes: vec![OVERLAY_DRIFT_AUDIT_NOTE.to_string()],
    })
}

/// Advice that names the strategies which keep or drop unmanaged copy edits.
const UNMANAGED_COPY_EDIT_NOTE: &str = "unmanaged projected-copy edits were detected; keep them \
                                        with 'skillctl update --apply --strategy overlay', or \
                                        back them up and drop them with '--strategy discard'";

/// Recommended action, available actions, and advice for a check that observes
/// no upstream revision.
///
/// A local source and a commit-pinned git import share this shape: no upstream
/// revision can move, so the check recommends the repair of unmanaged edits
/// only, and nothing when the workspace is clean.
fn recommendation_without_upstream(
    modifications: &[PlannedModification],
) -> (UpdateAction, Vec<UpdateAction>, Vec<String>) {
    if modifications
        .iter()
        .any(|modification| !modification.managed)
    {
        (
            UpdateAction::CreateOverlay,
            vec![
                UpdateAction::CreateOverlay,
                UpdateAction::Detach,
                UpdateAction::PublishVariant,
                UpdateAction::Skip,
            ],
            vec![UNMANAGED_COPY_EDIT_NOTE.to_string()],
        )
    } else {
        (UpdateAction::Skip, vec![UpdateAction::Skip], Vec::new())
    }
}

/// Unmanaged edits found inside the generated copies of one managed skill.
fn detect_projected_copy_modifications(
    context: &AppContext,
    manifest: &WorkspaceManifest,
    install: &InstallRecord,
    projections: &[ProjectionRecord],
    candidate: Option<&ResolvedSkillCandidate>,
) -> Result<Vec<PlannedModification>, AppError> {
    let mut checked_roots = BTreeSet::new();
    let mut modifications = Vec::new();

    // A disabled import resolves to no candidate, so the workspace holds no
    // effective content to compare a copy against. Its leftover copies keep
    // the edits a sync preserved them for, and the same repair strategies
    // resolve those edits, so the check reports them instead of calling the
    // import clean.
    let desired = match candidate {
        Some(candidate) => {
            for projection in projections {
                if projection.effective_version_hash != install.effective_version_hash {
                    continue;
                }

                let projection_root =
                    resolve_runtime_root_path(context, &projection.physical_root)?
                        .join(&projection.projected_path);

                // An absent projection is not drift. The caller classifies a
                // missing destination separately, so a copy that is not there
                // is reported as no difference at all.
                if !checked_roots.insert(projection_root.clone()) {
                    continue;
                }

                if let Some(path) =
                    first_projection_difference(context, &projection_root, candidate)?
                {
                    modifications.push(PlannedModification {
                        kind: LocalModificationKind::ProjectedCopy,
                        managed: false,
                        path: Some(path.clone()),
                        details: Some(
                            "projected runtime copy differs from the recorded effective skill"
                                .to_string(),
                        ),
                    });
                }
            }
            // The digest must be computed the way a materialization of the
            // current mode records it, because a symlink projection hashes its
            // link targets instead of its content.
            Some(crate::apply::desired_projection_digest(
                candidate,
                manifest.projection.mode == ProjectionMode::Symlink,
            )?)
        }
        None => None,
    };

    // A preserved copy no longer has a projection record, and a disabled import
    // never had one for the copy a sync kept, so the check also inspects the
    // roots directly. This keeps the recommendation the apply engine agrees
    // with.
    if modifications.is_empty() {
        modifications.extend(detect_unrecorded_copies_in_documented_roots(
            context,
            manifest,
            install,
            desired.as_deref(),
        )?);
    }

    Ok(modifications)
}

/// Drifted generated copies a direct scan of the documented roots finds.
///
/// The scan covers the same universe an edit repair resolves, so a copy the
/// repair engine cannot see is never reported as repairable. A planned root is
/// also compared against the content the caller expects, which names a copy
/// written before digests were recorded. A documented root the plan dropped is
/// judged from the digest the copy records alone, as the sync prune pass judges
/// it, because no plan decides what that root should hold any more; the repair
/// engine, which keeps the revision the copy was written from, may still
/// resolve such a copy against the recorded baseline of the pinned source.
fn detect_unrecorded_copies_in_documented_roots(
    context: &AppContext,
    manifest: &WorkspaceManifest,
    install: &InstallRecord,
    desired: Option<&str>,
) -> Result<Vec<PlannedModification>, AppError> {
    let scope = target_scope_for(install.skill.scope);
    let planned: BTreeSet<PathBuf> =
        crate::materialize::planned_physical_root_paths(context, manifest, scope)?
            .into_iter()
            .collect();

    let mut modifications = Vec::new();
    for root in crate::materialize::edit_repair_root_paths(context, manifest, scope)? {
        let expected = if planned.contains(&root) {
            desired
        } else {
            None
        };
        let dir = root.join(&install.skill.skill_id);
        // The bundled skill owns its own copies in every documented user root,
        // so the repair engine must not be told to resolve them.
        if crate::materialize::is_bundled_projection_directory(&dir)? {
            continue;
        }
        if let crate::apply::ProjectionState::Drift(drift) =
            crate::apply::inspect_projection(&dir, expected)?
        {
            modifications.push(PlannedModification {
                kind: LocalModificationKind::ProjectedCopy,
                managed: false,
                path: Some(display_path(context, &drift.path)),
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
        // An absent projection is not drift. The caller classifies a missing
        // destination separately, so a copy that is not there is reported as
        // no difference at all.
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(None),
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
    let commit_pinned = plans
        .iter()
        .filter(|plan| plan.outcome == UpdateCheckOutcome::CommitPinned)
        .count();
    let failed = plans
        .iter()
        .filter(|plan| plan.outcome == UpdateCheckOutcome::Failed)
        .count();

    format!(
        "Checked {} managed skill{}. {} update available, {} blocked, {} up to date, {} detached, \
         {} local source, {} commit pinned, {} failed.",
        plans.len(),
        plural_suffix(plans.len()),
        update_available,
        blocked,
        up_to_date,
        detached,
        local,
        commit_pinned,
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
    fn ls_remote_output_yields_one_revision() {
        let stdout = concat!(
            "0123456789abcdef0123456789abcdef01234567\trefs/heads/main\n",
            "fedcba9876543210fedcba9876543210fedcba98\trefs/tags/v1\n",
            "fedcba9876543210fedcba9876543210fedcba98\trefs/tags/v1^{}\n",
        );

        assert_eq!(
            parse_ls_remote_revision(stdout).as_deref(),
            Some("fedcba9876543210fedcba9876543210fedcba98"),
            "a peeled tag wins over the ref it peels",
        );
        assert_eq!(
            parse_ls_remote_revision("0123456789abcdef0123456789abcdef01234567\trefs/heads/main\n")
                .as_deref(),
            Some("0123456789abcdef0123456789abcdef01234567"),
        );
        assert_eq!(
            parse_ls_remote_revision(""),
            None,
            "a source that answers with no ref yields no observation",
        );
    }

    #[test]
    fn immutable_references_are_recognized_by_shape() {
        let revision = "0123456789abcdef0123456789abcdef01234567";

        for reference in [
            revision,
            &revision[..7],
            "sha256:a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2",
        ] {
            assert!(
                crate::source::reference_pins_immutable_revision(reference, revision),
                "'{reference}' pins one immutable revision",
            );
        }

        for reference in [
            "main",
            "HEAD",
            "refs/tags/v1",
            "0123456789abcdef0123456789abcdef01234568",
        ] {
            assert!(
                !crate::source::reference_pins_immutable_revision(reference, revision),
                "'{reference}' names a movable or unrelated reference",
            );
        }
    }

    #[test]
    fn only_a_movable_reference_records_an_upstream_observation() {
        let revision = "0123456789abcdef0123456789abcdef01234567";

        assert_eq!(
            crate::source::observed_upstream_revision(Some("main"), revision).as_deref(),
            Some(revision),
            "following a branch observes the revision it resolved to",
        );
        assert_eq!(
            crate::source::observed_upstream_revision(None, revision).as_deref(),
            Some(revision),
            "an apply that moves to an observed revision records it",
        );
        assert_eq!(
            crate::source::observed_upstream_revision(Some(revision), revision),
            None,
            "moving to a named commit observes nothing upstream",
        );
        assert_eq!(
            crate::source::observed_upstream_revision(Some(&revision[..7]), revision),
            None,
            "an abbreviated commit observes nothing upstream either",
        );
    }

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
    fn user_scope_planner_uses_documented_roots_for_claude_and_github() {
        let registry = AdapterRegistry::new();

        let plan = plan_target_roots(
            &registry,
            TargetScope::User,
            ProjectionPolicy::PreferNative,
            &[TargetRuntime::ClaudeCode, TargetRuntime::GithubCopilot],
            &BTreeMap::new(),
        )
        .expect("plan succeeds");

        // Copilot's personal scope no longer documents ~/.claude/skills, so the
        // pair plans one documented root per runtime instead of a shared root.
        assert_eq!(
            plan.physical_roots,
            vec![
                PhysicalRootPlan {
                    path: "~/.claude/skills".into(),
                    targets: vec![TargetRuntime::ClaudeCode],
                },
                PhysicalRootPlan {
                    path: "~/.copilot/skills".into(),
                    targets: vec![TargetRuntime::GithubCopilot],
                },
            ]
        );
    }

    #[test]
    fn user_scope_planner_shares_neutral_root_between_codex_and_github() {
        let registry = AdapterRegistry::new();

        let plan = plan_target_roots(
            &registry,
            TargetScope::User,
            ProjectionPolicy::PreferNeutral,
            &[TargetRuntime::Codex, TargetRuntime::GithubCopilot],
            &BTreeMap::new(),
        )
        .expect("plan succeeds");

        assert_eq!(
            plan.physical_roots,
            vec![PhysicalRootPlan {
                path: "~/.agents/skills".into(),
                targets: vec![TargetRuntime::Codex, TargetRuntime::GithubCopilot],
            }]
        );
    }

    #[test]
    fn workspace_planner_shares_claude_root_between_claude_code_and_copilot_or_amp() {
        let registry = AdapterRegistry::new();

        // Rows of the spec.md section 15.3 workspace table that pair claude-code
        // with a runtime documenting `.claude/skills` as a compatibility root.
        for pair in [
            (
                TargetRuntime::ClaudeCode,
                TargetRuntime::GithubCopilot,
                ".claude/skills",
            ),
            (
                TargetRuntime::ClaudeCode,
                TargetRuntime::Amp,
                ".claude/skills",
            ),
        ] {
            let plan = plan_target_roots(
                &registry,
                TargetScope::Workspace,
                ProjectionPolicy::PreferNeutral,
                &[pair.0, pair.1],
                &BTreeMap::new(),
            )
            .expect("plan succeeds");

            assert_eq!(
                plan.physical_roots,
                vec![PhysicalRootPlan {
                    path: pair.2.into(),
                    targets: vec![pair.0, pair.1],
                }],
                "claude-code + {} must share one workspace root",
                pair.1.as_str(),
            );
        }
    }

    #[test]
    fn workspace_planner_shares_agents_root_between_codex_and_copilot() {
        let registry = AdapterRegistry::new();

        let plan = plan_target_roots(
            &registry,
            TargetScope::Workspace,
            ProjectionPolicy::PreferNeutral,
            &[TargetRuntime::Codex, TargetRuntime::GithubCopilot],
            &BTreeMap::new(),
        )
        .expect("plan succeeds");

        // Copilot documents `.agents/skills` as a workspace neutral root since the
        // 2026-11-05 verification, so the pair plans one shared root.
        assert_eq!(
            plan.physical_roots,
            vec![PhysicalRootPlan {
                path: ".agents/skills".into(),
                targets: vec![TargetRuntime::Codex, TargetRuntime::GithubCopilot],
            }]
        );
    }

    #[test]
    fn user_scope_planner_shares_agents_root_between_codex_and_amp() {
        let registry = AdapterRegistry::new();

        let plan = plan_target_roots(
            &registry,
            TargetScope::User,
            ProjectionPolicy::PreferNeutral,
            &[TargetRuntime::Codex, TargetRuntime::Amp],
            &BTreeMap::new(),
        )
        .expect("plan succeeds");

        // Amp joins the shared `~/.agents/skills` neutral root in multi-target
        // user plans instead of creating a separate `~/.config/agents/skills`
        // root.
        assert_eq!(
            plan.physical_roots,
            vec![PhysicalRootPlan {
                path: "~/.agents/skills".into(),
                targets: vec![TargetRuntime::Codex, TargetRuntime::Amp],
            }]
        );
    }

    #[test]
    fn user_scope_planner_keeps_amp_alone_on_its_neutral_config_root() {
        let registry = AdapterRegistry::new();

        for policy in [
            ProjectionPolicy::PreferNeutral,
            ProjectionPolicy::PreferNative,
        ] {
            let plan = plan_target_roots(
                &registry,
                TargetScope::User,
                policy,
                &[TargetRuntime::Amp],
                &BTreeMap::new(),
            )
            .expect("plan succeeds");

            assert_eq!(
                assignment_roots(&plan),
                vec![(
                    TargetRuntime::Amp,
                    "~/.config/agents/skills",
                    RootSelectionSource::Planner,
                )],
                "amp alone under {:?} must keep its documented neutral config root",
                policy,
            );
        }
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

    /// One check plan carrying only the fields target selection reads.
    fn check_plan(
        outcome: UpdateCheckOutcome,
        local_modification_detected: bool,
        trust: Option<crate::trust::SkillTrust>,
    ) -> SkillUpdatePlan {
        SkillUpdatePlan {
            skill: "delta".to_string(),
            scope: ManagedScope::Workspace,
            checked_at: "2026-11-06T00:00:00Z".to_string(),
            source: UpdateSourceSummary {
                kind: SourceKind::Git,
                url: "file:///source".to_string(),
                subpath: ".agents/skills/delta".to_string(),
            },
            pinned_revision: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
            latest_revision: Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string()),
            outcome,
            overlay_detected: false,
            overlay_changed_since_recorded_state: false,
            local_modification_detected,
            recommended_action: UpdateAction::Apply,
            available_actions: vec![UpdateAction::Apply, UpdateAction::Skip],
            modifications: Vec::new(),
            missing_projections: Vec::new(),
            trust,
            notes: Vec::new(),
            store_diverged: false,
        }
    }

    fn plan_request() -> UpdateRequest {
        UpdateRequest {
            plan: Some(PathBuf::from("update-plan.json")),
            ..UpdateRequest::default()
        }
    }

    fn apply_request() -> UpdateRequest {
        UpdateRequest {
            apply: true,
            ..UpdateRequest::default()
        }
    }

    fn blocked_trust() -> crate::trust::SkillTrust {
        crate::trust::SkillTrust::imported_unreviewed("delta", true).block_apply_update("delta")
    }

    #[test]
    fn a_blocked_check_contributes_to_a_plan_document_under_every_strategy() {
        let ungated = check_plan(UpdateCheckOutcome::Blocked, true, None);
        let gated = check_plan(UpdateCheckOutcome::Blocked, true, Some(blocked_trust()));

        for strategy in [
            crate::apply::ApplyStrategy::Clean,
            crate::apply::ApplyStrategy::Overlay,
            crate::apply::ApplyStrategy::Discard,
            crate::apply::ApplyStrategy::Fork,
            crate::apply::ApplyStrategy::Skip,
        ] {
            assert!(
                apply_requested(&plan_request(), &ungated, strategy),
                "an ungated blocked check must contribute under {}",
                strategy.as_str(),
            );
            assert!(
                apply_requested(&plan_request(), &gated, strategy),
                "a gated blocked check must contribute under {}",
                strategy.as_str(),
            );
        }
    }

    #[test]
    fn a_blocked_check_contributes_to_an_apply_only_when_something_resolves_it() {
        let ungated = check_plan(UpdateCheckOutcome::Blocked, true, None);
        let gated = check_plan(UpdateCheckOutcome::Blocked, true, Some(blocked_trust()));

        let clean = crate::apply::ApplyStrategy::Clean;
        assert!(
            !apply_requested(&apply_request(), &ungated, clean),
            "a clean apply cannot resolve unmanaged edits itself",
        );
        assert!(
            apply_requested(&apply_request(), &gated, clean),
            "a gated skill must reach the engine so the gate refuses it",
        );
        for strategy in [
            crate::apply::ApplyStrategy::Overlay,
            crate::apply::ApplyStrategy::Discard,
            crate::apply::ApplyStrategy::Fork,
        ] {
            assert!(apply_requested(&apply_request(), &ungated, strategy));
            assert!(apply_requested(&apply_request(), &gated, strategy));
        }
        assert!(!apply_requested(
            &apply_request(),
            &ungated,
            crate::apply::ApplyStrategy::Skip,
        ));
    }

    #[test]
    fn a_diverged_store_contributes_no_target_under_any_strategy() {
        let mut diverged = check_plan(UpdateCheckOutcome::Blocked, false, None);
        diverged.store_diverged = true;

        for strategy in [
            crate::apply::ApplyStrategy::Clean,
            crate::apply::ApplyStrategy::Overlay,
            crate::apply::ApplyStrategy::Discard,
            crate::apply::ApplyStrategy::Fork,
            crate::apply::ApplyStrategy::Skip,
        ] {
            assert!(
                !apply_requested(&apply_request(), &diverged, strategy),
                "every strategy resolves through the diverged store, so none may apply it under {}",
                strategy.as_str(),
            );
            assert!(
                !apply_requested(&plan_request(), &diverged, strategy),
                "a plan document must not carry the diverged import under {}",
                strategy.as_str(),
            );
        }
        assert!(
            !blocks_clean_apply(&diverged),
            "a diverged store is not an edit, so it never refuses a clean apply",
        );
        assert!(
            skip_reason(&diverged, crate::apply::ApplyStrategy::Clean, false)
                .contains("skillctl restore --frozen"),
            "the skip reason names a repair that rebuilds the store",
        );
    }

    #[test]
    fn a_detached_check_never_refuses_a_clean_apply() {
        // A detached check always reports local modifications, because the
        // detached-fork record is unmanaged. That report describes ownership,
        // not an edit, so it must not refuse a clean apply for any skill.
        let forked = check_plan(UpdateCheckOutcome::Detached, true, None);
        assert!(
            !blocks_clean_apply(&forked),
            "a detached skill contributes no apply target to refuse",
        );

        let gated = check_plan(UpdateCheckOutcome::Blocked, true, Some(blocked_trust()));
        assert!(
            !blocks_clean_apply(&gated),
            "a gated skill is left to the engine's trust gate",
        );

        for outcome in [
            UpdateCheckOutcome::Blocked,
            UpdateCheckOutcome::LocalSource,
            UpdateCheckOutcome::CommitPinned,
            UpdateCheckOutcome::UpToDate,
        ] {
            let edited = check_plan(outcome, true, None);
            assert!(
                blocks_clean_apply(&edited),
                "unmanaged edits under {} refuse a clean apply",
                outcome_label(outcome),
            );
            let untouched = check_plan(outcome, false, None);
            assert!(
                !blocks_clean_apply(&untouched),
                "a clean check under {} refuses nothing",
                outcome_label(outcome),
            );
        }
    }

    #[test]
    fn an_up_to_date_check_contributes_only_to_an_edit_repair() {
        let mut edited = check_plan(UpdateCheckOutcome::UpToDate, true, None);
        // An up-to-date check reports the pinned revision as the latest one.
        edited.latest_revision = Some(edited.pinned_revision.clone());
        let clean = check_plan(UpdateCheckOutcome::UpToDate, false, None);

        for strategy in [
            crate::apply::ApplyStrategy::Overlay,
            crate::apply::ApplyStrategy::Discard,
            crate::apply::ApplyStrategy::Fork,
        ] {
            assert!(
                apply_requested(&apply_request(), &edited, strategy),
                "the edits are repaired by {} without moving the revision",
                strategy.as_str(),
            );
        }
        for strategy in [
            crate::apply::ApplyStrategy::Clean,
            crate::apply::ApplyStrategy::Skip,
        ] {
            assert!(
                !apply_requested(&apply_request(), &edited, strategy),
                "a plain {} apply has no revision to move",
                strategy.as_str(),
            );
        }
        for strategy in [
            crate::apply::ApplyStrategy::Clean,
            crate::apply::ApplyStrategy::Overlay,
            crate::apply::ApplyStrategy::Discard,
            crate::apply::ApplyStrategy::Fork,
            crate::apply::ApplyStrategy::Skip,
        ] {
            assert!(
                !apply_requested(&apply_request(), &clean, strategy),
                "an up-to-date check without edits contributes no target",
            );
        }

        // A document is wider than a direct apply, because the apply that runs
        // it later picks its own strategy, and that apply refuses loudly when
        // the strategy cannot resolve the edits.
        assert!(apply_requested(
            &plan_request(),
            &edited,
            crate::apply::ApplyStrategy::Clean
        ));
        assert!(!apply_requested(
            &plan_request(),
            &clean,
            crate::apply::ApplyStrategy::Clean
        ));
    }

    #[test]
    fn plan_warnings_name_strategies_the_gate_allows() {
        let ungated = check_plan(UpdateCheckOutcome::Blocked, true, None);
        let gated = check_plan(UpdateCheckOutcome::Blocked, true, Some(blocked_trust()));
        let clean = crate::apply::ApplyStrategy::Clean;

        let plain = unresolvable_edits_warning(&apply_target(ungated), clean);
        assert!(
            plain.contains("--strategy overlay") && plain.contains("--strategy discard"),
            "an ungated import is advised the repair strategies: {plain}",
        );

        let blocked = unresolvable_edits_warning(&apply_target(gated), clean);
        assert!(
            blocked.contains("--strategy fork") && !blocked.contains("--strategy overlay"),
            "a gated import must only be advised the fork: {blocked}",
        );
    }

    #[test]
    fn a_document_that_leaves_nothing_to_apply_is_reported() {
        let empty = crate::apply::PlanDocument::new(Path::new("/workspace"), "clean", Vec::new());
        assert!(
            empty_plan_document_warning(&empty, true, true)
                .expect("an empty document is reported")
                .contains("contains no plans"),
        );

        // A document whose plans name no skill also contributes nothing.
        let unnamed = crate::apply::MutationPlanBuilder::new(
            crate::apply::MutationOperation::Update,
            Path::new("/workspace"),
        )
        .seal();
        assert!(unnamed.skill.is_none());
        let anonymous =
            crate::apply::PlanDocument::new(Path::new("/workspace"), "clean", vec![unnamed]);
        assert!(
            empty_plan_document_warning(&anonymous, true, true)
                .expect("a document without a named skill is reported")
                .contains("names a managed skill"),
        );

        assert!(
            empty_plan_document_warning(&empty, false, true).is_none(),
            "a document that produced a target needs no warning",
        );
        assert!(
            empty_plan_document_warning(&empty, true, false).is_none(),
            "a document that skipped a skill is already visible",
        );
    }

    fn apply_target(plan: SkillUpdatePlan) -> crate::apply::UpdateApplyTarget {
        crate::apply::UpdateApplyTarget {
            skill: ManagedSkillRef::new(ManagedScope::Workspace, plan.skill.clone()),
            source_url: plan.source.url.clone(),
            target_revision: plan
                .latest_revision
                .clone()
                .unwrap_or_else(|| plan.pinned_revision.clone()),
            expected: crate::apply::ExpectedImport {
                import_id: plan.skill.clone(),
                scope: ManagedScope::Workspace,
                revision: plan.pinned_revision.clone(),
                content_hash: "sha256:content".to_string(),
                overlay_hash: "sha256:none".to_string(),
                effective_version_hash: "sha256:effective".to_string(),
                bundle_digest: None,
                install_record_present: true,
            },
            summary: plan,
        }
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
