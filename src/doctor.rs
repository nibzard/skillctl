//! Diagnostics and validation domain entry points.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    path::{Component, Path, PathBuf},
};

use crate::{
    adapter::{AdapterRegistry, InstallModeRisk, TargetRuntime, TargetScope},
    app::AppContext,
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
        SkillDefinition, SkillVendorMetadata,
    },
    source::{imports_store_root, stored_import_root},
    state::{LocalStateStore, ManagedScope, ManagedSkillRef},
    trust::SkillTrust,
};
use serde::Serialize;

const UNUSED_IMPORTS_PLACEHOLDER_DIR: &str = ".agents/.skillctl-unused-imports";

/// Structured diagnostics report for `validate` and `doctor`.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct DiagnosticReport {
    /// Deterministic totals for the report.
    pub summary: DiagnosticSummary,
    /// Ordered set of actionable issues.
    pub issues: Vec<DiagnosticIssue>,
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
    /// Whether the requested skill is the active winner for the target.
    pub visible: bool,
    /// Plain-English explanation for the visibility result.
    pub reason: String,
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
    lockfile: WorkspaceLockfile,
    validation: ValidationArtifacts,
    graph: Option<EffectiveSkillGraph>,
}

struct BundledDoctorArtifacts {
    checked_skill_count: usize,
    issues: Vec<DiagnosticIssue>,
}

/// Handle `skillctl doctor`.
pub fn handle_doctor(
    context: &AppContext,
    _request: DoctorRequest,
) -> Result<AppResponse, AppError> {
    let analysis = analyze_workspace(context)?;
    let mut issues = analysis.validation.issues.clone();
    issues.extend(doctor_issues(context, &analysis)?);
    let bundled = bundled_doctor_artifacts(context)?;
    issues.extend(bundled.issues);
    let report = report_from_issues(
        analysis.validation.checked_skill_count + bundled.checked_skill_count,
        issues,
    );
    render_diagnostic_report("doctor", report)
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
    let status = if report.status == ExplainStatus::Conflict {
        ExitStatus::ValidationFailure
    } else {
        ExitStatus::Success
    };
    let summary = explain_summary(&report);
    let warnings = report
        .issues
        .iter()
        .filter(|issue| issue.severity == DiagnosticSeverity::Warning)
        .map(issue_line)
        .collect::<Vec<_>>();

    let response = AppResponse::success("explain")
        .with_summary(summary)
        .with_data(serde_json::to_value(&report)?)
        .with_exit_status(status);
    Ok(with_warning_messages(response, warnings))
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

fn bundled_doctor_artifacts(context: &AppContext) -> Result<BundledDoctorArtifacts, AppError> {
    let mut issues = bundled_base_issues(context)?;
    if selected_managed_scope(context) != ManagedScope::User {
        return Ok(BundledDoctorArtifacts {
            checked_skill_count: 0,
            issues,
        });
    }

    let store = LocalStateStore::open_default_for(&context.working_directory)?;
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
        if let Some(path) = builtin::projection_difference(context, &projection_root)? {
            issues.push(DiagnosticIssue {
                severity: DiagnosticSeverity::Warning,
                code: "projection-drift".to_string(),
                skill: Some("skillctl".to_string()),
                scope: Some(ManagedScope::User.as_str().to_string()),
                target: Some(assignment.target),
                path: Some(path),
                trust: None,
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
    let issues = bundled_base_issues(context)?;
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
                let differs = builtin::projection_difference(context, &root.join("skillctl"))?;
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
        issues,
    })
}

fn bundled_base_issues(context: &AppContext) -> Result<Vec<DiagnosticIssue>, AppError> {
    builtin::diagnostics(context)?
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
            let visible = status == ExplainStatus::Selected;
            let reason = match status {
                ExplainStatus::Selected => {
                    "the bundled skillctl asset projects into this user-scope root".to_string()
                }
                ExplainStatus::Conflict => {
                    "a same-name conflict prevents a single active winner".to_string()
                }
                ExplainStatus::Missing => {
                    "the bundled skillctl asset is not currently installed in user scope"
                        .to_string()
                }
            };

            Ok(ExplainTarget {
                target: assignment.target,
                root: planner::display_path(context, &root),
                path: planner::display_path(context, &path),
                visible,
                reason,
            })
        })
        .collect()
}

fn analyze_workspace(context: &AppContext) -> Result<WorkspaceAnalysis, AppError> {
    let manifest = load_manifest_or_default(&context.working_directory)?;
    let lockfile = load_lockfile_or_default(&context.working_directory)?;
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
        lockfile,
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
                        message: format!(
                            "canonical skills root '{}' contains a non-directory entry",
                            planner::display_path(context, &root)
                        ),
                        fix: Some("keep only skill directories under .agents/skills".to_string()),
                    });
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
        "reinstall the skill so the pinned source is restored",
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
) -> Result<Vec<DiagnosticIssue>, AppError> {
    let mut issues = Vec::new();
    let scope = selected_managed_scope(context);
    let targets = selected_targets(context, &analysis.manifest)?;

    issues.extend(stale_lockfile_issues(
        &analysis.manifest,
        &analysis.lockfile,
    ));

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
    issues.extend(script_risk_issues(scope, graph));

    if !targets.is_empty() {
        let plan = planner::plan_target_roots(
            &AdapterRegistry::new(),
            target_scope_from_managed(scope),
            analysis.manifest.projection.policy,
            &targets,
            &analysis.manifest.adapters,
        )?;
        issues.extend(projection_record_issues(context, graph, scope, &plan)?);
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

fn script_risk_issues(scope: ManagedScope, graph: &EffectiveSkillGraph) -> Vec<DiagnosticIssue> {
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

        issues.push(DiagnosticIssue {
            severity: DiagnosticSeverity::Warning,
            code: "script-risk".to_string(),
            skill: Some(candidate.skill.name.as_str().to_string()),
            scope: Some(scope.as_str().to_string()),
            target: None,
            path: Some(candidate.skill.root.display().to_string()),
            trust: Some(crate::trust::trust_for_candidate(candidate)),
            message: format!(
                "imported skill '{}' contains files under scripts/ and should be reviewed before use",
                candidate.skill.name.as_str()
            ),
            fix: Some("review the skill contents or fork it into local ownership".to_string()),
        });
    }

    issues
}

fn projection_record_issues(
    context: &AppContext,
    graph: &EffectiveSkillGraph,
    scope: ManagedScope,
    plan: &ProjectionPlan,
) -> Result<Vec<DiagnosticIssue>, AppError> {
    let mut issues = Vec::new();
    let store = LocalStateStore::open_default_for(&context.working_directory)?;
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
                message: format!(
                    "target '{}' is projected into '{}' but the manifest now plans '{}'",
                    record.target.as_str(),
                    record.physical_root,
                    expected_root
                ),
                fix: Some(
                    "run skillctl sync to regenerate projections in the planned roots".to_string(),
                ),
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
            if let Some(path) = first_projection_difference(context, &projection_root, winner)? {
                issues.push(DiagnosticIssue {
                    severity: DiagnosticSeverity::Warning,
                    code: "projection-drift".to_string(),
                    skill: Some(projection.name.clone()),
                    scope: Some(scope.as_str().to_string()),
                    target: Some(assignment.target),
                    path: Some(path),
                    trust: None,
                    message: format!(
                        "target '{}' currently materializes a copy that does not match the active winner",
                        assignment.target.as_str()
                    ),
                    fix: Some("run skillctl sync to refresh generated projections".to_string()),
                });
            }
        }
    }

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
            message: format!(
                "lockfile entry '{}' no longer has a matching manifest import",
                id
            ),
            fix: Some("remove the stale lockfile entry or reinstall the skill".to_string()),
        })
        .collect()
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

    let targets = explain_targets(
        context,
        skill_name,
        target_plan.as_ref(),
        status,
        winner.as_ref(),
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
                    let differs = first_projection_difference(
                        context,
                        &root.join(skill_name),
                        winner_candidate,
                    )?;
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
        issues,
    })
}

fn explain_targets(
    context: &AppContext,
    skill_name: &str,
    plan: Option<&ProjectionPlan>,
    status: ExplainStatus,
    winner: Option<&ExplainCandidate>,
) -> Result<Vec<ExplainTarget>, AppError> {
    let Some(plan) = plan else {
        return Ok(Vec::new());
    };

    plan.assignments
        .iter()
        .map(|assignment| {
            let root = planner::resolve_runtime_root_path(context, &assignment.root)?;
            let path = root.join(skill_name);
            let visible = status == ExplainStatus::Selected && winner.is_some();
            let reason = match status {
                ExplainStatus::Selected => {
                    "the selected winner projects into this physical root".to_string()
                }
                ExplainStatus::Conflict => {
                    "a same-name conflict prevents a single active winner".to_string()
                }
                ExplainStatus::Missing => {
                    "no candidate currently resolves to this projected skill name".to_string()
                }
            };

            Ok(ExplainTarget {
                target: assignment.target,
                root: planner::display_path(context, &root),
                path: planner::display_path(context, &path),
                visible,
                reason,
            })
        })
        .collect()
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

fn missing_skill_issues(
    analysis: &WorkspaceAnalysis,
    scope: ManagedScope,
    skill_name: &str,
) -> Vec<DiagnosticIssue> {
    let mut issues = Vec::new();

    if analysis.manifest.imports.iter().any(|import| {
        import.id == skill_name
            && import.scope == manifest_scope_from_managed(scope)
            && !import.enabled
    }) {
        issues.push(DiagnosticIssue {
            severity: DiagnosticSeverity::Warning,
            code: "disabled-import".to_string(),
            skill: Some(skill_name.to_string()),
            scope: Some(scope.as_str().to_string()),
            target: None,
            path: Some(analysis.manifest.path.display().to_string()),
            trust: None,
            message: format!("manifest import '{}' exists but is disabled", skill_name),
            fix: Some(format!("run skillctl enable {skill_name}")),
        });
    }

    if analysis.lockfile.imports.contains_key(skill_name) {
        issues.push(DiagnosticIssue {
            severity: DiagnosticSeverity::Warning,
            code: "stale-lockfile-entry".to_string(),
            skill: Some(skill_name.to_string()),
            scope: Some(scope.as_str().to_string()),
            target: None,
            path: Some(analysis.lockfile.path.display().to_string()),
            trust: None,
            message: format!(
                "lockfile still contains '{}' even though no active candidate resolves for it",
                skill_name
            ),
            fix: Some("remove the stale entry or reinstall the skill".to_string()),
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
        .map(issue_line)
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

fn explain_summary(report: &ExplainReport) -> String {
    match report.status {
        ExplainStatus::Selected => format!(
            "{} resolves to {}.",
            report.skill,
            report
                .winner
                .as_ref()
                .map_or("the active winner", |winner| winner.root.as_str())
        ),
        ExplainStatus::Conflict => format!(
            "{} has a same-name conflict and no single active winner.",
            report.skill
        ),
        ExplainStatus::Missing => format!("{} is not currently visible.", report.skill),
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
    match (&issue.skill, &issue.target) {
        (Some(skill), Some(target)) => format!(
            "{} {} [{}]: {}",
            issue.severity.severity_label(),
            skill,
            target.as_str(),
            issue.message
        ),
        (Some(skill), None) => {
            format!(
                "{} {}: {}",
                issue.severity.severity_label(),
                skill,
                issue.message
            )
        }
        (None, Some(target)) => format!(
            "{} [{}]: {}",
            issue.severity.severity_label(),
            target.as_str(),
            issue.message
        ),
        (None, None) => format!("{}: {}", issue.severity.severity_label(), issue.message),
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

fn manifest_scope_from_managed(scope: ManagedScope) -> ManifestScope {
    match scope {
        ManagedScope::Workspace => ManifestScope::Workspace,
        ManagedScope::User => ManifestScope::User,
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

impl DiagnosticSeverity {
    fn severity_label(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
        }
    }
}
