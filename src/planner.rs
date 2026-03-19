//! Planning domain types and update entry points.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::{
    adapter::{AdapterRegistry, TargetRuntime, TargetScope},
    app::AppContext,
    error::AppError,
    manifest::{AdapterOverride, AdapterRoot, ProjectionPolicy},
    response::AppResponse,
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
    _context: &AppContext,
    _request: UpdateRequest,
) -> Result<AppResponse, AppError> {
    Err(AppError::NotYetImplemented { command: "update" })
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
