//! Local review decisions for managed imported skills.
//!
//! An import starts `imported-unreviewed`. Recording a review is the local,
//! human decision that moves it to `imported-reviewed`, which is the state the
//! trust gate needs before it lets an update apply move a script-bearing
//! import. The decision is stored in the local state store with the content
//! digest it covers, so it stops counting as soon as the content changes.

use serde_json::json;

use crate::{
    app::AppContext,
    error::AppError,
    history::HistoryLedger,
    response::AppResponse,
    skill::resolve_installed_skill,
    source::{current_timestamp, stored_import_root},
    state::{InstallRecord, LocalStateStore, ManagedSkillRef, ReviewDecision, TrustReviewRecord},
    trust::{self, ReviewIndex, SkillTrust, TrustState},
};

/// Request for `skillctl review`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewRequest {
    /// Managed skill name.
    pub skill: String,
    /// Decision to record, or `None` to report the current state.
    pub decision: Option<ReviewDecision>,
    /// Optional reviewer note stored with the decision.
    pub note: Option<String>,
}

impl ReviewRequest {
    /// Create a request that only reports the current review state.
    pub fn new(skill: String) -> Self {
        Self {
            skill,
            decision: None,
            note: None,
        }
    }

    /// Record a decision instead of only reporting the state.
    pub fn with_decision(mut self, decision: ReviewDecision, note: Option<String>) -> Self {
        self.decision = Some(decision);
        self.note = note;
        self
    }
}

/// Handle `skillctl review`.
pub fn handle_review(
    context: &AppContext,
    request: ReviewRequest,
) -> Result<AppResponse, AppError> {
    let store = LocalStateStore::open_default_for(&context.working_directory)?;
    let managed_skill = resolve_installed_skill(&store, &request.skill, context.selector.scope)?;
    let install =
        store
            .install_record(&managed_skill)?
            .ok_or_else(|| AppError::ResolutionValidation {
                message: format!(
                    "skill '{}' does not have an installed state record",
                    request.skill
                ),
            })?;
    ensure_review_supported(&install)?;

    match request.decision {
        None => review_status(context, &store, &install),
        Some(decision) => record_decision(context, &managed_skill, decision, request.note),
    }
}

/// Refuse skills that carry no imported content to review.
fn ensure_review_supported(install: &InstallRecord) -> Result<(), AppError> {
    if install.source_url.starts_with("builtin://") {
        return Err(AppError::ResolutionValidation {
            message: format!(
                "skill '{}' is bundled content that skillctl ships, so it carries no review \
                 decision",
                install.skill.skill_id
            ),
        });
    }
    if install.detached || install.forked {
        return Err(AppError::ResolutionValidation {
            message: format!(
                "skill '{}' is locally owned after a fork or detach, so it is trusted without a \
                 review",
                install.skill.skill_id
            ),
        });
    }

    Ok(())
}

/// Effective content root of one import, used for script detection.
fn effective_root(
    context: &AppContext,
    install: &InstallRecord,
) -> Result<std::path::PathBuf, AppError> {
    Ok(stored_import_root(
        install.skill.scope,
        &context.working_directory,
        &install.skill.skill_id,
    )?
    .join(&install.source_subpath))
}

/// Trust decision for the recorded install, including the recorded review.
fn install_trust(
    context: &AppContext,
    store: &LocalStateStore,
    install: &InstallRecord,
) -> Result<SkillTrust, AppError> {
    let reviews = ReviewIndex::load(store)?;
    trust::trust_for_install_record(
        install,
        Some(&effective_root(context, install)?),
        reviews.approved_for_install(install),
    )
}

/// Report the current review state without changing anything.
fn review_status(
    context: &AppContext,
    store: &LocalStateStore,
    install: &InstallRecord,
) -> Result<AppResponse, AppError> {
    let recorded = store.trust_review(&install.skill)?;
    let covers_current = recorded
        .as_ref()
        .is_some_and(|record| record.effective_version_hash == install.effective_version_hash);
    let trust = install_trust(context, store, install)?;
    let skill = install.skill.skill_id.as_str();

    let summary = match trust.effective_state {
        TrustState::ImportedReviewed => format!(
            "{skill} in {} scope is imported-reviewed for revision {}.",
            install.skill.scope.as_str(),
            install.resolved_revision
        ),
        TrustState::ImportedUnreviewed => {
            let risk = if trust.contains_scripts {
                "elevated risk"
            } else {
                "normal risk"
            };
            // A decision on record that no longer counts is either revoked or an
            // approval of an earlier effective version. Only the second case is
            // stale coverage, so the explanation follows the recorded decision
            // and never claims a version the record does cover.
            let stale = match recorded.as_ref().map(|record| record.decision) {
                Some(ReviewDecision::Revoked) => {
                    "the recorded review was revoked, so record a fresh one"
                }
                Some(ReviewDecision::Approved) => {
                    "the recorded review covers an earlier version, so record a fresh one"
                }
                None => "record a review",
            };
            format!(
                "{skill} in {} scope is imported-unreviewed with {risk}; {stale} with 'skillctl \
                 review {skill} --approve'.",
                install.skill.scope.as_str()
            )
        }
        // `ensure_review_supported` refuses the builtin, detached, and forked
        // installs that `trust_for_install_record` marks locally trusted, so
        // that state cannot reach the report. Re-run the guard instead of
        // duplicating its refusal, keeping one source of truth if the two ever
        // disagree.
        TrustState::LocalTrusted => {
            ensure_review_supported(install)?;
            unreachable!("the review guard accepted a locally trusted install");
        }
    };

    let mut response = AppResponse::success("review")
        .with_summary(summary)
        .with_data(json!({
            "skill": skill,
            "scope": install.skill.scope.as_str(),
            "resolved_revision": install.resolved_revision,
            "effective_version_hash": install.effective_version_hash,
            "trust": trust,
            "review": recorded,
            "review_covers_current_content": covers_current,
        }));
    for warning in &trust.warnings {
        response = response.with_warning(warning.clone());
    }

    Ok(response)
}

/// Record one review decision through the guarded engine.
///
/// A review changes local state only, so the plan declares no filesystem
/// target: the engine still verifies the import pre-state, snapshots the state
/// database, and rolls the decision back when anything fails.
fn record_decision(
    context: &AppContext,
    managed_skill: &ManagedSkillRef,
    decision: ReviewDecision,
    note: Option<String>,
) -> Result<AppResponse, AppError> {
    let store = LocalStateStore::open_default_for(&context.working_directory)?;
    let install =
        store
            .install_record(managed_skill)?
            .ok_or_else(|| AppError::ResolutionValidation {
                message: format!(
                    "skill '{}' does not have an installed state record",
                    managed_skill.skill_id
                ),
            })?;
    ensure_review_supported(&install)?;
    let expected = crate::apply::capture_expected_import(context, &install)?;

    let plan = crate::apply::MutationPlanBuilder::new(
        crate::apply::MutationOperation::Review,
        &context.working_directory,
    )
    .skill(managed_skill.clone())
    .expect_import(expected.clone())
    .propose(crate::apply::ProposedImport {
        import_id: managed_skill.skill_id.clone(),
        revision: None,
        bundle_digest: expected.bundle_digest,
        bundle_policy: crate::apply::BundlePolicy::Preserve,
        summary: format!(
            "record a {} review of '{}' without touching its content",
            decision.as_str(),
            managed_skill.skill_id
        ),
    })
    .seal();

    crate::apply::run_guarded(&plan, context, |transaction| {
        transaction.track_state_database()?;
        let mut store = LocalStateStore::open_default_for(&context.working_directory)?;
        let install =
            store
                .install_record(managed_skill)?
                .ok_or_else(|| AppError::ResolutionValidation {
                    message: format!(
                        "skill '{}' does not have an installed state record",
                        managed_skill.skill_id
                    ),
                })?;
        ensure_review_supported(&install)?;

        let contains_scripts =
            trust::directory_contains_scripts(&effective_root(context, &install)?)?;
        let record = TrustReviewRecord {
            skill: install.skill.clone(),
            decision,
            resolved_revision: install.resolved_revision.clone(),
            effective_version_hash: install.effective_version_hash,
            contains_scripts,
            reviewed_at: current_timestamp(),
            note: note.clone(),
        };

        let mut ledger = HistoryLedger::new(&mut store);
        ledger.record_trust_review(&record)?;

        Ok(AppResponse::success("review")
            .with_summary(decision_summary(&record))
            .with_data(json!({
                "skill": record.skill.skill_id,
                "scope": record.skill.scope.as_str(),
                "resolved_revision": record.resolved_revision,
                "effective_version_hash": record.effective_version_hash,
                "review": record,
            }))
            .with_detail_lines(decision_detail(&record)))
    })
}

/// Human detail line that states how long one recorded decision stays valid.
fn decision_detail(record: &TrustReviewRecord) -> Vec<String> {
    match record.decision {
        ReviewDecision::Approved => vec![format!(
            "  the approval covers {} only; an applied update returns the skill to \
             imported-unreviewed",
            record.effective_version_hash
        )],
        ReviewDecision::Revoked => vec![format!(
            "  {} is imported-unreviewed again, so the trust gate holds back update applies",
            record.skill.skill_id
        )],
    }
}

/// Human summary for one recorded decision.
fn decision_summary(record: &TrustReviewRecord) -> String {
    match record.decision {
        ReviewDecision::Approved => format!(
            "Recorded a local review of {} at {}.",
            record.skill.skill_id, record.resolved_revision
        ),
        ReviewDecision::Revoked => {
            format!("Revoked the recorded review of {}.", record.skill.skill_id)
        }
    }
}
