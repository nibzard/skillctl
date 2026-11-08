//! Diagnostics and validation domain entry points.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    path::{Component, MAIN_SEPARATOR, Path, PathBuf},
};

use crate::{
    adapter::{AdapterRegistry, InstallModeRisk, TargetRuntime, TargetScope},
    app::AppContext,
    apply::{DriftReason, RepairCommands, scoped_sync_command},
    builtin,
    cli::Scope,
    error::{AppError, ExitStatus},
    lockfile::WorkspaceLockfile,
    manifest::{ManifestScope, ProjectionMode, ProjectionPolicy, WorkspaceManifest},
    materialize::PROJECTION_METADATA_FILE,
    planner::{self, ProjectionPlan},
    resolver::{
        self, EffectiveSkillGraph, InternalSkillId, ProjectionOutcome, ResolutionStage,
        ResolveWorkspaceRequest, ResolvedSkillCandidate, SkillScope, SkillSourceClass,
    },
    response::AppResponse,
    skill::{
        self, CLAUDE_FRONTMATTER_FIELDS, OPENAI_METADATA_FILE, SKILL_MANIFEST_FILE,
        SkillDefinition, SkillSafetySummary, SkillVendorMetadata,
    },
    source::{hash_directory_contents, imports_store_root, stored_import_root},
    state::{LocalStateStore, ManagedScope, ManagedSkillRef},
    trust::SkillTrust,
};
use serde::Serialize;
use sha2::{Digest, Sha256};

const UNUSED_IMPORTS_PLACEHOLDER_DIR: &str = ".agents/.skillctl-unused-imports";
/// Issue code for a generated copy left behind in a root runtimes still read.
const STALE_GENERATED_PROJECTION_CODE: &str = "stale-generated-projection";
/// Issue code for a state database SQLite cannot read at all.
const STATE_DATABASE_UNREADABLE_CODE: &str = "state-database-unreadable";
/// Issue code for a workspace that holds no manifest and runs on defaults.
const MANIFEST_MISSING_CODE: &str = "manifest-missing";
/// Issue code for a workspace whose manifest survives but whose lockfile is gone.
const LOCKFILE_MISSING_CODE: &str = "lockfile-missing";
/// Issue code for a stored import that no longer hashes to its lockfile pin.
const IMPORT_STORE_DIVERGENCE_CODE: &str = "import-store-divergence";

/// Structured diagnostics report for `validate` and `doctor`.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct DiagnosticReport {
    /// Deterministic totals for the report.
    pub summary: DiagnosticSummary,
    /// Ordered set of actionable issues.
    pub issues: Vec<DiagnosticIssue>,
    /// Adapter verification metadata, populated by `doctor` only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub adapters: Vec<AdapterVerification>,
}

/// Machine-readable record of when one adapter's facts were last verified.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct AdapterVerification {
    /// Runtime target the record describes.
    pub target: TargetRuntime,
    /// Date the adapter facts were last verified against vendor documentation.
    pub verified_on: String,
    /// Vendor documentation pages that back the recorded discovery roots.
    pub documentation_sources: Vec<String>,
}

/// Report summary.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct DiagnosticSummary {
    /// Number of skill roots checked during validation.
    pub checked_skill_count: usize,
    /// Number of error-severity issues.
    pub error_count: usize,
    /// Number of warning-severity issues.
    pub warning_count: usize,
}

/// One actionable validation or doctor issue.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DiagnosticIssue {
    /// Error or warning severity.
    pub severity: DiagnosticSeverity,
    /// Stable machine-readable issue code.
    pub code: String,
    /// Related skill name when applicable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skill: Option<String>,
    /// Related management scope when applicable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// Related target runtime when applicable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<TargetRuntime>,
    /// Related filesystem path when applicable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Trust decision associated with the issue, when relevant.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trust: Option<SkillTrust>,
    /// Declarative safety metadata associated with the issue, when relevant.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub safety: Option<SkillSafetySummary>,
    /// Plain-English explanation.
    pub message: String,
    /// Suggested next action.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix: Option<String>,
}

/// Severity for one diagnostic issue.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DiagnosticSeverity {
    /// Blocking validation failure.
    Error,
    /// Non-blocking doctor warning.
    Warning,
}

/// Typed request for `skillctl doctor`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DoctorRequest;

/// Typed request for `skillctl doctor --repair-state`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RepairStateRequest {
    /// Apply the previewed repairs instead of only showing them.
    pub apply: bool,
}

/// Typed request for `skillctl validate`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ValidateRequest;

/// Explain payload returned through `skillctl explain`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ExplainReport {
    /// Requested projected skill name.
    pub skill: String,
    /// Scope used for root planning and visibility checks.
    pub scope: String,
    /// Overall explain status.
    pub status: ExplainStatus,
    /// Active winner, when one exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub winner: Option<ExplainCandidate>,
    /// Shadowed candidates for the same projected skill name.
    pub shadowed: Vec<ExplainCandidate>,
    /// Visibility view per selected target.
    pub targets: Vec<ExplainTarget>,
    /// Drift and state summary relevant to the active copy.
    pub drift: ExplainDrift,
    /// Scope that owns the requested name when no candidate resolves in the
    /// requested scope, for example the bundled skill under an unscoped
    /// request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner_scope: Option<String>,
    /// Related issues already known for this skill.
    pub issues: Vec<DiagnosticIssue>,
}

/// Preflight payload returned through `skillctl inspect`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct InspectReport {
    /// Requested projected skill name.
    pub skill: String,
    /// Scope used for root planning and visibility checks.
    pub scope: String,
    /// Overall inspect status.
    pub status: ExplainStatus,
    /// Active winner, when one exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub candidate: Option<ExplainCandidate>,
    /// Trust summary for the active winner, when one exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trust: Option<SkillTrust>,
    /// Declarative safety metadata for the active winner, when one exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub safety: Option<SkillSafetySummary>,
    /// Required credentials currently missing from the environment.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing_credentials: Vec<String>,
    /// Required tools currently missing from PATH.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing_tools: Vec<String>,
    /// Visibility view per selected target.
    pub targets: Vec<ExplainTarget>,
    /// Scope that owns the requested name when no candidate resolves in the
    /// requested scope, for example the bundled skill under an unscoped
    /// request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner_scope: Option<String>,
    /// Related issues already known for this skill.
    pub issues: Vec<DiagnosticIssue>,
}

/// Explain status for one projected skill name.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExplainStatus {
    /// One winner resolved cleanly.
    Selected,
    /// Same-name conflict remains unresolved.
    Conflict,
    /// No candidate exists for the requested name.
    Missing,
}

/// Candidate view rendered by `skillctl explain`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ExplainCandidate {
    /// Selected scope for the candidate.
    pub scope: String,
    /// Stable internal candidate identifier.
    pub internal_id: String,
    /// Source-class precedence bucket.
    pub source_class: String,
    /// Filesystem root of the candidate.
    pub root: String,
    /// Why the candidate won or lost.
    pub why: String,
    /// Import identifier when the candidate is managed by the manifest.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub import_id: Option<String>,
    /// Overlay root when one exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overlay_root: Option<String>,
    /// Pinned resolved revision when one exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_revision: Option<String>,
    /// Effective version hash when one exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effective_version_hash: Option<String>,
}

/// Per-target visibility details for `skillctl explain`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ExplainTarget {
    /// Runtime target.
    pub target: TargetRuntime,
    /// Physical root selected for the target.
    pub root: String,
    /// Resolved path where the skill should appear.
    pub path: String,
    /// Whether the target currently materializes the active winner.
    pub visible: bool,
    /// Plain-English explanation for the visibility result.
    pub reason: String,
    /// Observed presence of the projection at the planned path.
    pub observed_presence: ProjectionObservation,
    /// Whether the observed projection content matches the active winner,
    /// when a winner exists and the projection is observable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed_matches_winner: Option<bool>,
    /// Content digest of the observed projection, excluding generated metadata.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed_digest: Option<String>,
    /// Content digest expected from the active winner, when one exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_digest: Option<String>,
    /// Why a plain `sync` preserves this copy instead of refreshing it, when
    /// the copy holds content `skillctl` did not record.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed_preserved_reason: Option<DriftReason>,
    /// Actionable repair step when the destination is not healthy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix: Option<String>,
}

/// Observed state of one planned projection destination.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProjectionObservation {
    /// A readable skill directory exists at the planned path.
    Present,
    /// Nothing exists at the planned path.
    Missing,
    /// The planned path exists but is not a directory.
    NotADirectory,
    /// The planned path could not be inspected.
    Unreadable,
}

impl ProjectionObservation {
    /// Classify what the filesystem holds at one planned projection path.
    ///
    /// Shared with the update planner so a check and a diagnosis never
    /// disagree about whether a projection destination exists.
    pub(crate) fn observe(path: &Path) -> Self {
        match fs::metadata(path) {
            Ok(metadata) if metadata.is_dir() => Self::Present,
            Ok(_) => Self::NotADirectory,
            Err(source) if source.kind() == io::ErrorKind::NotFound => Self::Missing,
            Err(_) => Self::Unreadable,
        }
    }
}

/// Drift summary for `skillctl explain`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ExplainDrift {
    /// Whether the currently materialized root content matches the active winner.
    pub active_projection_matches_winner: bool,
    /// Whether the active copy differs from the pinned managed source.
    pub active_differs_from_pinned_source: bool,
    /// Pinned user reference when one exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pinned_reference: Option<String>,
    /// Effective version hash from the current managed state when one exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effective_version_hash: Option<String>,
    /// Whether the managed install is detached.
    pub detached: bool,
    /// Whether the managed install was forked into local ownership.
    pub forked: bool,
}

struct ValidationArtifacts {
    checked_skill_count: usize,
    issues: Vec<DiagnosticIssue>,
}

struct WorkspaceAnalysis {
    manifest: WorkspaceManifest,
    /// Whether no manifest file exists and the documented defaults applied.
    manifest_defaulted: bool,
    lockfile: WorkspaceLockfile,
    /// Whether no lockfile file exists and the empty default applied.
    lockfile_defaulted: bool,
    validation: ValidationArtifacts,
    graph: Option<EffectiveSkillGraph>,
}

struct BundledDoctorArtifacts {
    checked_skill_count: usize,
    issues: Vec<DiagnosticIssue>,
}

/// Local state access for one `doctor` run.
///
/// `doctor` is the entry point a user reaches for when something is broken, so
/// it must answer even when the state database is unreadable. The store is then
/// absent, the checks that read local state are skipped, and the reason is
/// reported as its own issue. Every other open failure still fails the command,
/// because a lock timeout or a missing home directory has a different fix.
struct DoctorState {
    /// The store, when SQLite could read the state database.
    store: Option<LocalStateStore>,
    /// Issue reporting why local state is unavailable, when it is.
    unavailable: Option<DiagnosticIssue>,
}

impl DoctorState {
    /// Open the store, or record the one failure `doctor` degrades on.
    fn open(context: &AppContext) -> Result<Self, AppError> {
        match LocalStateStore::open_default_for(&context.working_directory) {
            Ok(store) => Ok(Self {
                store: Some(store),
                unavailable: None,
            }),
            Err(AppError::LocalStateDatabaseUnreadable { path, reason }) => Ok(Self {
                store: None,
                unavailable: Some(state_database_unreadable_issue(context, &path, &reason)),
            }),
            Err(error) => Err(error),
        }
    }

    /// The store the state-dependent checks read, when they can run.
    fn store(&self) -> Option<&LocalStateStore> {
        self.store.as_ref()
    }
}

/// Build the issue that reports a state database SQLite cannot read.
fn state_database_unreadable_issue(
    context: &AppContext,
    path: &Path,
    reason: &str,
) -> DiagnosticIssue {
    DiagnosticIssue {
        severity: DiagnosticSeverity::Error,
        code: STATE_DATABASE_UNREADABLE_CODE.to_string(),
        skill: None,
        scope: None,
        target: None,
        path: Some(planner::display_path(context, path)),
        trust: None,
        safety: None,
        message: format!(
            "the state database is not a readable SQLite database: {reason}, so this report \
             skips the checks that read local state"
        ),
        fix: Some(
            "run 'skillctl doctor --repair-state --apply' to move the corrupt file aside, or \
             move it aside yourself; the next command rebuilds local state from \
             .agents/skillctl.yaml and .agents/skillctl.lock"
                .to_string(),
        ),
    }
}

/// Build the issue that reports a workspace without a manifest file.
///
/// A missing manifest is a supported state, not an error: the documented
/// defaults keep the simplest local-only flow working (spec section 11.1). But
/// the user never chose those defaults, so `doctor` says they applied and names
/// the command that makes the workspace explicit.
fn missing_manifest_issue(context: &AppContext, manifest: &WorkspaceManifest) -> DiagnosticIssue {
    let path = planner::display_path(context, &manifest.path);
    DiagnosticIssue {
        severity: DiagnosticSeverity::Warning,
        code: MANIFEST_MISSING_CODE.to_string(),
        skill: None,
        scope: None,
        target: None,
        path: Some(path.clone()),
        trust: None,
        safety: None,
        message: format!(
            "the workspace manifest '{path}' does not exist, so this report ran on the \
             documented defaults instead of a manifest you chose"
        ),
        fix: Some(format!(
            "run 'skillctl init' to create {path} and choose targets explicitly"
        )),
    }
}

/// Whether the workspace shows signs of skillctl use besides its manifest.
///
/// A directory with no manifest, no canonical skills root, and no lockfile
/// never had a manifest to lose. The bundled user-scope flow runs in such
/// directories and stays healthy, so the missing-manifest issue waits until the
/// workspace holds skills or pinned imports that the defaults now govern.
fn workspace_holds_skillctl_state(context: &AppContext, manifest: &WorkspaceManifest) -> bool {
    let skills_root = context
        .working_directory
        .join(manifest.layout.skills_dir.as_str());
    let lockfile = context
        .working_directory
        .join(crate::lockfile::DEFAULT_LOCKFILE_PATH);
    skills_root.exists() || lockfile.exists()
}

/// Build the issue that reports a workspace that lost its lockfile but kept
/// its manifest.
///
/// The manifest is the declaration the user chose, and `skillctl init` writes
/// both files, so a workspace that holds a manifest held a lockfile too. An
/// empty set of pins still lets this report answer, so the loss is a warning
/// while `verify` and `update` refuse. The fix keeps the two repairs their
/// refusal names, because they are not interchangeable: `init` writes an empty
/// lockfile, which only fits a workspace that pinned nothing yet.
fn missing_lockfile_issue(context: &AppContext, lockfile: &WorkspaceLockfile) -> DiagnosticIssue {
    let path = planner::display_path(context, &lockfile.path);
    DiagnosticIssue {
        severity: DiagnosticSeverity::Warning,
        code: LOCKFILE_MISSING_CODE.to_string(),
        skill: None,
        scope: None,
        target: None,
        path: Some(path.clone()),
        trust: None,
        safety: None,
        message: format!(
            "the workspace lockfile '{path}' does not exist, so this report ran on an \
             empty set of pins instead of the imports a lockfile holds"
        ),
        fix: Some(format!(
            "in a team checkout, retrieve the committed {path}; run 'skillctl init' only \
             when this workspace pinned nothing yet"
        )),
    }
}

/// Handle `skillctl doctor`.
pub fn handle_doctor(
    context: &AppContext,
    _request: DoctorRequest,
) -> Result<AppResponse, AppError> {
    let analysis = analyze_workspace(context)?;
    let state = DoctorState::open(context)?;
    let mut issues = analysis.validation.issues.clone();
    issues.extend(doctor_issues(context, &analysis, state.store())?);
    issues.extend(state_recovery_issues(context)?);
    if analysis.manifest_defaulted && workspace_holds_skillctl_state(context, &analysis.manifest) {
        issues.push(missing_manifest_issue(context, &analysis.manifest));
    }
    // The missing-manifest warning already names `skillctl init`, which writes
    // both files, so the lockfile issue waits for a manifest that says the
    // workspace adopted declared state and therefore held a lockfile.
    if analysis.lockfile_defaulted && !analysis.manifest_defaulted {
        issues.push(missing_lockfile_issue(context, &analysis.lockfile));
    }
    if let Some(unavailable) = state.unavailable.as_ref() {
        issues.push(unavailable.clone());
    }
    let bundled = bundled_doctor_artifacts(context, state.store())?;
    issues.extend(bundled.issues);
    let mut report = report_from_issues(
        analysis.validation.checked_skill_count + bundled.checked_skill_count,
        issues,
    );
    report.adapters = adapter_verification_entries();
    render_diagnostic_report("doctor", report)
}

/// Handle `skillctl doctor --repair-state`.
///
/// The preview mode never changes state. The apply mode performs only the
/// repairs whose outcome is unambiguous and reports everything else.
pub fn handle_repair_state(
    context: &AppContext,
    request: RepairStateRequest,
) -> Result<AppResponse, AppError> {
    let report = crate::state::repair_state(&context.working_directory, request.apply)?;
    let command = "doctor-repair-state";

    let mut response = AppResponse::success(command).with_data(serde_json::to_value(&report)?);
    let summary = if !request.apply {
        if report.has_findings() {
            "Found recoverable local state. Re-run with --apply to repair the unambiguous \
             items."
                .to_string()
        } else {
            "No recoverable local-state operations found.".to_string()
        }
    } else if report.repairs.is_empty() {
        "Nothing needed repair.".to_string()
    } else {
        format!("Repaired {} local-state item(s).", report.repairs.len())
    };
    response = response.with_summary(summary);

    for unresolved in &report.unresolved {
        response = response.with_warning(unresolved.clone());
    }
    if request.apply {
        // Human output names what was done, because a repair that moves a file
        // must say where that file went.
        response = response.with_detail_lines(report.repairs.iter().cloned());
    } else {
        if let Some(reason) = &report.state_database_unreadable {
            response = response.with_warning(format!(
                "the state database is not a readable SQLite database: {reason}. Run 'skillctl \
                 doctor --repair-state --apply' to move the corrupt file aside, or move it aside \
                 yourself; the next command rebuilds local state from .agents/skillctl.yaml and \
                 .agents/skillctl.lock"
            ));
        }
        for operation in &report.pending_operations {
            response = response.with_warning(format!(
                "interrupted '{}' from process {} left a recoverable journal",
                operation.operation, operation.pid
            ));
        }
        for table in &report.legacy_tables {
            response = response.with_warning(format!(
                "legacy table {} still exists: {}",
                table.legacy, table.detail
            ));
        }
    }

    Ok(response)
}

/// Report local-state recovery findings as ordinary doctor issues.
fn state_recovery_issues(context: &AppContext) -> Result<Vec<DiagnosticIssue>, AppError> {
    let report = crate::state::inspect_state_recovery(&context.working_directory)?;
    let mut issues = Vec::new();

    for operation in &report.pending_operations {
        issues.push(DiagnosticIssue {
            severity: DiagnosticSeverity::Warning,
            code: "state-recovery-pending".to_string(),
            skill: None,
            scope: None,
            target: None,
            path: Some(operation.workspace_path.clone()),
            trust: None,
            safety: None,
            message: format!(
                "an interrupted '{}' from process {} left a recoverable operation journal",
                operation.operation, operation.pid
            ),
            fix: Some("run 'skillctl doctor --repair-state --apply'".to_string()),
        });
    }

    for table in report.legacy_tables {
        let ambiguous = table.action == "ambiguous";
        issues.push(DiagnosticIssue {
            severity: if ambiguous {
                DiagnosticSeverity::Error
            } else {
                DiagnosticSeverity::Warning
            },
            code: if ambiguous {
                "state-legacy-tables-ambiguous".to_string()
            } else {
                "state-legacy-tables".to_string()
            },
            skill: None,
            scope: None,
            target: None,
            path: None,
            trust: None,
            safety: None,
            message: format!(
                "legacy table {} still exists beside the current schema: {}",
                table.legacy, table.detail
            ),
            fix: Some("run 'skillctl doctor --repair-state' to preview the recovery".to_string()),
        });
    }

    Ok(issues)
}

/// Collect verification metadata for every registered adapter.
fn adapter_verification_entries() -> Vec<AdapterVerification> {
    AdapterRegistry::new()
        .all()
        .iter()
        .map(|adapter| AdapterVerification {
            target: adapter.target,
            verified_on: adapter.verified_on.to_string(),
            documentation_sources: adapter
                .documentation_sources
                .iter()
                .map(std::string::ToString::to_string)
                .collect(),
        })
        .collect()
}

/// Handle `skillctl validate`.
pub fn handle_validate(
    context: &AppContext,
    _request: ValidateRequest,
) -> Result<AppResponse, AppError> {
    let analysis = analyze_workspace(context)?;
    let report = report_from_issues(
        analysis.validation.checked_skill_count,
        analysis.validation.issues,
    );
    render_diagnostic_report("validate", report)
}

/// Build the response payload for `skillctl explain`.
pub fn build_explain_response(
    context: &AppContext,
    skill_name: &str,
) -> Result<AppResponse, AppError> {
    let report = if is_bundled_user_skill_request(context, skill_name) {
        build_bundled_explain_report(context)?
    } else {
        let analysis = analyze_workspace(context)?;
        build_explain_report(context, &analysis, skill_name)?
    };
    // The summary names the `sync` command the observed scope needs, so it
    // reads the scope the report was planned in rather than the label alone.
    let summary = explain_summary(&report, selected_managed_scope(context));
    let warnings = report
        .issues
        .iter()
        .filter(|issue| issue.severity == DiagnosticSeverity::Warning)
        .map(issue_line)
        .collect::<Vec<_>>();
    let errors = report
        .issues
        .iter()
        .filter(|issue| issue.severity == DiagnosticSeverity::Error)
        .map(error_line)
        .collect::<Vec<_>>();
    let data = serde_json::to_value(&report)?;

    if errors.is_empty() {
        let response = AppResponse::success("explain")
            .with_summary(summary)
            .with_data(data);
        Ok(with_warning_messages(response, warnings))
    } else {
        // The payload `ok`, the process exit code, and MCP `isError` must all
        // describe the same outcome: a conflict is a failure, not a success
        // body with a failure exit override.
        Ok(AppResponse {
            ok: false,
            command: "explain",
            warnings,
            errors,
            data,
            summary: Some(summary),
            detail: Vec::new(),
            status_override: Some(ExitStatus::ValidationFailure),
        })
    }
}

/// Build the response payload for `skillctl inspect`.
pub fn build_inspect_response(
    context: &AppContext,
    skill_name: &str,
) -> Result<AppResponse, AppError> {
    let report = if is_bundled_user_skill_request(context, skill_name) {
        build_bundled_inspect_report(context)?
    } else {
        let analysis = analyze_workspace(context)?;
        build_inspect_report(context, &analysis, skill_name)?
    };
    let summary = inspect_summary(&report);
    let warnings = report
        .issues
        .iter()
        .filter(|issue| issue.severity == DiagnosticSeverity::Warning)
        .map(issue_line)
        .collect::<Vec<_>>();
    let errors = report
        .issues
        .iter()
        .filter(|issue| issue.severity == DiagnosticSeverity::Error)
        .map(error_line)
        .collect::<Vec<_>>();
    let data = serde_json::to_value(&report)?;

    if errors.is_empty() {
        let response = AppResponse::success("inspect")
            .with_summary(summary)
            .with_data(data);
        Ok(with_warning_messages(response, warnings))
    } else {
        Ok(AppResponse {
            ok: false,
            command: "inspect",
            warnings,
            errors,
            data,
            summary: Some(summary),
            detail: Vec::new(),
            status_override: Some(ExitStatus::ValidationFailure),
        })
    }
}

/// Build the typed explain report used by the read-only TUI dashboard.
pub(crate) fn build_explain_report_for_tui(
    context: &AppContext,
    skill_name: &str,
) -> Result<ExplainReport, AppError> {
    if is_bundled_user_skill_request(context, skill_name) {
        return build_bundled_explain_report(context);
    }

    let analysis = analyze_workspace(context)?;
    build_explain_report(context, &analysis, skill_name)
}

fn is_bundled_user_skill_request(context: &AppContext, skill_name: &str) -> bool {
    selected_managed_scope(context) == ManagedScope::User
        && builtin::is_bundled_request(skill_name, context.selector.scope)
}

fn bundled_doctor_artifacts(
    context: &AppContext,
    store: Option<&LocalStateStore>,
) -> Result<BundledDoctorArtifacts, AppError> {
    // The bundled diagnostics and the bundled projection checks read local
    // state, so an unreadable state database leaves nothing to report here.
    // The unreadable database is reported as its own issue.
    let mut issues = match store {
        Some(store) => bundled_base_issues(context, store)?,
        None => Vec::new(),
    };
    if selected_managed_scope(context) != ManagedScope::User {
        return Ok(BundledDoctorArtifacts {
            checked_skill_count: 0,
            issues,
        });
    }

    // Without a readable store the bundled install state cannot be read, so
    // the bundled skill is not counted as checked.
    let Some(store) = store else {
        return Ok(BundledDoctorArtifacts {
            checked_skill_count: usize::from(!issues.is_empty()),
            issues,
        });
    };

    let managed_skill = ManagedSkillRef::new(ManagedScope::User, "skillctl");
    let snapshot = store.skill_snapshot(&managed_skill)?;

    let managed_state_present = snapshot.install.is_some() || !snapshot.projections.is_empty();
    if !managed_state_present {
        return Ok(BundledDoctorArtifacts {
            checked_skill_count: usize::from(!issues.is_empty()),
            issues,
        });
    }

    let plan = bundled_target_plan(context)?;
    let expected_roots: BTreeMap<_, _> = plan
        .assignments
        .iter()
        .map(|assignment| (assignment.target, assignment.root.clone()))
        .collect();

    for record in &snapshot.projections {
        if let Some(expected_root) = expected_roots.get(&record.target)
            && &record.physical_root != expected_root
        {
            issues.push(DiagnosticIssue {
                severity: DiagnosticSeverity::Warning,
                code: "wrong-precedence-root".to_string(),
                skill: Some("skillctl".to_string()),
                scope: Some(ManagedScope::User.as_str().to_string()),
                target: Some(record.target),
                path: Some(record.physical_root.clone()),
                trust: None,
                safety: None,
                message: format!(
                    "target '{}' is projected into '{}' but the bundled plan expects '{}'",
                    record.target.as_str(),
                    record.physical_root,
                    expected_root
                ),
                fix: Some(
                    "run skillctl --scope user enable skillctl to rebuild the bundled projections"
                        .to_string(),
                ),
            });
        }
    }

    for assignment in &plan.assignments {
        let root = planner::resolve_runtime_root_path(context, &assignment.root)?;
        let projection_root = root.join("skillctl");
        let presence = ProjectionObservation::observe(&projection_root);
        match presence {
            ProjectionObservation::Present => {
                if let Some(path) = builtin::projection_difference(context, &projection_root)? {
                    issues.push(DiagnosticIssue {
                        severity: DiagnosticSeverity::Warning,
                        code: "projection-drift".to_string(),
                        skill: Some("skillctl".to_string()),
                        scope: Some(ManagedScope::User.as_str().to_string()),
                        target: Some(assignment.target),
                        path: Some(path),
                        trust: None,
                        safety: None,
                        message: format!(
                            "target '{}' currently materializes a bundled copy that does not match the active built-in asset",
                            assignment.target.as_str()
                        ),
                        fix: Some(
                            "run skillctl --scope user enable skillctl to rebuild the bundled projections"
                                .to_string(),
                        ),
                    });
                }
            }
            ProjectionObservation::Missing => {
                issues.push(DiagnosticIssue {
                    severity: DiagnosticSeverity::Warning,
                    code: "missing-projection".to_string(),
                    skill: Some("skillctl".to_string()),
                    scope: Some(ManagedScope::User.as_str().to_string()),
                    target: Some(assignment.target),
                    path: Some(planner::display_path(context, &projection_root)),
                    trust: None,
                    safety: None,
                    message: format!(
                        "target '{}' has no bundled projection at '{}'",
                        assignment.target.as_str(),
                        planner::display_path(context, &projection_root)
                    ),
                    fix: Some(
                        "run skillctl --scope user enable skillctl to rebuild the bundled projections"
                            .to_string(),
                    ),
                });
            }
            ProjectionObservation::NotADirectory | ProjectionObservation::Unreadable => {
                issues.push(DiagnosticIssue {
                    severity: DiagnosticSeverity::Warning,
                    code: "blocked-projection".to_string(),
                    skill: Some("skillctl".to_string()),
                    scope: Some(ManagedScope::User.as_str().to_string()),
                    target: Some(assignment.target),
                    path: Some(planner::display_path(context, &projection_root)),
                    trust: None,
                    safety: None,
                    message: format!(
                        "target '{}' cannot materialize a bundled projection at '{}' because the planned path is not a readable skill directory",
                        assignment.target.as_str(),
                        planner::display_path(context, &projection_root)
                    ),
                    fix: Some(
                        "inspect the planned path, remove the blocking entry or fix its permissions, then run skillctl --scope user enable skillctl"
                            .to_string(),
                    ),
                });
            }
        }
    }

    Ok(BundledDoctorArtifacts {
        checked_skill_count: 1,
        issues,
    })
}

fn build_bundled_explain_report(context: &AppContext) -> Result<ExplainReport, AppError> {
    let store = LocalStateStore::open_default_for(&context.working_directory)?;
    let managed_skill = ManagedSkillRef::new(ManagedScope::User, "skillctl");
    let snapshot = store.skill_snapshot(&managed_skill)?;
    let plan = bundled_target_plan(context)?;
    let issues = bundled_base_issues(context, &store)?;
    let managed_state_present = snapshot.install.is_some() || !snapshot.projections.is_empty();
    let status = if managed_state_present {
        ExplainStatus::Selected
    } else {
        ExplainStatus::Missing
    };

    let managed_effective_version = snapshot
        .install
        .as_ref()
        .map(|install| install.effective_version_hash.clone())
        .or_else(|| {
            snapshot
                .pin
                .as_ref()
                .and_then(|pin| pin.effective_version_hash.clone())
        });

    let winner = managed_state_present.then(|| ExplainCandidate {
        scope: ManagedScope::User.as_str().to_string(),
        internal_id: "builtin:user:skillctl".to_string(),
        source_class: "bundled".to_string(),
        root: snapshot.install.as_ref().map_or_else(
            || "builtin://skillctl".to_string(),
            |install| install.source_url.clone(),
        ),
        why: "skillctl manages the bundled user-scope asset directly".to_string(),
        import_id: None,
        overlay_root: None,
        resolved_revision: snapshot
            .install
            .as_ref()
            .map(|install| install.resolved_revision.clone())
            .or_else(|| {
                snapshot
                    .pin
                    .as_ref()
                    .map(|pin| pin.resolved_revision.clone())
            }),
        effective_version_hash: managed_effective_version.clone(),
    });

    let targets = bundled_explain_targets(context, &plan, status)?;
    let active_projection_matches_winner = if managed_state_present {
        plan.assignments
            .iter()
            .try_fold(true, |matches, assignment| {
                let root = planner::resolve_runtime_root_path(context, &assignment.root)?;
                let path = root.join("skillctl");
                if ProjectionObservation::observe(&path) != ProjectionObservation::Present {
                    return Ok::<_, AppError>(false);
                }
                let differs = builtin::projection_difference(context, &path)?;
                Ok::<_, AppError>(matches && differs.is_none())
            })?
    } else {
        false
    };

    let active_differs_from_pinned_source = match (status, managed_effective_version.as_ref()) {
        (ExplainStatus::Selected, Some(_)) | (ExplainStatus::Selected, None) => {
            !active_projection_matches_winner
        }
        (ExplainStatus::Missing, Some(_)) => true,
        (ExplainStatus::Missing, None) | (ExplainStatus::Conflict, _) => false,
    };

    Ok(ExplainReport {
        skill: "skillctl".to_string(),
        scope: ManagedScope::User.as_str().to_string(),
        status,
        winner,
        shadowed: Vec::new(),
        targets,
        drift: ExplainDrift {
            active_projection_matches_winner,
            active_differs_from_pinned_source,
            pinned_reference: snapshot
                .pin
                .as_ref()
                .map(|pin| pin.requested_reference.clone()),
            effective_version_hash: managed_effective_version,
            detached: snapshot
                .install
                .as_ref()
                .is_some_and(|install| install.detached),
            forked: snapshot
                .install
                .as_ref()
                .is_some_and(|install| install.forked),
        },
        // The bundled report already reads the scope that owns the name.
        owner_scope: None,
        issues,
    })
}

fn build_bundled_inspect_report(context: &AppContext) -> Result<InspectReport, AppError> {
    let explain = build_bundled_explain_report(context)?;
    let trust = explain.winner.as_ref().map(|_| SkillTrust::local(false));

    Ok(InspectReport {
        skill: explain.skill,
        scope: explain.scope,
        status: explain.status,
        candidate: explain.winner,
        trust,
        safety: None,
        missing_credentials: Vec::new(),
        missing_tools: Vec::new(),
        targets: explain.targets,
        owner_scope: None,
        issues: explain.issues,
    })
}

fn bundled_base_issues(
    context: &AppContext,
    store: &LocalStateStore,
) -> Result<Vec<DiagnosticIssue>, AppError> {
    builtin::diagnostics(context, store)?
        .into_iter()
        .map(|diagnostic| {
            Ok(DiagnosticIssue {
                severity: DiagnosticSeverity::Warning,
                code: diagnostic.code,
                skill: Some("skillctl".to_string()),
                scope: Some(ManagedScope::User.as_str().to_string()),
                target: None,
                path: diagnostic.path,
                trust: None,
                safety: None,
                message: diagnostic.message,
                fix: diagnostic.fix,
            })
        })
        .collect()
}

fn bundled_target_plan(context: &AppContext) -> Result<ProjectionPlan, AppError> {
    let targets = bundled_targets(context)?;
    planner::plan_target_roots(
        &AdapterRegistry::new(),
        TargetScope::User,
        ProjectionPolicy::PreferNeutral,
        &targets,
        &BTreeMap::new(),
    )
}

fn bundled_targets(context: &AppContext) -> Result<Vec<TargetRuntime>, AppError> {
    if context.selector.targets.is_empty() {
        return Ok(TargetRuntime::all().to_vec());
    }

    context
        .selector
        .targets
        .iter()
        .map(|target| parse_target_runtime(target))
        .collect()
}

fn bundled_explain_targets(
    context: &AppContext,
    plan: &ProjectionPlan,
    status: ExplainStatus,
) -> Result<Vec<ExplainTarget>, AppError> {
    plan.assignments
        .iter()
        .map(|assignment| {
            let root = planner::resolve_runtime_root_path(context, &assignment.root)?;
            let path = root.join("skillctl");
            let presence = ProjectionObservation::observe(&path);
            let matches_bundled = match (status, presence) {
                (ExplainStatus::Selected, ProjectionObservation::Present) => {
                    Some(builtin::projection_difference(context, &path)?.is_none())
                }
                _ => None,
            };
            let visible = status == ExplainStatus::Selected && matches_bundled == Some(true);
            let observed_digest = if presence == ProjectionObservation::Present {
                projection_content_digest(&path)?
            } else {
                None
            };

            let reason = match status {
                ExplainStatus::Selected => match (presence, matches_bundled) {
                    (ProjectionObservation::Present, Some(true)) => {
                        "the bundled skillctl asset is materialized and current at this \
                         user-scope root"
                            .to_string()
                    }
                    (ProjectionObservation::Present, _) => {
                        "a bundled projection exists at this root but does not match the \
                         active built-in asset"
                            .to_string()
                    }
                    (ProjectionObservation::Missing, _) => {
                        "no bundled projection currently exists at this root".to_string()
                    }
                    (ProjectionObservation::NotADirectory, _) => {
                        "the planned bundled projection path exists but is not a directory"
                            .to_string()
                    }
                    (ProjectionObservation::Unreadable, _) => {
                        "the planned bundled projection path could not be inspected".to_string()
                    }
                },
                ExplainStatus::Conflict => {
                    "a same-name conflict prevents a single active winner".to_string()
                }
                ExplainStatus::Missing => {
                    "the bundled skillctl asset is not currently installed in user scope"
                        .to_string()
                }
            };
            let fix = if visible {
                None
            } else {
                Some(
                    "run skillctl --scope user enable skillctl to rebuild the bundled \
                     projections"
                        .to_string(),
                )
            };

            Ok(ExplainTarget {
                target: assignment.target,
                root: planner::display_path(context, &root),
                path: planner::display_path(context, &path),
                visible,
                reason,
                observed_presence: presence,
                observed_matches_winner: matches_bundled,
                observed_digest,
                expected_digest: None,
                observed_preserved_reason: None,
                fix,
            })
        })
        .collect()
}

fn analyze_workspace(context: &AppContext) -> Result<WorkspaceAnalysis, AppError> {
    let (manifest, manifest_defaulted) =
        WorkspaceManifest::load_from_workspace_or_default(&context.working_directory)?;
    let (lockfile, lockfile_defaulted) =
        WorkspaceLockfile::load_from_workspace_or_default(&context.working_directory)?;
    let mut validation = collect_validation_issues(context, &manifest, &lockfile)?;

    let graph = if validation
        .issues
        .iter()
        .any(|issue| issue.severity == DiagnosticSeverity::Error)
    {
        None
    } else {
        let request = ResolveWorkspaceRequest::new(
            &context.working_directory,
            imports_directory_for(&context.working_directory, &manifest)?,
            manifest.clone(),
            lockfile.clone(),
        );
        match resolver::build_effective_skill_graph(&request) {
            Ok(graph) => Some(graph),
            Err(error) => {
                validation.issues.push(DiagnosticIssue {
                    severity: DiagnosticSeverity::Error,
                    code: "resolution-error".to_string(),
                    skill: None,
                    scope: None,
                    target: None,
                    path: None,
                    trust: None,
                    safety: None,
                    message: error.to_string(),
                    fix: Some(
                        "fix validation errors in the manifest, lockfile, overlays, or stored imports"
                            .to_string(),
                    ),
                });
                None
            }
        }
    };

    Ok(WorkspaceAnalysis {
        manifest,
        manifest_defaulted,
        lockfile,
        lockfile_defaulted,
        validation,
        graph,
    })
}

fn collect_validation_issues(
    context: &AppContext,
    manifest: &WorkspaceManifest,
    lockfile: &WorkspaceLockfile,
) -> Result<ValidationArtifacts, AppError> {
    let mut issues = Vec::new();
    let mut checked_skill_count = 0usize;

    let skills_root = context
        .working_directory
        .join(manifest.layout.skills_dir.as_str());
    match fs::metadata(&skills_root) {
        Ok(metadata) if metadata.is_dir() => {
            let mut entries = fs::read_dir(&skills_root)
                .map_err(|source| AppError::FilesystemOperation {
                    action: "read canonical skills root",
                    path: skills_root.clone(),
                    source,
                })?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|source| AppError::FilesystemOperation {
                    action: "read canonical skill entry",
                    path: skills_root.clone(),
                    source,
                })?;
            entries.sort_by_key(std::fs::DirEntry::path);

            for entry in entries {
                let root = entry.path();
                let metadata =
                    entry
                        .metadata()
                        .map_err(|source| AppError::FilesystemOperation {
                            action: "inspect canonical skill root",
                            path: root.clone(),
                            source,
                        })?;
                if !metadata.is_dir() {
                    issues.push(DiagnosticIssue {
                        severity: DiagnosticSeverity::Error,
                        code: "invalid-skill-root".to_string(),
                        skill: entry
                            .file_name()
                            .to_str()
                            .map(std::string::ToString::to_string),
                        scope: Some(ManagedScope::Workspace.as_str().to_string()),
                        target: None,
                        path: Some(planner::display_path(context, &root)),
                        trust: None,
                        safety: None,
                        message: format!(
                            "canonical skills root '{}' contains a non-directory entry",
                            planner::display_path(context, &root)
                        ),
                        fix: Some("keep only skill directories under .agents/skills".to_string()),
                    });
                    continue;
                }
                if resolver::is_generated_projection_root(&root)? {
                    continue;
                }

                checked_skill_count += 1;
                match SkillDefinition::load_from_dir(&root) {
                    Ok(skill) => issues.extend(skill_target_compatibility_issues(
                        context,
                        manifest,
                        &skill,
                        "target-compatibility",
                    )),
                    Err(error) => issues.push(skill_error_issue(
                        context,
                        "invalid-skill",
                        &root,
                        ManagedScope::Workspace,
                        error,
                    )),
                }
            }
        }
        Ok(_) => {
            issues.push(DiagnosticIssue {
                severity: DiagnosticSeverity::Error,
                code: "invalid-skill-root".to_string(),
                skill: None,
                scope: Some(ManagedScope::Workspace.as_str().to_string()),
                target: None,
                path: Some(planner::display_path(context, &skills_root)),
                trust: None,
                safety: None,
                message: format!(
                    "canonical skills root '{}' must be a directory",
                    planner::display_path(context, &skills_root)
                ),
                fix: Some("replace the path with a directory or run skillctl init".to_string()),
            });
        }
        Err(source) if source.kind() == io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(AppError::FilesystemOperation {
                action: "inspect canonical skills root",
                path: skills_root,
                source,
            });
        }
    }

    for import in manifest.imports.iter().filter(|import| import.enabled) {
        checked_skill_count += 1;
        issues.extend(validate_import(context, manifest, lockfile, import)?);
    }

    Ok(ValidationArtifacts {
        checked_skill_count,
        issues,
    })
}

fn validate_import(
    context: &AppContext,
    manifest: &WorkspaceManifest,
    lockfile: &WorkspaceLockfile,
    import: &crate::manifest::ImportDefinition,
) -> Result<Vec<DiagnosticIssue>, AppError> {
    let mut issues = Vec::new();
    let scope = managed_scope_from_manifest(import.scope);

    let Some(locked_import) = lockfile.imports.get(&import.id) else {
        issues.push(DiagnosticIssue {
            severity: DiagnosticSeverity::Error,
            code: "missing-lockfile-entry".to_string(),
            skill: Some(import.id.clone()),
            scope: Some(scope.as_str().to_string()),
            target: None,
            path: Some(lockfile.path.display().to_string()),
            trust: None,
            safety: None,
            message: format!(
                "enabled import '{}' is missing from the lockfile",
                import.id
            ),
            fix: Some("reinstall the skill or update .agents/skillctl.lock".to_string()),
        });
        return Ok(issues);
    };

    let stored_root = stored_import_root(
        managed_scope_from_manifest(import.scope),
        &context.working_directory,
        &import.id,
    )?;
    if let Some(issue) = ensure_directory_issue(
        context,
        "missing-import-store",
        &stored_root,
        scope,
        &import.id,
        "stored import root",
        "run 'skillctl restore --frozen' to reconstruct the pinned source from the lockfile",
    ) {
        issues.push(issue);
        return Ok(issues);
    }

    let skill_root = stored_root.join(locked_import.source.subpath.as_str());
    if let Some(issue) = ensure_directory_issue(
        context,
        "missing-import-source",
        &skill_root,
        scope,
        &import.id,
        "stored imported skill root",
        "sync the lockfile entry with the stored import path or reinstall the skill",
    ) {
        issues.push(issue);
        return Ok(issues);
    }

    // A store that no longer hashes to its pin is named here instead of being
    // left to the generic resolution error, so `explain` reports the state for
    // the one skill the reader asked about, the same way it names a missing
    // store. In symlink mode the divergence usually arrived through a
    // projected link, so the fix carries the same command list every other
    // report of this state names, including the fork that keeps the bytes.
    let observed_content_hash = hash_directory_contents(&skill_root)?;
    if observed_content_hash != locked_import.hashes.content {
        issues.push(DiagnosticIssue {
            severity: DiagnosticSeverity::Error,
            code: IMPORT_STORE_DIVERGENCE_CODE.to_string(),
            skill: Some(import.id.clone()),
            scope: Some(scope.as_str().to_string()),
            target: None,
            path: Some(planner::display_path(context, &skill_root)),
            trust: None,
            safety: None,
            message: format!(
                "stored import '{}' hashes to {observed_content_hash} but the lockfile pins {}; \
                 the import store changed after install",
                import.id, locked_import.hashes.content
            ),
            fix: Some(resolver::store_divergence_repair(&import.id)),
        });
        return Ok(issues);
    }

    let base_files = collect_file_map(&skill_root, "imported skill")?;
    let effective_files = apply_overlay_validation(
        context,
        manifest,
        import.id.as_str(),
        &base_files,
        &mut issues,
    )?;
    let manifest_path = effective_files
        .get(Path::new(SKILL_MANIFEST_FILE))
        .cloned()
        .ok_or_else(|| AppError::ResolutionValidation {
            message: format!(
                "import '{}' does not contain an effective '{}'",
                import.id, SKILL_MANIFEST_FILE
            ),
        })?;
    let manifest_source =
        fs::read_to_string(&manifest_path).map_err(|source| AppError::FilesystemOperation {
            action: "read effective skill manifest",
            path: manifest_path.clone(),
            source,
        })?;

    match SkillDefinition::from_source(
        &skill_root,
        manifest_path,
        &manifest_source,
        load_effective_vendor_metadata(&effective_files)?,
    ) {
        Ok(skill) => issues.extend(skill_target_compatibility_issues(
            context,
            manifest,
            &skill,
            "target-compatibility",
        )),
        Err(error) => issues.push(skill_error_issue(
            context,
            "invalid-skill",
            &skill_root,
            scope,
            error,
        )),
    }

    let _ = locked_import;
    Ok(issues)
}

fn doctor_issues(
    context: &AppContext,
    analysis: &WorkspaceAnalysis,
    store: Option<&LocalStateStore>,
) -> Result<Vec<DiagnosticIssue>, AppError> {
    let mut issues = Vec::new();
    let scope = selected_managed_scope(context);
    let targets = selected_targets(context, &analysis.manifest)?;

    issues.extend(stale_lockfile_issues(
        &analysis.manifest,
        &analysis.lockfile,
    ));
    // The disable is manifest state, so the check runs before the graph and
    // the state store: `doctor` must keep naming it when either fails, the
    // way `explain` names it for the one skill the reader asks about.
    issues.extend(disabled_import_issues(&analysis.manifest, scope));

    if analysis.manifest.projection.mode == ProjectionMode::Symlink {
        let registry = AdapterRegistry::new();
        for target in &targets {
            if registry.get(*target).install_mode_risk == InstallModeRisk::SymlinkUnstable {
                let acknowledged = analysis
                    .manifest
                    .projection
                    .allow_unsafe_targets
                    .contains(target);
                issues.push(DiagnosticIssue {
                    severity: DiagnosticSeverity::Warning,
                    code: "symlink-risk".to_string(),
                    skill: None,
                    scope: Some(scope.as_str().to_string()),
                    target: Some(*target),
                    path: None,
                    trust: None,
                    safety: None,
                    message: if acknowledged {
                        format!(
                            "target '{}' documents unstable symlink behavior; projection.allow_unsafe_targets explicitly enables symlink mode and copy mode is still safer",
                            target.as_str()
                        )
                    } else {
                        format!(
                            "target '{}' documents unstable symlink behavior; projection.allow_unsafe_targets must explicitly acknowledge the risk or copy mode should be used",
                            target.as_str()
                        )
                    },
                    fix: Some(if acknowledged {
                        "set projection.mode to copy to return to the default safe mode"
                            .to_string()
                    } else {
                        format!(
                            "add '{}' to projection.allow_unsafe_targets or set projection.mode to copy",
                            target.as_str()
                        )
                    }),
                });
            }
        }
    }

    let Some(graph) = &analysis.graph else {
        return Ok(issues);
    };

    issues.extend(graph_shadowing_issues(scope, graph));
    issues.extend(graph_conflict_issues(scope, graph));
    issues.extend(adapter_field_issues(context, &analysis.manifest, graph));

    // Every check below reads local state. When the state database is
    // unreadable, running them would either fail or report state skillctl
    // cannot see, so the partial report stops here and the unreadable database
    // is reported as its own issue.
    let Some(store) = store else {
        return Ok(issues);
    };
    let reviews = crate::trust::ReviewIndex::load(store)?;
    issues.extend(missing_required_credential_issues(scope, graph, &reviews));
    issues.extend(missing_required_tool_issues(scope, graph, &reviews));
    issues.extend(declared_capability_risk_issues(scope, graph, &reviews));
    issues.extend(script_risk_issues(scope, graph, &reviews));

    if !targets.is_empty() {
        let plan = planner::plan_target_roots(
            &AdapterRegistry::new(),
            target_scope_from_managed(scope),
            analysis.manifest.projection.policy,
            &targets,
            &analysis.manifest.adapters,
        )?;
        issues.extend(projection_record_issues(
            context, store, graph, scope, &plan,
        )?);
        issues.extend(stale_generated_projection_issues(
            context,
            store,
            graph,
            scope,
            &plan,
            &analysis.manifest,
            None,
        )?);
    }

    Ok(issues)
}

fn graph_shadowing_issues(
    scope: ManagedScope,
    graph: &EffectiveSkillGraph,
) -> Vec<DiagnosticIssue> {
    let mut issues = Vec::new();
    for projection in &graph.projections {
        let ProjectionOutcome::Selected {
            winner, shadowed, ..
        } = &projection.outcome
        else {
            continue;
        };
        if shadowed.is_empty() {
            continue;
        }
        if winner.scope != skill_scope_from_managed(scope)
            && !shadowed
                .iter()
                .any(|candidate| candidate.scope == skill_scope_from_managed(scope))
        {
            continue;
        }

        issues.push(DiagnosticIssue {
            severity: DiagnosticSeverity::Warning,
            code: "shadowed-skill".to_string(),
            skill: Some(projection.name.clone()),
            scope: Some(scope.as_str().to_string()),
            target: None,
            path: None,
            trust: None,
            safety: None,
            message: format!(
                "'{}' resolves to {} and shadows {}",
                projection.name,
                candidate_label(winner),
                shadowed
                    .iter()
                    .map(candidate_label)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            fix: Some(
                "run skillctl explain <skill> to inspect the winner and shadowed candidates"
                    .to_string(),
            ),
        });
    }

    issues
}

fn graph_conflict_issues(scope: ManagedScope, graph: &EffectiveSkillGraph) -> Vec<DiagnosticIssue> {
    let mut issues = Vec::new();
    for conflict in graph.conflicts() {
        if !conflict
            .contenders
            .iter()
            .any(|candidate| candidate.scope == skill_scope_from_managed(scope))
        {
            continue;
        }

        issues.push(DiagnosticIssue {
            severity: DiagnosticSeverity::Error,
            code: "projected-name-conflict".to_string(),
            skill: Some(conflict.name.clone()),
            scope: Some(scope.as_str().to_string()),
            target: None,
            path: None,
            trust: None,
            safety: None,
            message: format!(
                "same-name conflict remains for '{}' across {}",
                conflict.name,
                conflict
                    .contenders
                    .iter()
                    .map(candidate_label)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            fix: Some(
                "adjust manifest priorities or remove one of the conflicting skill sources"
                    .to_string(),
            ),
        });
    }

    issues
}

fn adapter_field_issues(
    context: &AppContext,
    manifest: &WorkspaceManifest,
    graph: &EffectiveSkillGraph,
) -> Vec<DiagnosticIssue> {
    graph
        .candidates
        .iter()
        .flat_map(|candidate| {
            skill_target_compatibility_issues(
                context,
                manifest,
                &candidate.skill,
                "unsupported-adapter-field",
            )
        })
        .collect()
}

fn script_risk_issues(
    scope: ManagedScope,
    graph: &EffectiveSkillGraph,
    reviews: &crate::trust::ReviewIndex,
) -> Vec<DiagnosticIssue> {
    let mut issues = Vec::new();
    for candidate in &graph.candidates {
        if candidate.scope != skill_scope_from_managed(scope) || candidate.import.is_none() {
            continue;
        }
        if !candidate
            .files
            .iter()
            .any(|file| file.relative_path.starts_with("scripts"))
        {
            continue;
        }

        let trust =
            crate::trust::trust_for_candidate(candidate, reviews.approved_for_candidate(candidate));
        // The elevated messaging holds until the current content is reviewed,
        // so an approval that covers it ends the warning instead of repeating
        // it next to the evidence that a review exists.
        if trust.effective_state == crate::trust::TrustState::ImportedReviewed {
            continue;
        }

        issues.push(DiagnosticIssue {
            severity: DiagnosticSeverity::Warning,
            code: "script-risk".to_string(),
            skill: Some(candidate.skill.name.as_str().to_string()),
            scope: Some(scope.as_str().to_string()),
            target: None,
            path: Some(candidate.skill.root.display().to_string()),
            trust: Some(trust),
            safety: issue_safety(&candidate.skill.safety),
            message: format!(
                "imported skill '{}' contains files under scripts/ and should be reviewed before use",
                candidate.skill.name.as_str()
            ),
            fix: Some(format!(
                "record a local review with 'skillctl review {} --approve', or fork it into \
                 local ownership",
                candidate.skill.name.as_str()
            )),
        });
    }

    issues
}

fn issue_safety(summary: &SkillSafetySummary) -> Option<SkillSafetySummary> {
    if summary.is_empty() {
        None
    } else {
        Some(summary.clone())
    }
}

fn missing_required_credential_issues(
    scope: ManagedScope,
    graph: &EffectiveSkillGraph,
    reviews: &crate::trust::ReviewIndex,
) -> Vec<DiagnosticIssue> {
    let mut issues = Vec::new();

    for winner in graph
        .projections
        .iter()
        .filter_map(|projection| projection.winner())
    {
        if winner.scope != skill_scope_from_managed(scope) {
            continue;
        }

        let missing_credentials = winner.skill.safety.missing_required_credentials();
        if missing_credentials.is_empty() {
            continue;
        }

        issues.push(DiagnosticIssue {
            severity: DiagnosticSeverity::Warning,
            code: "missing-required-credential".to_string(),
            skill: Some(winner.skill.name.as_str().to_string()),
            scope: Some(scope.as_str().to_string()),
            target: None,
            path: Some(winner.skill.root.display().to_string()),
            trust: Some(crate::trust::trust_for_candidate(
                winner,
                reviews.approved_for_candidate(winner),
            )),
            safety: Some(winner.skill.safety.clone()),
            message: format!(
                "skill '{}' requires credentials {} that are not set in the current environment",
                winner.skill.name.as_str(),
                missing_credentials.join(", ")
            ),
            fix: Some(
                "export the required credentials before use or mark them optional if they are not always needed"
                    .to_string(),
            ),
        });
    }

    issues
}

fn missing_required_tool_issues(
    scope: ManagedScope,
    graph: &EffectiveSkillGraph,
    reviews: &crate::trust::ReviewIndex,
) -> Vec<DiagnosticIssue> {
    let mut issues = Vec::new();

    for winner in graph
        .projections
        .iter()
        .filter_map(|projection| projection.winner())
    {
        if winner.scope != skill_scope_from_managed(scope) {
            continue;
        }

        let missing_tools = winner.skill.safety.missing_required_tools();
        if missing_tools.is_empty() {
            continue;
        }

        issues.push(DiagnosticIssue {
            severity: DiagnosticSeverity::Warning,
            code: "missing-required-tool".to_string(),
            skill: Some(winner.skill.name.as_str().to_string()),
            scope: Some(scope.as_str().to_string()),
            target: None,
            path: Some(winner.skill.root.display().to_string()),
            trust: Some(crate::trust::trust_for_candidate(
                winner,
                reviews.approved_for_candidate(winner),
            )),
            safety: Some(winner.skill.safety.clone()),
            message: format!(
                "skill '{}' requires tools {} that are not available on PATH",
                winner.skill.name.as_str(),
                missing_tools.join(", ")
            ),
            fix: Some(
                "install the required tools or mark them optional if they are not always needed"
                    .to_string(),
            ),
        });
    }

    issues
}

fn declared_capability_risk_issues(
    scope: ManagedScope,
    graph: &EffectiveSkillGraph,
    reviews: &crate::trust::ReviewIndex,
) -> Vec<DiagnosticIssue> {
    let mut issues = Vec::new();
    for candidate in &graph.candidates {
        if candidate.scope != skill_scope_from_managed(scope) || candidate.import.is_none() {
            continue;
        }
        if candidate.skill.safety.capabilities.is_empty() {
            continue;
        }

        issues.push(DiagnosticIssue {
            severity: DiagnosticSeverity::Warning,
            code: "declared-capability-risk".to_string(),
            skill: Some(candidate.skill.name.as_str().to_string()),
            scope: Some(scope.as_str().to_string()),
            target: None,
            path: Some(candidate.skill.root.display().to_string()),
            trust: Some(crate::trust::trust_for_candidate(
                candidate,
                reviews.approved_for_candidate(candidate),
            )),
            safety: Some(candidate.skill.safety.clone()),
            message: format!(
                "imported skill '{}' declares capabilities: {}; review whether that access is expected",
                candidate.skill.name.as_str(),
                candidate.skill.safety.capabilities.join(", ")
            ),
            fix: Some("review the declared capabilities or fork the skill into local ownership".to_string()),
        });
    }

    issues
}

fn projection_record_issues(
    context: &AppContext,
    store: &LocalStateStore,
    graph: &EffectiveSkillGraph,
    scope: ManagedScope,
    plan: &ProjectionPlan,
) -> Result<Vec<DiagnosticIssue>, AppError> {
    let mut issues = Vec::new();
    // A plain `skillctl sync` reads workspace scope, so every fix below must
    // name the qualified command a report that observes `scope` needs.
    let sync = scoped_sync_command(scope);
    let projection_records = store.projection_records(None)?;
    let plan_roots: BTreeMap<_, _> = plan
        .assignments
        .iter()
        .map(|assignment| (assignment.target, assignment.root.clone()))
        .collect();

    for record in projection_records
        .iter()
        .filter(|record| record.skill.scope == scope)
    {
        if let Some(expected_root) = plan_roots.get(&record.target)
            && &record.physical_root != expected_root
        {
            issues.push(DiagnosticIssue {
                severity: DiagnosticSeverity::Warning,
                code: "wrong-precedence-root".to_string(),
                skill: Some(record.skill.skill_id.clone()),
                scope: Some(scope.as_str().to_string()),
                target: Some(record.target),
                path: Some(record.physical_root.clone()),
                trust: None,
                safety: None,
                message: format!(
                    "target '{}' is projected into '{}' but the manifest now plans '{}'",
                    record.target.as_str(),
                    record.physical_root,
                    expected_root
                ),
                fix: Some(format!(
                    "run {sync} to regenerate projections in the planned roots"
                )),
            });
        }
    }

    for projection in &graph.projections {
        let ProjectionOutcome::Selected { winner, .. } = &projection.outcome else {
            continue;
        };
        if winner.scope != skill_scope_from_managed(scope) {
            continue;
        }

        for assignment in &plan.assignments {
            let root = planner::resolve_runtime_root_path(context, &assignment.root)?;
            let projection_root = root.join(&projection.name);
            let presence = ProjectionObservation::observe(&projection_root);
            match presence {
                ProjectionObservation::Present => {
                    if let Some(path) =
                        first_projection_difference(context, &projection_root, winner)?
                    {
                        let commands = drift_repair_commands_for(
                            context,
                            store,
                            scope,
                            &projection.name,
                            Some(winner.as_ref()),
                        )?;
                        issues.push(DiagnosticIssue {
                            severity: DiagnosticSeverity::Warning,
                            code: "projection-drift".to_string(),
                            skill: Some(projection.name.clone()),
                            scope: Some(scope.as_str().to_string()),
                            target: Some(assignment.target),
                            path: Some(path),
                            trust: None,
                            safety: None,
                            message: format!(
                                "target '{}' currently materializes a copy that does not match the active winner",
                                assignment.target.as_str()
                            ),
                            fix: Some(projection_drift_fix(
                                context,
                                &projection_root,
                                winner,
                                &projection.name,
                                commands,
                                scope,
                            )?),
                        });
                    }
                }
                ProjectionObservation::Missing => {
                    issues.push(DiagnosticIssue {
                        severity: DiagnosticSeverity::Warning,
                        code: "missing-projection".to_string(),
                        skill: Some(projection.name.clone()),
                        scope: Some(scope.as_str().to_string()),
                        target: Some(assignment.target),
                        path: Some(planner::display_path(context, &projection_root)),
                        trust: None,
                        safety: None,
                        message: format!(
                            "target '{}' has no projection at '{}'; the active winner is not materialized there",
                            assignment.target.as_str(),
                            planner::display_path(context, &projection_root)
                        ),
                        fix: Some(format!(
                            "run {sync} to materialize the missing projection"
                        )),
                    });
                }
                ProjectionObservation::NotADirectory | ProjectionObservation::Unreadable => {
                    issues.push(DiagnosticIssue {
                        severity: DiagnosticSeverity::Warning,
                        code: "blocked-projection".to_string(),
                        skill: Some(projection.name.clone()),
                        scope: Some(scope.as_str().to_string()),
                        target: Some(assignment.target),
                        path: Some(planner::display_path(context, &projection_root)),
                        trust: None,
                        safety: None,
                        message: format!(
                            "target '{}' cannot materialize a projection at '{}' because the planned path is not a readable skill directory",
                            assignment.target.as_str(),
                            planner::display_path(context, &projection_root)
                        ),
                        fix: Some(format!(
                            "inspect the planned path, remove the blocking entry or fix its \
                             permissions, then run {sync}"
                        )),
                    });
                }
            }
        }
    }

    Ok(issues)
}

/// Report generated copies that sit in a runtime root although no candidate
/// resolves to them any more.
///
/// A planned root is read by every runtime assigned to it, and a root the
/// plan stopped including is still read by the runtimes that document it, so
/// a leftover generated copy keeps loading a skill the workspace stopped
/// managing. Only directories skillctl itself generated are considered;
/// hand-authored runtime directories stay untouched and unreported.
fn stale_generated_projection_issues(
    context: &AppContext,
    store: &LocalStateStore,
    graph: &EffectiveSkillGraph,
    scope: ManagedScope,
    plan: &ProjectionPlan,
    manifest: &WorkspaceManifest,
    skill_filter: Option<&str>,
) -> Result<Vec<DiagnosticIssue>, AppError> {
    let mut issues = Vec::new();

    let mut scanned_roots: Vec<(PathBuf, Vec<TargetRuntime>, bool)> = plan
        .physical_roots
        .iter()
        .map(|root| {
            Ok((
                planner::resolve_runtime_root_path(context, &root.path)?,
                root.targets.clone(),
                true,
            ))
        })
        .collect::<Result<_, AppError>>()?;
    scanned_roots.extend(
        crate::materialize::unplanned_documented_roots(
            context,
            manifest,
            target_scope_from_managed(scope),
            plan,
        )?
        .into_iter()
        .map(|(_display, resolved, targets)| (resolved, targets, false)),
    );

    for (resolved_root, root_targets, planned) in scanned_roots {
        let entries = match fs::read_dir(&resolved_root) {
            Ok(entries) => entries,
            Err(source) if source.kind() == io::ErrorKind::NotFound => continue,
            // A documented path the plan does not use can hold a stray file;
            // there is no generated copy inside one to report.
            Err(source) if !planned && source.kind() == io::ErrorKind::NotADirectory => {
                continue;
            }
            Err(source) => {
                return Err(AppError::FilesystemOperation {
                    action: "read projection root",
                    path: resolved_root,
                    source,
                });
            }
        };

        for entry in entries {
            let entry = entry.map_err(|source| AppError::FilesystemOperation {
                action: "read projection root entry",
                path: resolved_root.clone(),
                source,
            })?;
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if skill_filter.is_some_and(|filter| filter != name) {
                continue;
            }
            if !entry
                .file_type()
                .map_err(|source| AppError::FilesystemOperation {
                    action: "inspect projection root entry",
                    path: path.clone(),
                    source,
                })?
                .is_dir()
                || !resolver::is_generated_projection_root(&path)?
            {
                continue;
            }

            // A winner in this scope justifies the copy, and so does the
            // bundled user-scope skill, which skillctl projects without an
            // entry in the effective-skill graph. A name that still has
            // contenders is reported as a conflict instead.
            let projection = graph.projection_for(&name);
            if projection
                .and_then(|projection| projection.winner())
                .is_some_and(|winner| winner.scope == skill_scope_from_managed(scope))
                || projection.is_some_and(|projection| {
                    matches!(projection.outcome, ProjectionOutcome::Conflict(_))
                })
            {
                continue;
            }
            let managed_skill = ManagedSkillRef::new(scope, name.clone());
            let bundled = store
                .install_record(&managed_skill)?
                .is_some_and(|record| builtin::is_bundled_install(&record))
                || crate::materialize::is_bundled_projection_directory(&path)?;
            if bundled {
                continue;
            }

            let targets = root_targets
                .iter()
                .map(|target| target.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            // A winner at another scope still resolves the name, so the copy
            // is not leftovers of a skill that stopped existing; the winner
            // simply never projected into this scope.
            let winner_scope = projection
                .and_then(|projection| projection.winner())
                .filter(|winner| winner.scope != skill_scope_from_managed(scope))
                .map(|winner| skill_scope_label(winner.scope).to_string());
            let message = if planned {
                match winner_scope {
                    Some(winner_scope) => format!(
                        "a skillctl-generated copy of '{}' remains at '{}', a root read by {}, \
                         but its active winner is managed at {winner_scope} scope",
                        name,
                        planner::display_path(context, &path),
                        targets,
                    ),
                    None => format!(
                        "a skillctl-generated copy of '{}' remains at '{}', a root read by {}, \
                         but no candidate resolves to '{}' any more",
                        name,
                        planner::display_path(context, &path),
                        targets,
                        name,
                    ),
                }
            } else if targets.is_empty() {
                format!(
                    "a skillctl-generated copy of '{}' remains at '{}', a configured adapter \
                     root no enabled target plans any more",
                    name,
                    planner::display_path(context, &path),
                )
            } else {
                format!(
                    "a skillctl-generated copy of '{}' remains at '{}', a root documented \
                     for {}, but no enabled target plans that root any more",
                    name,
                    planner::display_path(context, &path),
                    targets,
                )
            };
            // A copy that holds edits skillctl did not record survives a plain
            // sync, so only the explicit discard removes it. That is the whole
            // repair for a skill no install record holds any more, because
            // `update` refuses the name outright.
            let unrecorded_edits = matches!(
                crate::apply::inspect_projection(&path, None)?,
                crate::apply::ProjectionState::Drift(_)
            );
            // A plain `skillctl sync` reads workspace scope, so a report that
            // observes user scope must qualify the command it names.
            let sync = scoped_sync_command(scope);
            let fix = match (manifest.projection.prune, unrecorded_edits) {
                (true, true) => format!(
                    "run '{sync} --discard' to back the edits up and prune the stale \
                     generated copy; a plain '{sync}' preserves them"
                ),
                (true, false) => {
                    format!("run '{sync}' to prune the stale generated copy")
                }
                (false, true) => format!(
                    "set projection.prune to true and run '{sync} --discard' to back the \
                     edits up and prune the copy, or remove it by hand"
                ),
                (false, false) => format!(
                    "set projection.prune to true and run '{sync}' to prune the stale \
                     generated copy, or remove it by hand"
                ),
            };
            issues.push(DiagnosticIssue {
                severity: DiagnosticSeverity::Warning,
                code: STALE_GENERATED_PROJECTION_CODE.to_string(),
                skill: Some(name.clone()),
                scope: Some(scope.as_str().to_string()),
                target: None,
                path: Some(planner::display_path(context, &path)),
                trust: None,
                safety: None,
                message,
                fix: Some(fix),
            });
        }
    }

    issues.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| left.skill.cmp(&right.skill))
    });
    Ok(issues)
}

fn stale_lockfile_issues(
    manifest: &WorkspaceManifest,
    lockfile: &WorkspaceLockfile,
) -> Vec<DiagnosticIssue> {
    let manifest_ids: BTreeSet<_> = manifest
        .imports
        .iter()
        .map(|import| import.id.as_str())
        .collect();
    lockfile
        .imports
        .keys()
        .filter(|id| !manifest_ids.contains(id.as_str()))
        .map(|id| DiagnosticIssue {
            severity: DiagnosticSeverity::Warning,
            code: "stale-lockfile-entry".to_string(),
            skill: Some(id.clone()),
            scope: None,
            target: None,
            path: Some(lockfile.path.display().to_string()),
            trust: None,
            safety: None,
            message: format!(
                "lockfile entry '{}' no longer has a matching manifest import",
                id
            ),
            // Hand-editing `.agents/skillctl.lock` breaks `restore --frozen`,
            // so the fix names the command that clears the entry directly.
            fix: Some(format!(
                "run 'skillctl remove {id}' to clear the entry, or 'skillctl install <source> \
                 --name {id}' to declare the import again"
            )),
        })
        .collect()
}

/// Whether an explain or inspect report must scan the planned roots for a
/// leftover generated copy of one skill.
///
/// A missing candidate leaves copies behind, and so does a winner that lives
/// outside the requested management scope: the roots of that scope hold no
/// projection the plan justifies, exactly the state `doctor` reports.
fn explain_must_scan_for_stale_copies(
    status: ExplainStatus,
    winner: Option<&ResolvedSkillCandidate>,
    scope: ManagedScope,
) -> bool {
    status == ExplainStatus::Missing
        || winner.is_some_and(|candidate| candidate.scope != skill_scope_from_managed(scope))
}

/// Scope that owns a name the requested scope cannot resolve, when the bundled
/// skill is the only thing the name refers to.
///
/// The bundled `skillctl` skill is installed at user scope only, so the
/// workspace effective-skill graph holds no candidate for its name and an
/// unscoped request reads workspace scope. Plain absence would hide an install
/// every command bootstraps, so the report names the scope that owns the name
/// and the commands that read it. A scoped request, another name, a name the
/// graph resolves, or a name the report already carries a finding for keeps the
/// ordinary report, because then the requested scope owes the reader its own
/// answer. `name_is_unclaimed` states that neither the validation issues nor
/// the absence findings of this report name the skill.
fn bundled_owner_scope(
    context: &AppContext,
    skill_name: &str,
    name_is_unclaimed: bool,
) -> Option<String> {
    (context.selector.scope.is_none()
        && builtin::is_bundled_request(skill_name, context.selector.scope)
        && name_is_unclaimed)
        .then(|| ManagedScope::User.as_str().to_string())
}

fn build_explain_report(
    context: &AppContext,
    analysis: &WorkspaceAnalysis,
    skill_name: &str,
) -> Result<ExplainReport, AppError> {
    let scope = selected_managed_scope(context);
    let scope_label = scope.as_str().to_string();
    let related_issues = related_issues_for_skill(&analysis.validation.issues, skill_name);
    let Some(graph) = &analysis.graph else {
        // A graph that failed to build leaves the name unresolved, so the
        // bundled name still points at the scope that owns it.
        let owner_scope = bundled_owner_scope(context, skill_name, related_issues.is_empty());
        return Ok(ExplainReport {
            skill: skill_name.to_string(),
            scope: scope_label,
            status: ExplainStatus::Missing,
            winner: None,
            shadowed: Vec::new(),
            targets: Vec::new(),
            drift: ExplainDrift {
                active_projection_matches_winner: false,
                active_differs_from_pinned_source: false,
                pinned_reference: None,
                effective_version_hash: None,
                detached: false,
                forked: false,
            },
            owner_scope,
            issues: related_issues,
        });
    };

    let targets = selected_targets(context, &analysis.manifest)?;
    let target_plan = if targets.is_empty() {
        None
    } else {
        Some(planner::plan_target_roots(
            &AdapterRegistry::new(),
            target_scope_from_managed(scope),
            analysis.manifest.projection.policy,
            &targets,
            &analysis.manifest.adapters,
        )?)
    };

    let store = LocalStateStore::open_default_for(&context.working_directory)?;
    let managed_skill = ManagedSkillRef::new(scope, skill_name);
    let snapshot = store.skill_snapshot(&managed_skill)?;
    let managed_effective_version = snapshot
        .install
        .as_ref()
        .map(|install| install.effective_version_hash.clone());

    let projection = graph.projection_for(skill_name);
    let (status, winner, shadowed, explain_issues) = match projection {
        Some(projection) => match &projection.outcome {
            ProjectionOutcome::Selected {
                winner,
                shadowed,
                trace,
            } => {
                let winner_view = Some(explain_candidate(
                    context,
                    winner,
                    Some(explain_winner_reason(trace)),
                ));
                let shadowed_views = shadowed
                    .iter()
                    .map(|candidate| {
                        explain_candidate(
                            context,
                            candidate,
                            Some(format!("shadowed by {}", candidate_label(winner))),
                        )
                    })
                    .collect::<Vec<_>>();
                (
                    ExplainStatus::Selected,
                    winner_view,
                    shadowed_views,
                    Vec::new(),
                )
            }
            ProjectionOutcome::Conflict(conflict) => (
                ExplainStatus::Conflict,
                None,
                conflict
                    .contenders
                    .iter()
                    .map(|candidate| {
                        explain_candidate(
                            context,
                            candidate,
                            Some("same-name conflict remains unresolved".to_string()),
                        )
                    })
                    .collect(),
                vec![DiagnosticIssue {
                    severity: DiagnosticSeverity::Error,
                    code: "projected-name-conflict".to_string(),
                    skill: Some(skill_name.to_string()),
                    scope: Some(scope.as_str().to_string()),
                    target: None,
                    path: None,
                    trust: None,
                    safety: None,
                    message: format!("same-name conflict remains for '{}'", skill_name),
                    fix: Some(
                        "adjust manifest priorities or remove one of the conflicting skill sources"
                            .to_string(),
                    ),
                }],
            ),
        },
        None => (
            ExplainStatus::Missing,
            None,
            Vec::new(),
            missing_skill_issues(analysis, scope, skill_name),
        ),
    };

    let resolved_winner = graph
        .projection_for(skill_name)
        .and_then(|projection| projection.winner());
    let owner_scope = bundled_owner_scope(
        context,
        skill_name,
        projection.is_none() && explain_issues.is_empty() && related_issues.is_empty(),
    );
    let targets = explain_targets(
        context,
        scope,
        skill_name,
        target_plan.as_ref(),
        status,
        resolved_winner,
        owner_scope.as_deref(),
    )?;
    let active_projection_matches_winner = match (winner.as_ref(), target_plan.as_ref()) {
        (Some(_winner), Some(plan)) => {
            let Some(winner_candidate) = graph
                .projection_for(skill_name)
                .and_then(|projection| projection.winner())
            else {
                return Err(AppError::ResolutionValidation {
                    message: format!(
                        "selected explain state for '{skill_name}' is missing a winning candidate"
                    ),
                });
            };
            plan.assignments
                .iter()
                .try_fold(true, |matches, assignment| {
                    let root = planner::resolve_runtime_root_path(context, &assignment.root)?;
                    let path = root.join(skill_name);
                    if ProjectionObservation::observe(&path) != ProjectionObservation::Present {
                        // Missing or unreadable destinations cannot match the
                        // winner; the per-target observation records the state.
                        return Ok::<_, AppError>(false);
                    }
                    let differs = first_projection_difference(context, &path, winner_candidate)?;
                    Ok::<_, AppError>(matches && differs.is_none())
                })?
        }
        (Some(_), None) => true,
        _ => false,
    };
    let active_differs_from_pinned_source = match (&winner, managed_effective_version.as_ref()) {
        (Some(winner), Some(effective_version_hash)) => {
            (winner.effective_version_hash.as_ref() != Some(effective_version_hash))
                || !active_projection_matches_winner
        }
        (Some(_), None) => false,
        (None, Some(_)) => true,
        (None, None) => false,
    };

    let mut issues = related_issues;
    issues.extend(explain_issues);
    if explain_must_scan_for_stale_copies(status, resolved_winner, scope)
        && let Some(plan) = target_plan.as_ref()
    {
        // A leftover generated copy keeps every assigned runtime loading the
        // skill, so the report must not read as a plain absence. The same
        // holds for a winner outside this scope: the copy is not a projection
        // this scope plans, so it must surface the way `doctor` reports it.
        issues.extend(stale_generated_projection_issues(
            context,
            &store,
            graph,
            scope,
            plan,
            &analysis.manifest,
            Some(skill_name),
        )?);
    }
    let drift = ExplainDrift {
        active_projection_matches_winner,
        active_differs_from_pinned_source,
        pinned_reference: snapshot
            .pin
            .as_ref()
            .map(|pin| pin.requested_reference.clone()),
        effective_version_hash: managed_effective_version,
        detached: snapshot
            .install
            .as_ref()
            .is_some_and(|install| install.detached),
        forked: snapshot
            .install
            .as_ref()
            .is_some_and(|install| install.forked),
    };

    Ok(ExplainReport {
        skill: skill_name.to_string(),
        scope: scope_label,
        status,
        winner,
        shadowed,
        targets,
        drift,
        owner_scope,
        issues,
    })
}

fn build_inspect_report(
    context: &AppContext,
    analysis: &WorkspaceAnalysis,
    skill_name: &str,
) -> Result<InspectReport, AppError> {
    let scope = selected_managed_scope(context);
    let scope_label = scope.as_str().to_string();
    let related_issues = related_issues_for_skill(&analysis.validation.issues, skill_name);
    let Some(graph) = &analysis.graph else {
        // A graph that failed to build leaves the name unresolved, so the
        // bundled name still points at the scope that owns it.
        let owner_scope = bundled_owner_scope(context, skill_name, related_issues.is_empty());
        return Ok(InspectReport {
            skill: skill_name.to_string(),
            scope: scope_label,
            status: ExplainStatus::Missing,
            candidate: None,
            trust: None,
            safety: None,
            missing_credentials: Vec::new(),
            missing_tools: Vec::new(),
            targets: Vec::new(),
            owner_scope,
            issues: related_issues,
        });
    };
    let store = LocalStateStore::open_default_for(&context.working_directory)?;
    let reviews = crate::trust::ReviewIndex::load(&store)?;

    let targets = selected_targets(context, &analysis.manifest)?;
    let target_plan = if targets.is_empty() {
        None
    } else {
        Some(planner::plan_target_roots(
            &AdapterRegistry::new(),
            target_scope_from_managed(scope),
            analysis.manifest.projection.policy,
            &targets,
            &analysis.manifest.adapters,
        )?)
    };

    let projection = graph.projection_for(skill_name);
    let (status, candidate, trust, safety, missing_credentials, missing_tools, inspect_issues) =
        match projection {
            Some(projection) => match &projection.outcome {
                ProjectionOutcome::Selected { winner, trace, .. } => {
                    let safety = issue_safety(&winner.skill.safety);
                    (
                        ExplainStatus::Selected,
                        Some(explain_candidate(
                            context,
                            winner,
                            Some(explain_winner_reason(trace)),
                        )),
                        Some(crate::trust::trust_for_candidate(
                            winner,
                            reviews.approved_for_candidate(winner),
                        )),
                        safety,
                        winner.skill.safety.missing_required_credentials(),
                        winner.skill.safety.missing_required_tools(),
                        Vec::new(),
                    )
                }
                ProjectionOutcome::Conflict(conflict) => (
                    ExplainStatus::Conflict,
                    None,
                    None,
                    None,
                    Vec::new(),
                    Vec::new(),
                    vec![DiagnosticIssue {
                        severity: DiagnosticSeverity::Error,
                        code: "projected-name-conflict".to_string(),
                        skill: Some(skill_name.to_string()),
                        scope: Some(scope.as_str().to_string()),
                        target: None,
                        path: None,
                        trust: None,
                        safety: None,
                        message: format!(
                            "same-name conflict remains for '{}' across {}",
                            skill_name,
                            conflict
                                .contenders
                                .iter()
                                .map(candidate_label)
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                        fix: Some(
                            "adjust manifest priorities or remove one of the conflicting skill sources"
                                .to_string(),
                        ),
                    }],
                ),
            },
            None => (
                ExplainStatus::Missing,
                None,
                None,
                None,
                Vec::new(),
                Vec::new(),
                missing_skill_issues(analysis, scope, skill_name),
            ),
        };

    let resolved_winner = graph
        .projection_for(skill_name)
        .and_then(|projection| projection.winner());
    let owner_scope = bundled_owner_scope(
        context,
        skill_name,
        projection.is_none() && inspect_issues.is_empty() && related_issues.is_empty(),
    );
    let targets = explain_targets(
        context,
        scope,
        skill_name,
        target_plan.as_ref(),
        status,
        resolved_winner,
        owner_scope.as_deref(),
    )?;
    let mut issues = related_issues;
    issues.extend(inspect_issues);
    if explain_must_scan_for_stale_copies(status, resolved_winner, scope)
        && let Some(plan) = target_plan.as_ref()
    {
        // Preflight must see the same leftover copy explain reports, because
        // a runtime keeps loading it. That includes a copy left under a
        // winner this scope does not manage.
        issues.extend(stale_generated_projection_issues(
            context,
            &store,
            graph,
            scope,
            plan,
            &analysis.manifest,
            Some(skill_name),
        )?);
    }

    Ok(InspectReport {
        skill: skill_name.to_string(),
        scope: scope_label,
        status,
        candidate,
        trust,
        safety,
        missing_credentials,
        missing_tools,
        targets,
        owner_scope,
        issues,
    })
}

fn explain_targets(
    context: &AppContext,
    scope: ManagedScope,
    skill_name: &str,
    plan: Option<&ProjectionPlan>,
    status: ExplainStatus,
    winner: Option<&ResolvedSkillCandidate>,
    owner_scope: Option<&str>,
) -> Result<Vec<ExplainTarget>, AppError> {
    let Some(plan) = plan else {
        return Ok(Vec::new());
    };
    // A winner outside the requested management scope never projects into the
    // roots this report observes. The observations then report absence from
    // that scope, and no `skillctl sync` run in it can change them, so the fix
    // must name the scope the winner does project into.
    let foreign_winner_scope = winner
        .filter(|candidate| candidate.scope != skill_scope_from_managed(scope))
        .map(|candidate| skill_scope_label(candidate.scope).to_string());
    // A name the requested scope cannot resolve at all, because the bundled
    // skill owns it in another scope, reports the same absence.
    let owner_scope = owner_scope.filter(|_| status == ExplainStatus::Missing);

    plan.assignments
        .iter()
        .map(|assignment| {
            let root = planner::resolve_runtime_root_path(context, &assignment.root)?;
            let path = root.join(skill_name);
            let presence = ProjectionObservation::observe(&path);
            let matches_winner = match (status, winner, presence) {
                (ExplainStatus::Selected, Some(winner), ProjectionObservation::Present) => {
                    Some(first_projection_difference(context, &path, winner)?.is_none())
                }
                _ => None,
            };
            // A copy under a foreign winner is never a projection this scope
            // plans, so it must not count as verified visibility.
            let visible = status == ExplainStatus::Selected
                && matches_winner == Some(true)
                && foreign_winner_scope.is_none();
            let observed_digest = if presence == ProjectionObservation::Present {
                projection_content_digest(&path)?
            } else {
                None
            };
            let expected_digest = match winner {
                Some(winner) => Some(winner_content_digest(winner)?),
                None => None,
            };

            let reason = match (status, foreign_winner_scope.as_deref()) {
                (ExplainStatus::Selected, Some(winner_scope)) => match presence {
                    ProjectionObservation::Present => format!(
                        "a projection exists at this root although the active winner is \
                         managed at {winner_scope} scope"
                    ),
                    ProjectionObservation::Missing => format!(
                        "the active winner is managed at {winner_scope} scope, so it does not \
                         project into {} scope",
                        scope.as_str()
                    ),
                    ProjectionObservation::NotADirectory => {
                        "the planned projection path exists but is not a directory".to_string()
                    }
                    ProjectionObservation::Unreadable => {
                        "the planned projection path could not be inspected".to_string()
                    }
                },
                (ExplainStatus::Selected, None) => match (presence, matches_winner) {
                    (ProjectionObservation::Present, Some(true)) => {
                        "the active winner is materialized at this root and matches the \
                         resolved content"
                            .to_string()
                    }
                    (ProjectionObservation::Present, _) => {
                        "a projection exists at this root but does not match the active \
                         winner"
                            .to_string()
                    }
                    (ProjectionObservation::Missing, _) => {
                        "no projection currently exists at this root".to_string()
                    }
                    (ProjectionObservation::NotADirectory, _) => {
                        "the planned projection path exists but is not a directory".to_string()
                    }
                    (ProjectionObservation::Unreadable, _) => {
                        "the planned projection path could not be inspected".to_string()
                    }
                },
                (ExplainStatus::Conflict, _) => {
                    "a same-name conflict prevents a single active winner".to_string()
                }
                (ExplainStatus::Missing, _) => match (presence, owner_scope) {
                    (ProjectionObservation::Present, _) => {
                        "a projection directory exists at this root but no candidate \
                         currently resolves to this projected skill name"
                            .to_string()
                    }
                    (_, Some(owner_scope)) => format!(
                        "this skill is managed at {owner_scope} scope, so {} scope plans no \
                         projection for it",
                        scope.as_str()
                    ),
                    (_, None) => {
                        "no candidate currently resolves to this projected skill name".to_string()
                    }
                },
            };
            // The copy holds edits skillctl did not record, so the repair
            // must name the update strategies that resolve them. A copy that
            // only went stale against the winner stays repairable by `sync`.
            // A copy under a foreign winner is excluded: this scope plans no
            // projection for it, so the update strategies of the managed
            // import do not apply. The stale-copy issue reports that copy.
            let preserved_reason = match (winner, matches_winner, presence) {
                (Some(winner), Some(false), ProjectionObservation::Present)
                    if status == ExplainStatus::Selected && foreign_winner_scope.is_none() =>
                {
                    projection_preserved_reason(context, &path, winner)?
                }
                _ => None,
            };
            let fix = match preserved_reason {
                Some(reason) => Some(preserved_copy_fix(
                    context, scope, winner, skill_name, reason,
                )?),
                None => match foreign_winner_scope
                    .as_deref()
                    .filter(|_| status == ExplainStatus::Selected)
                    // The bundled name has no candidate here, so the step sends
                    // the reader to the scope that owns it for the same reason.
                    .or(owner_scope)
                {
                    Some(owner_scope) => {
                        explain_foreign_winner_fix(skill_name, owner_scope, presence)
                    }
                    None => explain_target_fix(status, presence, matches_winner, scope),
                },
            };

            Ok(ExplainTarget {
                target: assignment.target,
                root: planner::display_path(context, &root),
                path: planner::display_path(context, &path),
                visible,
                reason,
                observed_presence: presence,
                observed_matches_winner: matches_winner,
                observed_digest,
                expected_digest,
                observed_preserved_reason: preserved_reason,
                fix,
            })
        })
        .collect()
}

/// Return the repair step for one target observed under a skill that lives
/// outside the requested management scope.
///
/// The requested scope plans no projection for that skill, so an absent
/// destination is not a gap a `skillctl sync` in it could close; the step sends
/// the reader to the scope that owns the projections instead. A destination
/// that holds a copy carries no step of its own, because the stale-copy issue
/// reports that copy and knows whether `sync` may prune it.
fn explain_foreign_winner_fix(
    skill_name: &str,
    winner_scope: &str,
    presence: ProjectionObservation,
) -> Option<String> {
    if presence == ProjectionObservation::Present {
        return None;
    }
    Some(format!(
        "run 'skillctl --scope {winner_scope} explain {skill_name}' to check the projections \
         it plans, or 'skillctl --scope {winner_scope} sync' to refresh them"
    ))
}

/// Return the repair step for one explain target, when a repair is needed.
///
/// The step names the `sync` command [`scoped_sync_command`] returns for
/// `scope`, because a plain `skillctl sync` cannot materialize or refresh a
/// root another scope plans.
fn explain_target_fix(
    status: ExplainStatus,
    presence: ProjectionObservation,
    matches_winner: Option<bool>,
    scope: ManagedScope,
) -> Option<String> {
    let sync = scoped_sync_command(scope);
    match status {
        ExplainStatus::Conflict => Some(
            "adjust manifest priorities or remove one of the conflicting skill sources".to_string(),
        ),
        ExplainStatus::Missing => Some(format!(
            "install or enable the skill, then run {sync} to materialize projections"
        )),
        ExplainStatus::Selected => match (presence, matches_winner) {
            (ProjectionObservation::Present, Some(true)) => None,
            (ProjectionObservation::Present, _) => {
                Some(format!("run {sync} to refresh generated projections"))
            }
            (ProjectionObservation::Missing, _) => {
                Some(format!("run {sync} to materialize the missing projection"))
            }
            (ProjectionObservation::NotADirectory, _) => Some(format!(
                "remove the blocking entry at the planned path and run {sync}"
            )),
            (ProjectionObservation::Unreadable, _) => Some(format!(
                "check directory permissions on the runtime root and run {sync}"
            )),
        },
    }
}

fn explain_candidate(
    context: &AppContext,
    candidate: &ResolvedSkillCandidate,
    why: Option<String>,
) -> ExplainCandidate {
    ExplainCandidate {
        scope: skill_scope_label(candidate.scope).to_string(),
        internal_id: internal_id_label(&candidate.internal_id),
        source_class: source_class_label(candidate.source_class).to_string(),
        root: planner::display_path(context, &candidate.skill.root),
        why: why
            .unwrap_or_else(|| "candidate participates in the effective-skill graph".to_string()),
        import_id: candidate.import.as_ref().map(|import| import.id.clone()),
        overlay_root: candidate
            .overlay
            .as_ref()
            .map(|overlay| planner::display_path(context, &overlay.root)),
        resolved_revision: candidate
            .import
            .as_ref()
            .map(|import| import.resolved_revision.clone()),
        effective_version_hash: candidate
            .import
            .as_ref()
            .map(|import| import.effective_version_hash.clone()),
    }
}

fn explain_winner_reason(trace: &resolver::ResolutionTrace) -> String {
    match trace.decisive_stage {
        ResolutionStage::ManifestPriority => trace.manifest_priority.map_or_else(
            || "manifest priority selected the active winner".to_string(),
            |priority| format!("manifest priority {priority} selected the active winner"),
        ),
        ResolutionStage::SourceClass => match trace.source_class {
            SkillSourceClass::CanonicalLocal => {
                "source-class precedence prefers canonical local skills over imported skills"
                    .to_string()
            }
            SkillSourceClass::OverriddenImported => {
                "source-class precedence prefers overlay-managed imported skills over plain imports"
                    .to_string()
            }
            SkillSourceClass::Imported => {
                "source-class precedence left the imported candidate as the active winner"
                    .to_string()
            }
        },
    }
}

fn related_issues_for_skill(issues: &[DiagnosticIssue], skill_name: &str) -> Vec<DiagnosticIssue> {
    issues
        .iter()
        .filter(|issue| issue.skill.as_deref() == Some(skill_name))
        .cloned()
        .collect()
}

/// Warning that reports one import the manifest declares with `enabled: false`.
///
/// A disabled import keeps its manifest declaration and its lockfile pin, so
/// the disable decision is the only reason no candidate resolves. Reporting
/// the pin as stale would invite hand-editing `.agents/skillctl.lock` and
/// break `restore --frozen`. The lockfile pins imports by name without a
/// scope, so a declaration at another scope owns the same entry, and the
/// repair for it must name that scope.
fn disabled_import_issue(
    manifest: &WorkspaceManifest,
    scope: ManagedScope,
    import: &crate::manifest::ImportDefinition,
) -> DiagnosticIssue {
    let skill_name = import.id.as_str();
    let import_scope = managed_scope_from_manifest(import.scope);
    let (message, fix) = if import_scope == scope {
        (
            format!("manifest import '{skill_name}' exists but is disabled"),
            format!(
                "run skillctl enable {skill_name} to project it again, or skillctl remove \
                 {skill_name} to drop the import and its lockfile entry"
            ),
        )
    } else {
        (
            format!(
                "manifest import '{}' exists at {} scope but is disabled",
                skill_name,
                import_scope.as_str()
            ),
            format!(
                "run 'skillctl --scope {} enable {}' to project it again, or 'skillctl \
                 --scope {} remove {}' to drop the import and its lockfile entry",
                import_scope.as_str(),
                skill_name,
                import_scope.as_str(),
                skill_name
            ),
        )
    };
    DiagnosticIssue {
        severity: DiagnosticSeverity::Warning,
        code: "disabled-import".to_string(),
        skill: Some(skill_name.to_string()),
        scope: Some(import_scope.as_str().to_string()),
        target: None,
        path: Some(manifest.path.display().to_string()),
        trust: None,
        safety: None,
        message,
        fix: Some(fix),
    }
}

/// Disabled-import warnings for every import the manifest declares with
/// `enabled: false`.
///
/// `explain` and `inspect` report the disable for the one skill the reader
/// names, so `doctor` must report the same state workspace-wide. A run that
/// reports zero errors and zero warnings would otherwise read as health for a
/// skill `explain` warns about in the same workspace.
fn disabled_import_issues(
    manifest: &WorkspaceManifest,
    scope: ManagedScope,
) -> Vec<DiagnosticIssue> {
    manifest
        .imports
        .iter()
        .filter(|import| !import.enabled)
        .map(|import| disabled_import_issue(manifest, scope, import))
        .collect()
}

fn missing_skill_issues(
    analysis: &WorkspaceAnalysis,
    scope: ManagedScope,
    skill_name: &str,
) -> Vec<DiagnosticIssue> {
    let mut issues = Vec::new();

    let declared_import = analysis
        .manifest
        .imports
        .iter()
        .find(|import| import.id == skill_name);

    if let Some(import) = declared_import.filter(|import| !import.enabled) {
        issues.push(disabled_import_issue(&analysis.manifest, scope, import));
    }

    // Only a name no manifest declares at all leaves the pin without an owner.
    if analysis.lockfile.imports.contains_key(skill_name) && declared_import.is_none() {
        issues.push(DiagnosticIssue {
            severity: DiagnosticSeverity::Warning,
            code: "stale-lockfile-entry".to_string(),
            skill: Some(skill_name.to_string()),
            scope: Some(scope.as_str().to_string()),
            target: None,
            path: Some(analysis.lockfile.path.display().to_string()),
            trust: None,
            safety: None,
            message: format!(
                "lockfile still contains '{}' even though no active candidate resolves for it",
                skill_name
            ),
            fix: Some(format!(
                "run 'skillctl remove {skill_name}' to clear the entry, or 'skillctl install \
                 <source> --name {skill_name}' to declare the import again"
            )),
        });
    }

    issues
}

fn render_diagnostic_report(
    command: &'static str,
    report: DiagnosticReport,
) -> Result<AppResponse, AppError> {
    let summary = diagnostic_summary_line(command, &report.summary);
    let error_messages = report
        .issues
        .iter()
        .filter(|issue| issue.severity == DiagnosticSeverity::Error)
        .map(error_line)
        .collect::<Vec<_>>();
    let warning_messages = report
        .issues
        .iter()
        .filter(|issue| issue.severity == DiagnosticSeverity::Warning)
        .map(issue_line)
        .collect::<Vec<_>>();

    if error_messages.is_empty() {
        let response = AppResponse::success(command)
            .with_summary(summary)
            .with_data(serde_json::to_value(&report)?);
        Ok(with_warning_messages(response, warning_messages))
    } else {
        Ok(AppResponse {
            ok: false,
            command,
            warnings: warning_messages,
            errors: error_messages,
            data: serde_json::to_value(&report)?,
            summary: Some(summary),
            detail: Vec::new(),
            status_override: Some(ExitStatus::ValidationFailure),
        })
    }
}

fn with_warning_messages(mut response: AppResponse, warnings: Vec<String>) -> AppResponse {
    response.warnings.extend(warnings);
    response
}

fn report_from_issues(
    checked_skill_count: usize,
    mut issues: Vec<DiagnosticIssue>,
) -> DiagnosticReport {
    sort_issues(&mut issues);
    issues.dedup();

    let error_count = issues
        .iter()
        .filter(|issue| issue.severity == DiagnosticSeverity::Error)
        .count();
    let warning_count = issues
        .iter()
        .filter(|issue| issue.severity == DiagnosticSeverity::Warning)
        .count();

    DiagnosticReport {
        summary: DiagnosticSummary {
            checked_skill_count,
            error_count,
            warning_count,
        },
        issues,
        adapters: Vec::new(),
    }
}

fn sort_issues(issues: &mut [DiagnosticIssue]) {
    issues.sort_by(|left, right| {
        left.severity
            .cmp(&right.severity)
            .then_with(|| left.code.cmp(&right.code))
            .then_with(|| left.skill.cmp(&right.skill))
            .then_with(|| left.scope.cmp(&right.scope))
            .then_with(|| left.target.cmp(&right.target))
            .then_with(|| left.path.cmp(&right.path))
            .then_with(|| left.message.cmp(&right.message))
    });
}

fn diagnostic_summary_line(command: &str, summary: &DiagnosticSummary) -> String {
    format!(
        "{} checked {} skill{}: {} error{}, {} warning{}.",
        command,
        summary.checked_skill_count,
        plural_suffix(summary.checked_skill_count),
        summary.error_count,
        plural_suffix(summary.error_count),
        summary.warning_count,
        plural_suffix(summary.warning_count),
    )
}

/// Repair advice for the unverified projections of an explain summary.
///
/// A copy that holds edits skillctl did not record is preserved by `sync`, so
/// the summary must not tell the user to run it. The per-target fix already
/// names the commands that resolve those edits, so the summary repeats it and
/// adds the refresh that always works. Returns `None` while every unverified
/// target stays repairable by a plain `sync` in the observed scope.
fn explain_preserved_copy_repair(report: &ExplainReport, scope: ManagedScope) -> Option<String> {
    let target = report
        .targets
        .iter()
        .find(|target| target.observed_preserved_reason.is_some())?;
    let reason = target
        .observed_preserved_reason
        .expect("the target holds a preserved reason");
    let fix = target.fix.as_deref()?;
    let sync = scoped_sync_command(scope);
    let mut detail = reason.detail().to_string();
    if let Some(first) = detail.get_mut(0..1) {
        first.make_ascii_uppercase();
    }
    // The update strategies need an update to apply, while an explicit
    // discard refreshes the copy whatever the upstream state is. The
    // unverifiable-copy fix and the gated fix already name that discard, so
    // the sentence is added only when the fix does not.
    let refresh = match reason {
        DriftReason::DirtyProjection if !fix.contains("sync --discard") => {
            format!(" '{sync} --discard' also refreshes the copy after a durable backup.")
        }
        _ => String::new(),
    };
    Some(format!(
        "{detail}, so a plain '{sync}' preserves the copy instead of \
         refreshing it; {fix}.{refresh}"
    ))
}

/// Sentence that reports the leftover generated copies of one skill and the
/// repair the issues already chose for them.
///
/// The issue fix already reflects whether pruning is enabled, so the summary
/// repeats it instead of recommending a sync that would not prune. The
/// fallback names the `sync` command qualified for `scope`, because a plain
/// run cannot prune a root another scope plans.
fn stale_generated_copy_sentence(stale_issues: &[&DiagnosticIssue], scope: ManagedScope) -> String {
    let stale_copies: Vec<&str> = stale_issues
        .iter()
        .filter_map(|issue| issue.path.as_deref())
        .collect();
    let fallback = format!(
        "run '{}' to prune the stale generated copy",
        scoped_sync_command(scope)
    );
    let fix = stale_issues
        .iter()
        .find_map(|issue| issue.fix.as_deref())
        .unwrap_or(fallback.as_str());
    let mut fix_chars = fix.chars();
    let fix_sentence = match fix_chars.next() {
        Some(first) => format!("{}{}.", first.to_ascii_uppercase(), fix_chars.as_str()),
        None => "Prune the stale generated copy.".to_string(),
    };
    format!(
        "a generated copy remains at {} and the runtimes that read those roots keep loading \
         it. {fix_sentence}",
        stale_copies.join(", "),
    )
}

fn stale_issues_for(report: &ExplainReport) -> Vec<&DiagnosticIssue> {
    report
        .issues
        .iter()
        .filter(|issue| issue.code == STALE_GENERATED_PROJECTION_CODE)
        .collect()
}

/// Summary for a winner that lives outside the scope the report observes.
///
/// The roots of that scope stay empty on purpose, so counting them as
/// unverified projections and recommending `skillctl sync` in that scope would
/// name a repair the command cannot deliver. The summary states the absence,
/// sends the reader to the winner's own scope, and keeps reporting a leftover
/// generated copy the way `doctor` does. Returns `None` while the winner lives
/// inside the observed scope.
fn explain_out_of_scope_summary(
    base: &str,
    report: &ExplainReport,
    scope: ManagedScope,
) -> Option<String> {
    let winner = report
        .winner
        .as_ref()
        .filter(|winner| winner.scope != report.scope)?;
    let stale_issues = stale_issues_for(report);
    let absence = format!(
        "The active winner is managed at {} scope, so {} scope plans no projection for it",
        winner.scope, report.scope
    );
    if stale_issues.is_empty() {
        return Some(format!(
            "{base} {absence}. Run 'skillctl --scope {} explain {}' to check the projections \
             it plans, or 'skillctl --scope {} sync' to refresh them.",
            winner.scope, report.skill, winner.scope
        ));
    }
    Some(format!(
        "{base} {absence}, but {}",
        stale_generated_copy_sentence(&stale_issues, scope)
    ))
}

/// Summary for a name no candidate resolves in the requested scope because the
/// bundled skill owns it in another one.
///
/// The bundled install exists even though this scope plans no projection for
/// it, so the summary must not read as plain absence. It names the scope that
/// owns the name and the commands that read it, and keeps reporting a leftover
/// generated copy the way `doctor` does. Returns `None` while the report
/// carries no owner scope.
fn explain_owner_scope_summary(report: &ExplainReport, scope: ManagedScope) -> Option<String> {
    let owner_scope = report.owner_scope.as_deref()?;
    let stale_issues = stale_issues_for(report);
    let base = format!(
        "{} is not currently visible in {} scope. The bundled {} skill is managed at {} scope",
        report.skill, report.scope, report.skill, owner_scope
    );
    if stale_issues.is_empty() {
        return Some(format!(
            "{base}. Run 'skillctl --scope {owner_scope} explain {}' to check the projections it \
             plans, or 'skillctl --scope {owner_scope} sync' to refresh them.",
            report.skill
        ));
    }
    Some(format!(
        "{base}, but {}",
        stale_generated_copy_sentence(&stale_issues, scope)
    ))
}

fn explain_summary(report: &ExplainReport, scope: ManagedScope) -> String {
    match report.status {
        ExplainStatus::Selected => {
            let base = format!(
                "{} resolves to {}.",
                report.skill,
                report
                    .winner
                    .as_ref()
                    .map_or("the active winner", |winner| winner.root.as_str())
            );
            if let Some(summary) = explain_out_of_scope_summary(&base, report, scope) {
                return summary;
            }
            let verified = report
                .targets
                .iter()
                .filter(|target| target.visible)
                .count();
            let total = report.targets.len();
            if total == 0 {
                base
            } else if verified == total {
                format!(
                    "{base} {verified} of {total} target projection{} verified.",
                    plural_suffix(total)
                )
            } else if let Some(repair) = explain_preserved_copy_repair(report, scope) {
                format!(
                    "{base} {verified} of {total} target projection{} verified. {repair}",
                    plural_suffix(total)
                )
            } else {
                format!(
                    "{base} {verified} of {total} target projection{} verified. Run \
                     '{}' to materialize or refresh the remaining projections.",
                    plural_suffix(total),
                    scoped_sync_command(scope)
                )
            }
        }
        ExplainStatus::Conflict => format!(
            "{} has a same-name conflict and no single active winner.",
            report.skill
        ),
        ExplainStatus::Missing => {
            if let Some(summary) = explain_owner_scope_summary(report, scope) {
                return summary;
            }
            let stale_issues = stale_issues_for(report);
            if stale_issues.is_empty() {
                format!("{} is not currently visible.", report.skill)
            } else {
                format!(
                    "{} is not currently visible, but {}",
                    report.skill,
                    stale_generated_copy_sentence(&stale_issues, scope)
                )
            }
        }
    }
}

fn skill_target_compatibility_issues(
    context: &AppContext,
    manifest: &WorkspaceManifest,
    skill: &SkillDefinition,
    code: &str,
) -> Vec<DiagnosticIssue> {
    let enabled_targets = manifest.targets.iter().copied().collect::<BTreeSet<_>>();
    let mut issues = Vec::new();
    let path = Some(planner::display_path(context, &skill.manifest_path));
    let skill_name = Some(skill.name.as_str().to_string());

    if skill
        .vendor_metadata
        .files
        .contains_key(Path::new(OPENAI_METADATA_FILE))
    {
        let unsupported: Vec<_> = enabled_targets
            .iter()
            .copied()
            .filter(|target| *target != TargetRuntime::Codex)
            .collect();
        if !unsupported.is_empty() {
            issues.push(DiagnosticIssue {
                severity: DiagnosticSeverity::Warning,
                code: code.to_string(),
                skill: skill_name.clone(),
                scope: None,
                target: None,
                path: path.clone(),
                trust: None,
                safety: None,
                message: format!(
                    "skill '{}' includes {} but enabled targets {} may ignore it",
                    skill.name.as_str(),
                    OPENAI_METADATA_FILE,
                    unsupported
                        .iter()
                        .map(|target| target.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                fix: Some("limit targets to compatible runtimes or keep vendor-specific metadata optional".to_string()),
            });
        }
    }

    let claude_fields: Vec<_> = skill
        .frontmatter
        .vendor_fields
        .keys()
        .filter(|field| CLAUDE_FRONTMATTER_FIELDS.contains(&field.as_str()))
        .cloned()
        .collect();
    if !claude_fields.is_empty() {
        let unsupported: Vec<_> = enabled_targets
            .iter()
            .copied()
            .filter(|target| {
                !matches!(
                    target,
                    TargetRuntime::ClaudeCode
                        | TargetRuntime::GithubCopilot
                        | TargetRuntime::Opencode
                )
            })
            .collect();
        if !unsupported.is_empty() {
            issues.push(DiagnosticIssue {
                severity: DiagnosticSeverity::Warning,
                code: code.to_string(),
                skill: skill_name,
                scope: None,
                target: None,
                path,
                trust: None,
                safety: None,
                message: format!(
                    "skill '{}' uses Claude-specific frontmatter fields {} that enabled targets {} may ignore",
                    skill.name.as_str(),
                    claude_fields.join(", "),
                    unsupported
                        .iter()
                        .map(|target| target.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                fix: Some("use only fields supported by the enabled targets or keep them optional".to_string()),
            });
        }
    }

    issues
}

fn selected_targets(
    context: &AppContext,
    manifest: &WorkspaceManifest,
) -> Result<Vec<TargetRuntime>, AppError> {
    if context.selector.targets.is_empty() {
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

fn ensure_directory_issue(
    context: &AppContext,
    code: &str,
    path: &Path,
    scope: ManagedScope,
    skill: &str,
    label: &str,
    fix: &str,
) -> Option<DiagnosticIssue> {
    match fs::metadata(path) {
        Ok(metadata) if metadata.is_dir() => None,
        Ok(_) => Some(DiagnosticIssue {
            severity: DiagnosticSeverity::Error,
            code: code.to_string(),
            skill: Some(skill.to_string()),
            scope: Some(scope.as_str().to_string()),
            target: None,
            path: Some(planner::display_path(context, path)),
            trust: None,
            safety: None,
            message: format!(
                "{} '{}' must be a directory",
                label,
                planner::display_path(context, path)
            ),
            fix: Some(fix.to_string()),
        }),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Some(DiagnosticIssue {
            severity: DiagnosticSeverity::Error,
            code: code.to_string(),
            skill: Some(skill.to_string()),
            scope: Some(scope.as_str().to_string()),
            target: None,
            path: Some(planner::display_path(context, path)),
            trust: None,
            safety: None,
            message: format!(
                "{} '{}' does not exist",
                label,
                planner::display_path(context, path)
            ),
            fix: Some(fix.to_string()),
        }),
        Err(source) => Some(DiagnosticIssue {
            severity: DiagnosticSeverity::Error,
            code: code.to_string(),
            skill: Some(skill.to_string()),
            scope: Some(scope.as_str().to_string()),
            target: None,
            path: Some(planner::display_path(context, path)),
            trust: None,
            safety: None,
            message: format!(
                "failed to inspect '{}': {}",
                planner::display_path(context, path),
                source
            ),
            fix: Some(fix.to_string()),
        }),
    }
}

fn apply_overlay_validation(
    context: &AppContext,
    manifest: &WorkspaceManifest,
    skill_id: &str,
    base_files: &BTreeMap<PathBuf, PathBuf>,
    issues: &mut Vec<DiagnosticIssue>,
) -> Result<BTreeMap<PathBuf, PathBuf>, AppError> {
    let Some(overlay_path) = manifest.overrides.get(skill_id) else {
        return Ok(base_files.clone());
    };

    let overlay_root = context.working_directory.join(overlay_path.as_str());
    match fs::metadata(&overlay_root) {
        Ok(metadata) if !metadata.is_dir() => {
            issues.push(DiagnosticIssue {
                severity: DiagnosticSeverity::Error,
                code: "missing-overlay-root".to_string(),
                skill: Some(skill_id.to_string()),
                scope: Some(ManagedScope::Workspace.as_str().to_string()),
                target: None,
                path: Some(planner::display_path(context, &overlay_root)),
                trust: None,
                safety: None,
                message: format!(
                    "overlay root '{}' must be a directory",
                    planner::display_path(context, &overlay_root)
                ),
                fix: Some("recreate the overlay directory or remove the override".to_string()),
            });
            return Ok(base_files.clone());
        }
        Ok(_) => {}
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            issues.push(DiagnosticIssue {
                severity: DiagnosticSeverity::Error,
                code: "missing-overlay-root".to_string(),
                skill: Some(skill_id.to_string()),
                scope: Some(ManagedScope::Workspace.as_str().to_string()),
                target: None,
                path: Some(planner::display_path(context, &overlay_root)),
                trust: None,
                safety: None,
                message: format!(
                    "overlay root '{}' does not exist",
                    planner::display_path(context, &overlay_root)
                ),
                fix: Some(
                    "run skillctl override <skill> or remove the override mapping".to_string(),
                ),
            });
            return Ok(base_files.clone());
        }
        Err(source) => {
            return Err(AppError::FilesystemOperation {
                action: "inspect overlay root",
                path: overlay_root,
                source,
            });
        }
    }

    let overlay_files = collect_file_map(&overlay_root, "overlay")?;
    let mut effective = base_files.clone();
    for (relative_path, source_path) in overlay_files {
        let normalized = match skill::normalize_overlay_relative_path(&relative_path) {
            Ok(path) => path,
            Err(error) => {
                issues.push(DiagnosticIssue {
                    severity: DiagnosticSeverity::Error,
                    code: "invalid-overlay-path".to_string(),
                    skill: Some(skill_id.to_string()),
                    scope: Some(ManagedScope::Workspace.as_str().to_string()),
                    target: None,
                    path: Some(planner::display_path(context, &source_path)),
                    trust: None,
                    safety: None,
                    message: error.to_string(),
                    fix: Some("remove the invalid overlay path or normalize it".to_string()),
                });
                continue;
            }
        };

        if !base_files.contains_key(&normalized) {
            issues.push(DiagnosticIssue {
                severity: DiagnosticSeverity::Error,
                code: "invalid-overlay-mapping".to_string(),
                skill: Some(skill_id.to_string()),
                scope: Some(ManagedScope::Workspace.as_str().to_string()),
                target: None,
                path: Some(planner::display_path(context, &source_path)),
                trust: None,
                safety: None,
                message: format!(
                    "overlay file '{}' does not map to a file in the imported skill",
                    planner::display_path(context, &source_path)
                ),
                fix: Some("remove the unmatched overlay file or add the file upstream before overriding it".to_string()),
            });
            continue;
        }

        effective.insert(normalized, source_path);
    }

    Ok(effective)
}

fn load_effective_vendor_metadata(
    files: &BTreeMap<PathBuf, PathBuf>,
) -> Result<SkillVendorMetadata, AppError> {
    let mut vendor_files = BTreeMap::new();
    let relative_path = PathBuf::from(OPENAI_METADATA_FILE);

    if let Some(path) = files.get(&relative_path) {
        let contents =
            fs::read_to_string(path).map_err(|source| AppError::FilesystemOperation {
                action: "read effective vendor metadata file",
                path: path.clone(),
                source,
            })?;
        vendor_files.insert(relative_path, contents);
    }

    Ok(SkillVendorMetadata {
        files: vendor_files,
    })
}

fn collect_file_map(
    root: &Path,
    kind: &'static str,
) -> Result<BTreeMap<PathBuf, PathBuf>, AppError> {
    ensure_directory(root, kind)?;
    let mut files = BTreeMap::new();
    collect_directory_files(root, root, kind, &mut files)?;
    Ok(files)
}

fn collect_directory_files(
    root: &Path,
    current: &Path,
    kind: &'static str,
    files: &mut BTreeMap<PathBuf, PathBuf>,
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
        let metadata =
            fs::symlink_metadata(&path).map_err(|source| AppError::FilesystemOperation {
                action: "inspect skill path",
                path: path.clone(),
                source,
            })?;

        if metadata.file_type().is_symlink() {
            return Err(AppError::ResolutionValidation {
                message: format!(
                    "{kind} '{}' contains unsupported symlink '{}'",
                    root.display(),
                    path.display()
                ),
            });
        }

        if metadata.is_dir() {
            collect_directory_files(root, &path, kind, files)?;
        } else if metadata.is_file() {
            let relative_path =
                normalize_relative_path(path.strip_prefix(root).map_err(|_| {
                    AppError::ResolutionValidation {
                        message: format!(
                            "{kind} path '{}' escaped the root '{}'",
                            path.display(),
                            root.display()
                        ),
                    }
                })?)?;
            files.insert(relative_path, path);
        } else {
            return Err(AppError::ResolutionValidation {
                message: format!(
                    "{kind} '{}' contains a non-file, non-directory entry '{}'",
                    root.display(),
                    path.display()
                ),
            });
        }
    }

    Ok(())
}

fn ensure_directory(path: &Path, label: &'static str) -> Result<(), AppError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| AppError::FilesystemOperation {
        action: "inspect skill directory",
        path: path.to_path_buf(),
        source,
    })?;
    if metadata.file_type().is_symlink() {
        return Err(AppError::ResolutionValidation {
            message: format!("{label} '{}' must not be a symlink", path.display()),
        });
    }
    if !metadata.is_dir() {
        return Err(AppError::ResolutionValidation {
            message: format!("{label} '{}' must be a directory", path.display()),
        });
    }
    Ok(())
}

fn normalize_relative_path(path: &Path) -> Result<PathBuf, AppError> {
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

/// Why a plain `sync` preserves one generated copy instead of refreshing it.
///
/// Returns `None` when the copy is clean, absent, or hand-authored, because a
/// plain `sync` repairs all three.
fn projection_preserved_reason(
    context: &AppContext,
    projection_root: &Path,
    winner: &ResolvedSkillCandidate,
) -> Result<Option<DriftReason>, AppError> {
    // A manifest-free workspace is a supported state, and this helper reads
    // only the projection mode, so the defaults answer it.
    let manifest = load_manifest_or_default(&context.working_directory)?;
    let desired = crate::apply::desired_projection_digest(
        winner,
        manifest.projection.mode == ProjectionMode::Symlink,
    )?;
    Ok(
        match crate::apply::inspect_projection(projection_root, Some(desired.as_str()))? {
            crate::apply::ProjectionState::Drift(drift) => Some(drift.reason),
            crate::apply::ProjectionState::Absent
            | crate::apply::ProjectionState::HandAuthored
            | crate::apply::ProjectionState::Clean => None,
        },
    )
}

/// Recommended repair for one drifted generated copy.
///
/// A copy that holds edits skillctl did not record is preserved by `sync`, so
/// plain `sync` is not a repair for it. The recommendation names the update
/// strategies that resolve the edits instead, unless the trust gate refuses
/// them for this skill or no install record holds the skill at all. A copy that
/// only went stale against the active winner is refreshed by a plain `sync` in
/// the scope that plans it.
fn projection_drift_fix(
    context: &AppContext,
    projection_root: &Path,
    winner: &ResolvedSkillCandidate,
    skill_name: &str,
    commands: RepairCommands,
    scope: ManagedScope,
) -> Result<String, AppError> {
    Ok(
        match projection_preserved_reason(context, projection_root, winner)? {
            Some(reason) => reason.repair(skill_name, commands, scope),
            None => format!(
                "run {} to refresh generated projections",
                scoped_sync_command(scope)
            ),
        },
    )
}

/// Which repair commands the advice for one projected skill may name.
///
/// Reports must not name `update --apply` strategies the gate refuses, nor an
/// update for a skill no install record holds, so every repair advice for a
/// drifted copy is built on this decision.
fn drift_repair_commands_for(
    context: &AppContext,
    store: &LocalStateStore,
    scope: ManagedScope,
    skill_name: &str,
    candidate: Option<&ResolvedSkillCandidate>,
) -> Result<RepairCommands, AppError> {
    let skill = ManagedSkillRef::new(scope, skill_name);
    crate::trust::drift_repair_commands(store, &skill, candidate, &context.working_directory)
}

/// Fix text for one explain target whose observed copy holds unrecorded edits.
fn preserved_copy_fix(
    context: &AppContext,
    scope: ManagedScope,
    winner: Option<&ResolvedSkillCandidate>,
    skill_name: &str,
    reason: DriftReason,
) -> Result<String, AppError> {
    let store = LocalStateStore::open_default_for(&context.working_directory)?;
    let commands = drift_repair_commands_for(context, &store, scope, skill_name, winner)?;
    Ok(reason.repair(skill_name, commands, scope))
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
            return Ok(Some(planner::display_path(context, projection_root)));
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
        return Ok(Some(planner::display_path(context, projection_root)));
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
            return Ok(Some(planner::display_path(context, &actual_path)));
        };

        if !file_contents_equal(&actual_path, &expected_path)? {
            return Ok(Some(planner::display_path(context, &actual_path)));
        }
    }

    if let Some((relative_path, _)) = expected.into_iter().next() {
        return Ok(Some(planner::display_path(
            context,
            &projection_root.join(relative_path),
        )));
    }

    Ok(None)
}

/// Compute a stable content digest over the observed projection files,
/// excluding generated projection metadata. Returns `None` is never expected
/// for a present directory but keeps the payload honest if the directory
/// became unreadable mid-inspection.
fn projection_content_digest(root: &Path) -> Result<Option<String>, AppError> {
    let mut relative_paths = Vec::new();
    collect_projection_files(root, root, &mut relative_paths)?;
    relative_paths.sort();

    let mut hasher = Sha256::new();
    for relative_path in &relative_paths {
        if relative_path == Path::new(PROJECTION_METADATA_FILE) {
            continue;
        }
        let full_path = root.join(relative_path);
        let contents = fs::read(&full_path).map_err(|source| AppError::FilesystemOperation {
            action: "read projected file for digest",
            path: full_path,
            source,
        })?;
        hash_file_entry(&mut hasher, relative_path, &contents);
    }

    Ok(Some(format!("sha256:{:x}", hasher.finalize())))
}

/// Compute the expected content digest for the winner's effective files,
/// using the same scheme as [`projection_content_digest`].
fn winner_content_digest(candidate: &ResolvedSkillCandidate) -> Result<String, AppError> {
    let mut entries = BTreeMap::new();
    for file in &candidate.files {
        if file.relative_path == Path::new(PROJECTION_METADATA_FILE) {
            continue;
        }
        entries.insert(file.relative_path.clone(), file.source_path.clone());
    }

    let mut hasher = Sha256::new();
    for (relative_path, source_path) in &entries {
        let contents = fs::read(source_path).map_err(|source| AppError::FilesystemOperation {
            action: "read effective file for digest",
            path: source_path.clone(),
            source,
        })?;
        hash_file_entry(&mut hasher, relative_path, &contents);
    }

    Ok(format!("sha256:{:x}", hasher.finalize()))
}

/// Hash one file entry as `path`, NUL, contents, NUL with forward-slash paths.
fn hash_file_entry(hasher: &mut Sha256, relative_path: &Path, contents: &[u8]) {
    let path_key = relative_path.to_string_lossy().replace(MAIN_SEPARATOR, "/");
    hasher.update(path_key.as_bytes());
    hasher.update(b"\0");
    hasher.update(contents);
    hasher.update(b"\0");
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

        files.push(
            path.strip_prefix(root)
                .map_err(|_| AppError::ResolutionValidation {
                    message: format!(
                        "projected path '{}' escaped the root '{}'",
                        path.display(),
                        root.display()
                    ),
                })?
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

fn load_manifest_or_default(working_directory: &Path) -> Result<WorkspaceManifest, AppError> {
    let (manifest, _) = WorkspaceManifest::load_from_workspace_or_default(working_directory)?;
    Ok(manifest)
}

fn imports_directory_for(
    working_directory: &Path,
    manifest: &WorkspaceManifest,
) -> Result<PathBuf, AppError> {
    if manifest.imports.iter().any(|import| import.enabled) {
        imports_store_root()
    } else {
        Ok(working_directory.join(UNUSED_IMPORTS_PLACEHOLDER_DIR))
    }
}

fn skill_error_issue(
    context: &AppContext,
    code: &str,
    root: &Path,
    scope: ManagedScope,
    error: AppError,
) -> DiagnosticIssue {
    DiagnosticIssue {
        severity: DiagnosticSeverity::Error,
        code: code.to_string(),
        skill: root
            .file_name()
            .and_then(|segment| segment.to_str())
            .map(std::string::ToString::to_string),
        scope: Some(scope.as_str().to_string()),
        target: None,
        path: Some(path_for_error(context, &error, root)),
        trust: None,
        safety: None,
        message: error.to_string(),
        fix: Some("fix the SKILL.md contents or remove the malformed skill".to_string()),
    }
}

fn path_for_error(context: &AppContext, error: &AppError, fallback: &Path) -> String {
    match error {
        AppError::FilesystemOperation { path, .. }
        | AppError::PathConflict { path, .. }
        | AppError::ManifestParse { path, .. }
        | AppError::ManifestValidation { path, .. }
        | AppError::LockfileParse { path, .. }
        | AppError::LockfileValidation { path, .. }
        | AppError::SkillParse { path, .. }
        | AppError::SkillValidation { path, .. } => planner::display_path(context, path),
        _ => planner::display_path(context, fallback),
    }
}

fn issue_line(issue: &DiagnosticIssue) -> String {
    let base = match (&issue.skill, &issue.target) {
        (Some(skill), Some(target)) => {
            format!("{} [{}]: {}", skill, target.as_str(), issue.message)
        }
        (Some(skill), None) => format!("{}: {}", skill, issue.message),
        (None, Some(target)) => format!("[{}]: {}", target.as_str(), issue.message),
        (None, None) => issue.message.clone(),
    };

    match &issue.fix {
        Some(fix) => format!("{base}; fix: {fix}"),
        None => base,
    }
}

/// Render one error-severity issue for the response error list. Warning lines
/// gain their `warning:` prefix from the human presenter, error lines are
/// printed verbatim and therefore carry their own prefix.
fn error_line(issue: &DiagnosticIssue) -> String {
    format!("error: {}", issue_line(issue))
}

fn inspect_summary(report: &InspectReport) -> String {
    match report.status {
        ExplainStatus::Selected => {
            let candidate = report
                .candidate
                .as_ref()
                .expect("selected inspect report includes a candidate");
            let trust_fragment = report.trust.as_ref().map_or_else(
                || "trust state unavailable".to_string(),
                |trust| match trust.effective_state {
                    crate::trust::TrustState::LocalTrusted => "trusted locally".to_string(),
                    crate::trust::TrustState::ImportedReviewed => "reviewed locally".to_string(),
                    crate::trust::TrustState::ImportedUnreviewed => "review required".to_string(),
                },
            );
            let safety = report.safety.as_ref();
            let capability_count = safety.map_or(0, |summary| summary.capabilities.len());
            let credential_count = safety.map_or(0, |summary| summary.credentials.len());
            let tool_count = safety.map_or(0, |summary| summary.dependencies.tools.len());
            let missing_credentials = report.missing_credentials.len();
            let missing_tools = report.missing_tools.len();
            // A winner outside the requested scope belongs to that scope, so
            // the label must not name the scope the request defaulted to. The
            // note keeps the requested scope visible and names the command
            // that checks the projections the winner does plan.
            let (scope_label, scope_note) = if candidate.scope == report.scope {
                (report.scope.as_str(), String::new())
            } else {
                let mut requested = report.scope.clone();
                if let Some(first) = requested.get_mut(0..1) {
                    first.make_ascii_uppercase();
                }
                (
                    candidate.scope.as_str(),
                    format!(
                        " {requested} scope plans no projection for it; run 'skillctl --scope {} \
                         explain {}' to check the projections it plans.",
                        candidate.scope, report.skill
                    ),
                )
            };

            format!(
                "Inspected preflight for {} in {} scope: {} ({}, {} capability declaration{}, {} credential declaration{}, {} tool requirement{}, {} missing credential{}, {} missing tool{}).{scope_note}",
                report.skill,
                scope_label,
                candidate.source_class,
                trust_fragment,
                capability_count,
                plural_suffix(capability_count),
                credential_count,
                plural_suffix(credential_count),
                tool_count,
                plural_suffix(tool_count),
                missing_credentials,
                plural_suffix(missing_credentials),
                missing_tools,
                plural_suffix(missing_tools),
            )
        }
        ExplainStatus::Conflict => format!(
            "Cannot inspect '{}' in {} scope because a same-name conflict remains unresolved.",
            report.skill, report.scope
        ),
        ExplainStatus::Missing => {
            // The bundled name belongs to the scope that owns it, so the
            // preflight sends the reader there instead of stopping at absence.
            if let Some(owner_scope) = report.owner_scope.as_deref() {
                return format!(
                    "No active skill named '{}' resolved for {} scope preflight inspection. The \
                     bundled {} skill is managed at {owner_scope} scope; run 'skillctl --scope \
                     {owner_scope} explain {}' to check the projections it plans.",
                    report.skill, report.scope, report.skill, report.skill
                );
            }
            format!(
                "No active skill named '{}' resolved for {} scope preflight inspection.",
                report.skill, report.scope
            )
        }
    }
}

fn candidate_label(candidate: &ResolvedSkillCandidate) -> String {
    format!(
        "{} ({})",
        candidate.skill.name.as_str(),
        source_class_label(candidate.source_class)
    )
}

fn source_class_label(source_class: SkillSourceClass) -> &'static str {
    match source_class {
        SkillSourceClass::CanonicalLocal => "canonical-local",
        SkillSourceClass::OverriddenImported => "overridden-imported",
        SkillSourceClass::Imported => "imported",
    }
}

fn internal_id_label(internal_id: &InternalSkillId) -> String {
    match internal_id {
        InternalSkillId::Local {
            scope,
            relative_path,
        } => {
            format!("local:{}:{}", skill_scope_label(*scope), relative_path)
        }
        InternalSkillId::Imported {
            scope,
            import_id,
            source_url,
            subpath,
        } => format!(
            "imported:{}:{}:{}#{}",
            skill_scope_label(*scope),
            import_id,
            source_url,
            subpath
        ),
    }
}

fn skill_scope_label(scope: SkillScope) -> &'static str {
    match scope {
        SkillScope::Workspace => "workspace",
        SkillScope::User => "user",
    }
}

fn selected_managed_scope(context: &AppContext) -> ManagedScope {
    match context.selector.scope.unwrap_or(Scope::Workspace) {
        Scope::Workspace => ManagedScope::Workspace,
        Scope::User => ManagedScope::User,
    }
}

fn target_scope_from_managed(scope: ManagedScope) -> TargetScope {
    match scope {
        ManagedScope::Workspace => TargetScope::Workspace,
        ManagedScope::User => TargetScope::User,
    }
}

fn skill_scope_from_managed(scope: ManagedScope) -> SkillScope {
    match scope {
        ManagedScope::Workspace => SkillScope::Workspace,
        ManagedScope::User => SkillScope::User,
    }
}

fn managed_scope_from_manifest(scope: ManifestScope) -> ManagedScope {
    match scope {
        ManifestScope::Workspace => ManagedScope::Workspace,
        ManifestScope::User => ManagedScope::User,
    }
}

fn plural_suffix(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}
