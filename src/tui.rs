//! Terminal UI domain entry points.
//!
//! The current TUI is a read-only dashboard over the same state, explain, and
//! history model used by the rest of the CLI. It does not perform lifecycle
//! writes on open, so inspection remains deterministic and safe to run
//! alongside other commands.

use std::{fmt::Write as _, fs, io};

use serde::Serialize;

use crate::{
    app::AppContext,
    doctor::{self, ExplainReport, ExplainStatus},
    error::AppError,
    history, planner,
    response::AppResponse,
    skill,
    source::SourceKind,
    state::{
        HistoryEntry, HistoryQuery, InstallRecord, LocalStateStore, ManagedScope, ManagedSkillRef,
        PinRecord, RollbackRecord, SkillStateSnapshot, UpdateCheckRecord,
    },
};

const SKILL_HISTORY_PREVIEW_LIMIT: usize = 5;
const HISTORY_PANEL_LIMIT: usize = 10;

/// Read-only terminal dashboard state for `skillctl tui`.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct TuiApp {
    /// Workspace root being inspected.
    pub workspace: String,
    /// Selected skill filter, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected_skill: Option<String>,
    /// Selected scope filter, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope_filter: Option<ManagedScope>,
    /// Selected target filters, when present.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub target_filters: Vec<String>,
    /// Per-skill dashboard cards.
    pub skills: Vec<TuiSkillCard>,
    /// History panel rendered below the skill cards.
    pub history: TuiHistoryPanel,
}

/// One skill card inside the TUI dashboard.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct TuiSkillCard {
    /// Managed skill identifier.
    pub skill: String,
    /// Managed scope.
    pub scope: ManagedScope,
    /// Current install state.
    pub installed: InstallRecord,
    /// Current pin, when one exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pin: Option<PinRecord>,
    /// Latest recorded update check, when one exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub update: Option<UpdateCheckRecord>,
    /// Overlay inspection details.
    pub overlay: TuiOverlayView,
    /// Known local modifications ordered newest first.
    pub local_modifications: Vec<crate::state::LocalModificationRecord>,
    /// Rollback summary for the skill.
    pub rollback: TuiRollbackSummary,
    /// Visibility and drift report shared with `skillctl explain`.
    pub visibility: ExplainReport,
    /// Recent history preview for the skill.
    pub history_preview: Vec<HistoryEntry>,
    /// Exact CLI command mappings for supported actions.
    pub actions: TuiActionMap,
}

/// Overlay state shown in the dashboard.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct TuiOverlayView {
    /// Whether an overlay is present or was detected in recent state.
    pub present: bool,
    /// Resolved overlay path, when one exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

/// Rollback state shown in the dashboard.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct TuiRollbackSummary {
    /// Number of rollback entries recorded for the skill.
    pub count: usize,
    /// Most recent rollback entry, when one exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest: Option<RollbackRecord>,
}

/// Exact CLI action mappings rendered by the TUI.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct TuiActionMap {
    /// Refresh update state.
    pub update: String,
    /// Inspect winner and target visibility.
    pub explain: String,
    /// Inspect filesystem paths.
    pub path: String,
    /// Inspect detailed history.
    pub history: String,
    /// Pin the skill to an exact revision.
    pub pin: String,
    /// Roll back the skill to a prior version or commit.
    pub rollback: String,
    /// Rebuild projections for the selected scope.
    pub sync: String,
}

/// History section shown beneath the skill cards.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct TuiHistoryPanel {
    /// Skill filter applied to the history section, when one exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skill: Option<String>,
    /// Scope filter applied to the history section, when one exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<ManagedScope>,
    /// Recent history entries.
    pub entries: Vec<HistoryEntry>,
}

/// Typed request for `skillctl tui`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct OpenTuiRequest;

/// Handle `skillctl tui`.
pub fn handle_open(
    context: &AppContext,
    _request: OpenTuiRequest,
) -> Result<AppResponse, AppError> {
    let app = build_app(context)?;
    let summary = render_dashboard(&app);

    Ok(AppResponse::success("tui")
        .with_summary(summary)
        .with_data(serde_json::to_value(&app)?))
}

fn build_app(context: &AppContext) -> Result<TuiApp, AppError> {
    let store = LocalStateStore::open_default()?;
    let selected_skill = context.selector.skill_name.clone();
    let managed_skills = selected_managed_skills(context, &store)?;
    let mut skills = Vec::with_capacity(managed_skills.len());

    for managed_skill in &managed_skills {
        skills.push(build_skill_card(context, &store, managed_skill)?);
    }

    Ok(TuiApp {
        workspace: context.working_directory.display().to_string(),
        selected_skill,
        scope_filter: context.selector.scope.map(managed_scope_from_cli),
        target_filters: context.selector.targets.clone(),
        history: history_panel(
            context,
            &store,
            if context.selector.skill_name.is_some() {
                managed_skills.first().cloned()
            } else {
                None
            },
        )?,
        skills,
    })
}

fn selected_managed_skills(
    context: &AppContext,
    store: &LocalStateStore,
) -> Result<Vec<ManagedSkillRef>, AppError> {
    if let Some(skill_name) = &context.selector.skill_name {
        return Ok(vec![skill::resolve_installed_skill(
            store,
            skill_name,
            context.selector.scope,
        )?]);
    }

    let requested_scope = context.selector.scope.map(managed_scope_from_cli);
    let mut installs = store.list_install_records()?;
    installs.retain(|record| requested_scope.is_none_or(|scope| record.skill.scope == scope));

    Ok(installs.into_iter().map(|record| record.skill).collect())
}

fn build_skill_card(
    context: &AppContext,
    store: &LocalStateStore,
    managed_skill: &ManagedSkillRef,
) -> Result<TuiSkillCard, AppError> {
    let snapshot = store.skill_snapshot(managed_skill)?;
    let install = snapshot
        .install
        .clone()
        .ok_or_else(|| AppError::ResolutionValidation {
            message: format!(
                "skill '{}' does not have an installed state record",
                managed_skill.skill_id
            ),
        })?;
    let scoped_context = history::context_for_scope(context, managed_skill.scope);
    let visibility =
        doctor::build_explain_report_for_tui(&scoped_context, &managed_skill.skill_id)?;
    let history_preview = skill_history_preview(store, managed_skill)?;

    Ok(TuiSkillCard {
        skill: managed_skill.skill_id.clone(),
        scope: managed_skill.scope,
        installed: install,
        pin: snapshot.pin.clone(),
        update: snapshot.latest_update_check.clone(),
        overlay: overlay_view(context, managed_skill, &snapshot, &visibility)?,
        local_modifications: snapshot.local_modifications.clone(),
        rollback: rollback_summary(&snapshot),
        visibility,
        history_preview,
        actions: action_map(managed_skill.scope, &managed_skill.skill_id),
    })
}

fn history_panel(
    context: &AppContext,
    store: &LocalStateStore,
    selected_skill: Option<ManagedSkillRef>,
) -> Result<TuiHistoryPanel, AppError> {
    let mut entries = if let Some(skill) = &selected_skill {
        store.history_entries(&HistoryQuery {
            skill: Some(skill.clone()),
            limit: Some(HISTORY_PANEL_LIMIT),
        })?
    } else {
        store.history_entries(&HistoryQuery::default())?
    };

    if selected_skill.is_none() {
        if let Some(scope) = context.selector.scope.map(managed_scope_from_cli) {
            entries.retain(|entry| entry.scope.is_none_or(|entry_scope| entry_scope == scope));
        }
        entries.truncate(HISTORY_PANEL_LIMIT);
    }

    Ok(TuiHistoryPanel {
        skill: selected_skill.as_ref().map(|skill| skill.skill_id.clone()),
        scope: selected_skill.as_ref().map(|skill| skill.scope),
        entries,
    })
}

fn skill_history_preview(
    store: &LocalStateStore,
    managed_skill: &ManagedSkillRef,
) -> Result<Vec<HistoryEntry>, AppError> {
    store.history_entries(&HistoryQuery {
        skill: Some(managed_skill.clone()),
        limit: Some(SKILL_HISTORY_PREVIEW_LIMIT),
    })
}

fn overlay_view(
    context: &AppContext,
    managed_skill: &ManagedSkillRef,
    snapshot: &SkillStateSnapshot,
    visibility: &ExplainReport,
) -> Result<TuiOverlayView, AppError> {
    let overlay_root = context
        .working_directory
        .join(".agents/overlays")
        .join(&managed_skill.skill_id);
    let path = overlay_path(context, &overlay_root)?;
    let present = path.is_some()
        || visibility
            .winner
            .as_ref()
            .and_then(|winner| winner.overlay_root.as_ref())
            .is_some()
        || snapshot
            .latest_update_check
            .as_ref()
            .is_some_and(|update| update.overlay_detected)
        || snapshot
            .local_modifications
            .iter()
            .any(|modification| modification.kind == crate::state::LocalModificationKind::Overlay);

    Ok(TuiOverlayView {
        present,
        path: path.or_else(|| {
            visibility
                .winner
                .as_ref()
                .and_then(|winner| winner.overlay_root.clone())
        }),
    })
}

fn overlay_path(
    context: &AppContext,
    overlay_root: &std::path::Path,
) -> Result<Option<String>, AppError> {
    match fs::metadata(overlay_root) {
        Ok(metadata) if metadata.is_dir() => Ok(Some(planner::display_path(context, overlay_root))),
        Ok(_) => Ok(Some(planner::display_path(context, overlay_root))),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(AppError::FilesystemOperation {
            action: "inspect overlay root",
            path: overlay_root.to_path_buf(),
            source,
        }),
    }
}

fn rollback_summary(snapshot: &SkillStateSnapshot) -> TuiRollbackSummary {
    TuiRollbackSummary {
        count: snapshot.rollbacks.len(),
        latest: snapshot.rollbacks.first().cloned(),
    }
}

fn action_map(scope: ManagedScope, skill: &str) -> TuiActionMap {
    let prefix = scope_prefix(scope);

    TuiActionMap {
        update: format!("skillctl {prefix}update {skill}"),
        explain: format!("skillctl {prefix}explain {skill}"),
        path: format!("skillctl {prefix}path {skill}"),
        history: format!("skillctl {prefix}history {skill}"),
        pin: format!("skillctl {prefix}pin {skill} <ref>"),
        rollback: format!("skillctl {prefix}rollback {skill} <version-or-commit>"),
        sync: format!("skillctl {prefix}sync"),
    }
}

fn scope_prefix(scope: ManagedScope) -> String {
    format!("--scope {} ", scope.as_str())
}

fn managed_scope_from_cli(scope: crate::cli::Scope) -> ManagedScope {
    match scope {
        crate::cli::Scope::Workspace => ManagedScope::Workspace,
        crate::cli::Scope::User => ManagedScope::User,
    }
}

fn render_dashboard(app: &TuiApp) -> String {
    let mut output = String::new();
    let _ = writeln!(output, "skillctl terminal UI");
    let _ = writeln!(output, "workspace: {}", app.workspace);
    let _ = writeln!(
        output,
        "scope filter: {}",
        app.scope_filter.map_or("all", ManagedScope::as_str)
    );
    let _ = writeln!(
        output,
        "target filters: {}",
        if app.target_filters.is_empty() {
            "all".to_string()
        } else {
            app.target_filters.join(", ")
        }
    );
    if let Some(skill) = &app.selected_skill {
        let _ = writeln!(output, "selected skill: {skill}");
    }
    let _ = writeln!(output);

    if app.skills.is_empty() {
        let _ = writeln!(output, "No managed skills are installed.");
        let _ = writeln!(output);
        let _ = writeln!(output, "Suggested actions:");
        let _ = writeln!(output, "  skillctl install <source>");
    } else {
        let _ = writeln!(output, "Installed skills: {}", app.skills.len());
        let _ = writeln!(output);

        for skill in &app.skills {
            render_skill_card(&mut output, skill);
        }
    }

    render_history_panel(&mut output, &app.history);
    output
}

fn render_skill_card(output: &mut String, skill: &TuiSkillCard) {
    let _ = writeln!(output, "- {} [{}]", skill.skill, skill.scope.as_str());
    let _ = writeln!(
        output,
        "  installed: {} @ {}",
        source_label(skill.installed.source_kind),
        skill.installed.resolved_revision
    );
    let _ = writeln!(
        output,
        "  effective version: {}",
        skill.installed.effective_version_hash
    );
    let _ = writeln!(output, "  installed at: {}", skill.installed.installed_at);
    let _ = writeln!(output, "  updated at: {}", skill.installed.updated_at);
    match &skill.pin {
        Some(pin) => {
            let _ = writeln!(
                output,
                "  pin: {} -> {}",
                pin.requested_reference, pin.resolved_revision
            );
        }
        None => {
            let _ = writeln!(output, "  pin: none recorded");
        }
    }

    match &skill.update {
        Some(update) => {
            let mut line = format!(
                "  update: {} (checked {})",
                update.outcome.as_str(),
                update.checked_at
            );
            if let Some(latest_revision) = &update.latest_revision {
                let _ = write!(line, ", latest {}", latest_revision);
            }
            if update.overlay_detected {
                let _ = write!(line, ", overlay detected");
            }
            if update.local_modification_detected {
                let _ = write!(line, ", local modifications detected");
            }
            let _ = writeln!(output, "{line}");
            if let Some(notes) = &update.notes {
                let _ = writeln!(output, "  update notes: {notes}");
            }
        }
        None => {
            let _ = writeln!(output, "  update: no recorded check");
        }
    }

    if skill.overlay.present {
        let _ = writeln!(
            output,
            "  overlay: {}",
            skill.overlay.path.as_deref().unwrap_or("present")
        );
    } else {
        let _ = writeln!(output, "  overlay: none");
    }

    if skill.local_modifications.is_empty() {
        let _ = writeln!(output, "  local modifications: none");
    } else {
        let _ = writeln!(
            output,
            "  local modifications: {}",
            skill.local_modifications.len()
        );
        for modification in &skill.local_modifications {
            let _ = writeln!(
                output,
                "    {} {}{}",
                modification.detected_at,
                modification.kind.as_str(),
                modification
                    .path
                    .as_ref()
                    .map_or(String::new(), |path| format!(" at {path}"))
            );
        }
    }

    let _ = writeln!(
        output,
        "  visibility: {}",
        explain_status_label(skill.visibility.status)
    );
    for target in &skill.visibility.targets {
        let _ = writeln!(
            output,
            "    {}: {} at {}",
            target.target.as_str(),
            if target.visible {
                "visible"
            } else {
                "not visible"
            },
            target.path
        );
    }
    for issue in &skill.visibility.issues {
        let _ = writeln!(output, "    {}: {}", issue.severity_label(), issue.message);
    }

    if skill.rollback.count == 0 {
        let _ = writeln!(output, "  rollback: none recorded");
    } else if let Some(latest) = &skill.rollback.latest {
        let _ = writeln!(
            output,
            "  rollback: {} recorded, latest {} -> {} at {}",
            skill.rollback.count, latest.from_reference, latest.to_reference, latest.rolled_back_at
        );
    }

    let _ = writeln!(output, "  actions:");
    let _ = writeln!(output, "    {}", skill.actions.update);
    let _ = writeln!(output, "    {}", skill.actions.explain);
    let _ = writeln!(output, "    {}", skill.actions.path);
    let _ = writeln!(output, "    {}", skill.actions.history);
    let _ = writeln!(output, "    {}", skill.actions.pin);
    let _ = writeln!(output, "    {}", skill.actions.rollback);
    let _ = writeln!(output, "    {}", skill.actions.sync);

    if !skill.history_preview.is_empty() {
        let _ = writeln!(output, "  recent history:");
        for entry in &skill.history_preview {
            let _ = writeln!(
                output,
                "    {} {}: {}",
                entry.occurred_at,
                entry.kind.as_str(),
                entry.summary
            );
        }
    }

    let _ = writeln!(output);
}

fn render_history_panel(output: &mut String, history: &TuiHistoryPanel) {
    let _ = writeln!(output, "Recent history");
    if let Some(skill) = &history.skill {
        let _ = writeln!(
            output,
            "skill: {}{}",
            skill,
            history
                .scope
                .map(|scope| format!(" [{}]", scope.as_str()))
                .unwrap_or_default()
        );
    }

    if history.entries.is_empty() {
        let _ = writeln!(output, "  no history recorded");
        return;
    }

    for entry in &history.entries {
        let _ = writeln!(
            output,
            "  {} {}: {}",
            entry.occurred_at,
            entry.kind.as_str(),
            entry.summary
        );
    }
}

fn explain_status_label(status: ExplainStatus) -> &'static str {
    match status {
        ExplainStatus::Selected => "selected",
        ExplainStatus::Conflict => "conflict",
        ExplainStatus::Missing => "missing",
    }
}

fn source_label(kind: SourceKind) -> &'static str {
    match kind {
        SourceKind::Git => "git",
        SourceKind::LocalPath => "local-path",
        SourceKind::Archive => "archive",
    }
}

trait SeverityLabel {
    fn severity_label(&self) -> &'static str;
}

impl SeverityLabel for crate::doctor::DiagnosticIssue {
    fn severity_label(&self) -> &'static str {
        match self.severity {
            crate::doctor::DiagnosticSeverity::Error => "error",
            crate::doctor::DiagnosticSeverity::Warning => "warning",
        }
    }
}
