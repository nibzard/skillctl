//! Trust-state modeling for imported and local skills.

use std::{collections::BTreeMap, fs, io, path::Path};

use serde::Serialize;
use url::Url;

use crate::{
    error::AppError,
    resolver::{ResolvedSkillCandidate, SkillScope},
    state::{
        InstallRecord, LocalStateStore, ManagedScope, ManagedSkillRef, ReviewDecision,
        TrustReviewRecord, UpdateCheckBlockReason, UpdateCheckOutcome,
    },
};

/// Stable trust states for managed and effective skills.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum TrustState {
    /// Canonical local ownership or bundled content.
    LocalTrusted,
    /// Imported content that has not been reviewed yet.
    ImportedUnreviewed,
    /// Imported content that has been explicitly reviewed.
    ImportedReviewed,
}

/// User-visible risk level derived from one trust decision.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum TrustRiskLevel {
    /// No elevated trust warning applies.
    Normal,
    /// Imported script-bearing content remains unreviewed.
    Elevated,
}

/// Operation currently blocked by a trust gate.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum TrustBlockedAction {
    /// Applying an upstream update is blocked until review or fork.
    ApplyUpdate,
}

/// Structured trust decision surfaced in JSON and local history.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SkillTrust {
    /// Trust state of the stored source.
    pub source_state: TrustState,
    /// Trust state of the currently effective skill.
    pub effective_state: TrustState,
    /// Derived risk level.
    pub risk_level: TrustRiskLevel,
    /// Whether the effective skill includes a top-level `scripts/` tree.
    pub contains_scripts: bool,
    /// Whether a human review is still required.
    pub review_required: bool,
    /// Recorded review decision that covers the effective content.
    ///
    /// Absent while the content is unreviewed, so the field doubles as the
    /// evidence that produced `imported-reviewed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<TrustReviewRecord>,
    /// Current operation-level trust-gate blocks.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocked_actions: Vec<TrustBlockedAction>,
    /// Deterministic warnings relevant to the current operation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

impl SkillTrust {
    /// Build trust for canonical local or bundled content.
    pub fn local(contains_scripts: bool) -> Self {
        Self {
            source_state: TrustState::LocalTrusted,
            effective_state: TrustState::LocalTrusted,
            risk_level: TrustRiskLevel::Normal,
            contains_scripts,
            review_required: false,
            review: None,
            blocked_actions: Vec::new(),
            warnings: Vec::new(),
        }
    }

    /// Build trust for an imported skill no recorded approval covers.
    ///
    /// Install candidates carry this state, because a review decision can only
    /// exist once the import has an identity in the local state store.
    pub fn imported_unreviewed(skill_name: &str, contains_scripts: bool) -> Self {
        let mut warnings = Vec::new();
        let risk_level = if contains_scripts {
            warnings.push(format!(
                "imported skill '{}' contains scripts and remains unreviewed",
                skill_name
            ));
            TrustRiskLevel::Elevated
        } else {
            TrustRiskLevel::Normal
        };

        Self {
            source_state: TrustState::ImportedUnreviewed,
            effective_state: TrustState::ImportedUnreviewed,
            risk_level,
            contains_scripts,
            review_required: true,
            review: None,
            blocked_actions: Vec::new(),
            warnings,
        }
    }

    /// Build trust for imported content a recorded review covers.
    ///
    /// The review is the evidence, so its digest and timestamp travel with the
    /// decision. Reviewed content raises no warning at all, because the
    /// elevated messaging holds only until a review exists; `contains_scripts`
    /// and `review` keep reporting the facts the decision covered.
    pub fn imported_reviewed(contains_scripts: bool, review: &TrustReviewRecord) -> Self {
        Self {
            source_state: TrustState::ImportedReviewed,
            effective_state: TrustState::ImportedReviewed,
            risk_level: TrustRiskLevel::Normal,
            contains_scripts,
            review_required: false,
            review: Some(review.clone()),
            blocked_actions: Vec::new(),
            warnings: Vec::new(),
        }
    }

    /// Build trust for an imported skill from its recorded review decision.
    ///
    /// Only an approval of the content the caller resolved counts as reviewed.
    /// Any other decision, or no decision at all, keeps the import
    /// `imported-unreviewed`.
    pub fn imported(
        skill_name: &str,
        contains_scripts: bool,
        review: Option<&TrustReviewRecord>,
    ) -> Self {
        match review {
            Some(review) if review.decision == ReviewDecision::Approved => {
                Self::imported_reviewed(contains_scripts, review)
            }
            _ => Self::imported_unreviewed(skill_name, contains_scripts),
        }
    }

    /// Return whether the trust gate blocks one operation.
    ///
    /// Callers that mutate installed content must refuse when this returns
    /// `true` for the operation they are about to perform.
    pub fn blocks(&self, action: TrustBlockedAction) -> bool {
        self.blocked_actions.contains(&action)
    }

    /// Add a trust-gate block for update application when risk is elevated.
    pub fn block_apply_update(mut self, skill_name: &str) -> Self {
        if self.risk_level == TrustRiskLevel::Elevated {
            self.blocked_actions.push(TrustBlockedAction::ApplyUpdate);
            self.warnings.push(format!(
                "trust gate is blocking update apply for '{}' until it is reviewed or forked; \
                 record a local review with 'skillctl review {} --approve'",
                skill_name, skill_name
            ));
        }
        self
    }

    /// Hold back an update apply that would install executable content.
    ///
    /// [`Self::block_apply_update`] decides from the installed content, which
    /// is the only content a recorded review can cover. A revision that adds a
    /// top-level `scripts/` directory brings executable content no recorded
    /// review covers, so an unreviewed import may not move to it. A recorded
    /// approval of the installed content licenses the move, and the apply then
    /// returns the import to `imported-unreviewed`, where `doctor` reports the
    /// scripts again.
    pub fn block_apply_update_for_incoming_scripts(
        mut self,
        skill_name: &str,
        revision: &str,
    ) -> Self {
        self.blocked_actions.push(TrustBlockedAction::ApplyUpdate);
        self.warnings.push(format!(
            "trust gate is blocking update apply for '{skill_name}' until it is reviewed or \
             forked; the incoming revision {revision} adds a top-level scripts/ directory no \
             recorded review covers; record a local review with 'skillctl review {skill_name} \
             --approve'",
        ));
        self
    }

    /// Whether the trust gate refuses an apply that moves this skill upstream.
    ///
    /// The decision is taken through [`Self::block_apply_update`], the same
    /// method the guarded apply engine consults, so advice built on this
    /// predicate cannot name a command the engine refuses.
    pub fn gates_update_apply(&self, skill_name: &str) -> bool {
        self.clone()
            .block_apply_update(skill_name)
            .blocks(TrustBlockedAction::ApplyUpdate)
    }
}

/// Recorded review decisions for the skills one command can see.
///
/// A review covers exactly one effective version hash. An import that moved to
/// different content finds no approval here, so it is unreviewed again until
/// someone records a fresh decision.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ReviewIndex(BTreeMap<(ManagedScope, String), TrustReviewRecord>);

impl ReviewIndex {
    /// Load every review decision the store exposes to this workspace.
    pub fn load(store: &LocalStateStore) -> Result<Self, AppError> {
        Ok(Self(
            store
                .trust_reviews()?
                .into_iter()
                .map(|record| ((record.skill.scope, record.skill.skill_id.clone()), record))
                .collect(),
        ))
    }

    /// The approval that covers one effective version hash, when one exists.
    pub fn approved(
        &self,
        skill: &ManagedSkillRef,
        effective_version_hash: &str,
    ) -> Option<&TrustReviewRecord> {
        self.0
            .get(&(skill.scope, skill.skill_id.clone()))
            .filter(|record| record.decision == ReviewDecision::Approved)
            .filter(|record| record.effective_version_hash == effective_version_hash)
    }

    /// The approval that covers one resolved import candidate.
    pub fn approved_for_candidate(
        &self,
        candidate: &ResolvedSkillCandidate,
    ) -> Option<&TrustReviewRecord> {
        let import = candidate.import.as_ref()?;
        self.approved(
            &ManagedSkillRef::new(
                managed_scope_from_skill(candidate.scope),
                import.id.as_str(),
            ),
            import.effective_version_hash.as_str(),
        )
    }

    /// The approval that covers one recorded install.
    pub fn approved_for_install(&self, install: &InstallRecord) -> Option<&TrustReviewRecord> {
        self.approved(&install.skill, install.effective_version_hash.as_str())
    }
}

/// Origin-trust policy for remote discovery sources.
///
/// The policy decides what `skillctl` may do with a discovery origin before it
/// trusts the index the origin publishes. It covers acquisition, whose safety
/// depends on the transport, and telemetry, whose visibility depends on the
/// publisher.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DiscoveryOriginTrust {
    /// A loopback origin, which serves a local index.
    Loopback,
    /// A public HTTPS publisher this policy verified.
    VerifiedPublicHttps,
    /// A remote HTTPS origin this policy has not verified.
    ///
    /// The declared digests are still verified against the fetched bytes, so
    /// acquisition proceeds, but nothing proves the publisher is public.
    UnverifiedRemote,
    /// An origin whose transport cannot carry a trustworthy declaration.
    ///
    /// Over plain HTTP off loopback, an attacker on the network can serve an
    /// index whose artifacts hash to the digests the attacker declared, so the
    /// digest pin protects nothing.
    InsecureTransport,
}

/// Discovery publishers verified to publish a public index.
///
/// Each host was verified by hand against the Agent Skills Discovery RFC, the
/// way [`crate::telemetry`] verifies public Git repositories. See
/// `notes/agent-skills-discovery-rfc.md` for the research dates.
const VERIFIED_PUBLIC_DISCOVERY_ORIGINS: &[&str] = &["www.russ.cloud"];

impl DiscoveryOriginTrust {
    /// Whether an index and its artifacts may be acquired from this origin.
    pub const fn permits_acquisition(self) -> bool {
        !matches!(self, Self::InsecureTransport)
    }

    /// Stable kebab-case name of this classification, matching the serialized
    /// form reports carry.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Loopback => "loopback",
            Self::VerifiedPublicHttps => "verified-public-https",
            Self::UnverifiedRemote => "unverified-remote",
            Self::InsecureTransport => "insecure-transport",
        }
    }
}

/// Evaluate the origin-trust policy for one discovery index URL.
pub fn discovery_origin_trust(index_url: &str) -> DiscoveryOriginTrust {
    let Ok(parsed) = Url::parse(index_url) else {
        return DiscoveryOriginTrust::InsecureTransport;
    };

    // Credentials name a specific private deployment, so the origin is remote
    // and unverified even when the transport itself is TLS.
    let credentialed = !parsed.username().is_empty() || parsed.password().is_some();
    let Some(host) = parsed.host_str() else {
        return DiscoveryOriginTrust::InsecureTransport;
    };

    if is_loopback_host(host) {
        return DiscoveryOriginTrust::Loopback;
    }

    match parsed.scheme() {
        "https" => {
            let normalized_host = host.to_ascii_lowercase();
            if !credentialed
                && VERIFIED_PUBLIC_DISCOVERY_ORIGINS.contains(&normalized_host.as_str())
            {
                DiscoveryOriginTrust::VerifiedPublicHttps
            } else {
                DiscoveryOriginTrust::UnverifiedRemote
            }
        }
        _ => DiscoveryOriginTrust::InsecureTransport,
    }
}

/// Refuse to acquire a discovery index from an origin the policy blocks.
///
/// Returns the decision so a caller that acquires anyway can warn about an
/// origin the policy did not verify.
pub fn ensure_discovery_origin_permits_acquisition(
    index_url: &str,
) -> Result<DiscoveryOriginTrust, AppError> {
    let trust = discovery_origin_trust(index_url);
    if !trust.permits_acquisition() {
        return Err(AppError::DiscoveryValidation {
            input: index_url.to_string(),
            message: "the origin trust policy refuses this discovery origin: only HTTPS origins \
                      and loopback origins may be read, because over plain HTTP an attacker on \
                      the network can serve an index whose artifacts match its declared digests; \
                      use an HTTPS origin, or a loopback origin for a local index"
                .to_string(),
        });
    }

    Ok(trust)
}

/// Warning acquisition from a discovery origin emits, when one applies.
pub fn discovery_origin_warning(trust: DiscoveryOriginTrust, index_url: &str) -> Option<String> {
    match trust {
        DiscoveryOriginTrust::UnverifiedRemote => Some(format!(
            "the discovery origin '{index_url}' is not on the verified public publisher list, so \
             its telemetry events stay suppressed; the declared digests are still verified \
             against the fetched bytes"
        )),
        DiscoveryOriginTrust::Loopback | DiscoveryOriginTrust::VerifiedPublicHttps => None,
        // The acquisition is refused before a warning could be shown.
        DiscoveryOriginTrust::InsecureTransport => None,
    }
}

/// Whether a host names this machine, which serves a local index.
pub(crate) fn is_loopback_host(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "::1" | "[::1]")
}

/// Build trust for one resolved effective-skill candidate.
pub fn trust_for_candidate(
    candidate: &ResolvedSkillCandidate,
    review: Option<&TrustReviewRecord>,
) -> SkillTrust {
    let contains_scripts = candidate_contains_scripts(candidate);
    if candidate.import.is_some() {
        SkillTrust::imported(candidate.skill.name.as_str(), contains_scripts, review)
    } else {
        SkillTrust::local(contains_scripts)
    }
}

/// Whether an upstream revision move is still waiting for one import.
///
/// The gate holds back applies that move an import upstream, while the repair
/// strategies keep the pinned revision, so advice that names a repair command
/// needs to know whether a move is pending. A diagnostic cannot ask the source,
/// so the answer comes from the local state alone. See
/// [`revision_move_state`] for what that state can prove.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RevisionMove {
    /// A recorded check observed a revision the pin does not hold yet.
    Pending,
    /// No revision move is pending.
    NotPending,
    /// The local state cannot tell.
    Unknown,
}

/// Whether an upstream revision move is still waiting for one import.
///
/// Two local facts settle the question. A reference that names an immutable
/// revision — the commit or effective version a `pin` or a `rollback` records —
/// names no upstream ref a source could move on from, so no move can wait
/// behind it. Otherwise the newest recorded update check answers, but only
/// while it still describes the current pin: a check that predates the pinned
/// revision says nothing about the revisions after it.
///
/// A well-known import records its verified digest as the requested reference,
/// because the digest is the only revision the source names. That reference is
/// immutable by construction and still leaves a move waiting — the next index
/// entry the check re-reads can declare another digest — so the shortcut above
/// covers Git imports only and the recorded check answers for well-known ones.
///
/// Every other state — no recorded check, a check that could not observe
/// upstream, or a check for an earlier pin — is unknown. A caller that must not
/// name a command the gate may refuse treats unknown as a move that may be
/// waiting.
pub fn revision_move_state(
    store: &LocalStateStore,
    install: &InstallRecord,
    candidate: Option<&ResolvedSkillCandidate>,
) -> Result<RevisionMove, AppError> {
    let pin = store.pin_record(&install.skill)?;
    let reference =
        crate::planner::active_requested_git_reference(install, pin.as_ref(), candidate);
    if install.source_kind == crate::source::SourceKind::Git
        && reference.is_some_and(|reference| {
            crate::source::reference_pins_immutable_revision(reference, &install.resolved_revision)
        })
    {
        return Ok(RevisionMove::NotPending);
    }

    let Some(check) = store.latest_update_check(&install.skill)? else {
        return Ok(RevisionMove::Unknown);
    };
    if check.pinned_revision != install.resolved_revision {
        return Ok(RevisionMove::Unknown);
    }

    Ok(match check.outcome {
        UpdateCheckOutcome::UpToDate | UpdateCheckOutcome::CommitPinned => RevisionMove::NotPending,
        UpdateCheckOutcome::UpdateAvailable | UpdateCheckOutcome::Blocked => RevisionMove::Pending,
        UpdateCheckOutcome::Detached
        | UpdateCheckOutcome::Failed
        | UpdateCheckOutcome::LocalSource => RevisionMove::Unknown,
    })
}

/// Whether the trust gate refuses an update apply for one managed skill.
///
/// Reports that name a repair command use this predicate to avoid recommending
/// `update --apply` strategies the gate refuses. Only an unreviewed import that
/// tracks an upstream revision can be gated: the `overlay` and `discard`
/// repairs move such an import to a new revision inside one apply when one
/// waits. A Git import tracks a branch ref and a well-known import tracks the
/// digest its index declares, so both can wait on upstream. A local-path or
/// archive import has no revision to move, and a detached, forked, or bundled
/// skill is locally owned, so none of them is gated.
///
/// The gate holds back only applies that move an import upstream, so a gated
/// import whose local state shows no waiting revision is not reported here: its
/// `overlay` and `discard` repairs keep the pinned revision and the apply engine
/// lets them through. See [`revision_move_state`] for the evidence that decides
/// it, and for the states that stay unknown and therefore conservative.
///
/// A script-free import is a separate case, because the revision a repair would
/// move it to can introduce the executable content the installed content never
/// had. The newest recorded update check is the only local witness of that
/// tree, so the block reason the check recorded decides. Without such a check
/// the import is not gated, because its repair keeps the pinned revision and no
/// local evidence says otherwise. A well-known check cannot look ahead into the
/// artifact it never downloads, so its recorded rows carry no incoming-scripts
/// reason and the apply engine's own gate on the staged tree stays the only
/// refusal for that content.
///
/// `candidate` is the resolved winner when the caller holds one, which is the
/// same input the update check builds its trust decision from. Without one, the
/// stored import stands in for the effective content.
pub fn update_apply_is_gated(
    store: &LocalStateStore,
    skill: &ManagedSkillRef,
    candidate: Option<&ResolvedSkillCandidate>,
    working_directory: &Path,
) -> Result<bool, AppError> {
    let Some(install) = store.install_record(skill)? else {
        return Ok(false);
    };
    if !matches!(
        install.source_kind,
        crate::source::SourceKind::Git | crate::source::SourceKind::WellKnown
    ) {
        return Ok(false);
    }

    let reviews = ReviewIndex::load(store)?;
    let trust = match candidate {
        Some(candidate) => {
            trust_for_candidate(candidate, reviews.approved_for_candidate(candidate))
        }
        None => {
            // The stored import holds the whole source, so the recorded subpath
            // selects the part that projects as the effective skill.
            let root =
                crate::source::stored_import_root(skill.scope, working_directory, &skill.skill_id)?
                    .join(&install.source_subpath);
            trust_for_install_record(
                &install,
                Some(&root),
                reviews.approved_for_install(&install),
            )?
        }
    };
    if trust.gates_update_apply(skill.skill_id.as_str()) {
        // The repair strategies keep the pinned revision, so the gate refuses
        // them only while a revision move is pending. An answer the local state
        // cannot give counts as pending, because the advice must not name a
        // command the gate may refuse.
        return Ok(revision_move_state(store, &install, candidate)? != RevisionMove::NotPending);
    }

    // The installed content is script-free, so the revision a repair would move
    // to is the only content the gate could refuse. A recorded approval
    // licenses that move, and a locally owned skill does not move at all, so
    // only an unreviewed import consults the recorded check.
    if trust.effective_state != TrustState::ImportedUnreviewed {
        return Ok(false);
    }
    // The same repair keeps the pinned revision while no move is pending, so an
    // immutable pin leaves the strategies open even when a check recorded
    // before the pin saw a gated revision.
    if revision_move_state(store, &install, candidate)? != RevisionMove::Pending {
        return Ok(false);
    }
    check_blocks_incoming_revision(store, &install)
}

/// Whether the newest recorded check saw a waiting revision the gate refuses to
/// move an unreviewed import to.
///
/// Only a check that still describes the current pin answers, and only while it
/// observed a revision to move to. The block reason it recorded then says which
/// tree held the executable content: `installed-scripts` restates what the
/// local state already proves, while `incoming-scripts` is the one fact no
/// later reader can rederive, because the revision it names is gone once the
/// check ends.
fn check_blocks_incoming_revision(
    store: &LocalStateStore,
    install: &InstallRecord,
) -> Result<bool, AppError> {
    let Some(check) = store.latest_update_check(&install.skill)? else {
        return Ok(false);
    };
    if check.pinned_revision != install.resolved_revision {
        return Ok(false);
    }

    Ok(matches!(
        (check.outcome, check.apply_blocked_by),
        (
            UpdateCheckOutcome::UpdateAvailable | UpdateCheckOutcome::Blocked,
            Some(UpdateCheckBlockReason::IncomingScripts),
        )
    ))
}

/// Which repair commands can resolve the unrecorded edits of one drifted copy.
///
/// [`crate::apply::DriftReason::repair`] names commands from this decision, so
/// it must never classify a skill `update` refuses. `update` resolves a skill by
/// name across every scope and exits with a validation error for a name no
/// scope records, which covers both a copy left behind by a removed import and
/// the generated copy of a canonical local skill. The trust gate is then applied
/// to the record that was found, which keeps the gate's own opinion of a
/// pending revision move: a gated import whose local state shows none gets the
/// update strategies, because the apply engine lets their repair through. A
/// script-free import is gated the same way when the newest recorded check saw
/// the waiting revision introduce `scripts/`, so see
/// [`update_apply_is_gated`] for both rules.
pub fn drift_repair_commands(
    store: &LocalStateStore,
    skill: &ManagedSkillRef,
    candidate: Option<&ResolvedSkillCandidate>,
    working_directory: &Path,
) -> Result<crate::apply::RepairCommands, AppError> {
    let Some(scope) = install_record_scope(store, skill.skill_id.as_str())? else {
        return Ok(crate::apply::RepairCommands::DiscardOnly);
    };
    let skill = ManagedSkillRef::new(scope, skill.skill_id.clone());
    if update_apply_is_gated(store, &skill, candidate, working_directory)? {
        return Ok(crate::apply::RepairCommands::GatedUpdateStrategies);
    }
    Ok(crate::apply::RepairCommands::UpdateStrategies)
}

/// The scope that holds an install record for one skill name, when any does.
///
/// This mirrors the name resolution `update` performs, so a caller that builds
/// advice from it cannot name an update command that refuses to run.
pub fn install_record_scope(
    store: &LocalStateStore,
    skill_id: &str,
) -> Result<Option<ManagedScope>, AppError> {
    for scope in [ManagedScope::Workspace, ManagedScope::User] {
        if store
            .install_record(&ManagedSkillRef::new(scope, skill_id.to_string()))?
            .is_some()
        {
            return Ok(Some(scope));
        }
    }
    Ok(None)
}

/// Whether one upstream revision holds a top-level `scripts/` directory.
///
/// The answer covers the subtree the recorded subpath selects, which is the
/// subtree an apply would install. A source that cannot be checked out answers
/// `false`, because a check must keep reporting the revision it observed. The
/// apply engine re-derives the gate from the tree it checks out, so a check
/// that could not look ahead defers the refusal to the apply instead of
/// dropping it.
pub fn upstream_revision_contains_scripts(source_url: &str, revision: &str, subpath: &str) -> bool {
    let Ok(checked_out) = crate::history::checkout_git_revision(source_url, revision) else {
        return false;
    };
    directory_contains_scripts(&checked_out.root.join(subpath)).unwrap_or(false)
}

/// Build trust for one revision an update apply would install.
///
/// The decision the update check records describes the content the workspace
/// already holds, and a plan document rebuilds that decision from the install
/// state alone. An apply that moves an import upstream joins two trees
/// instead: the stored import, which a recorded review can cover, and the
/// checked-out revision, which nothing can cover yet. Either tree holding a
/// top-level `scripts/` directory leaves an unreviewed import with executable
/// content, so the gate holds the apply back.
///
/// `incoming_root` is the staged source root of the revision the apply moves
/// to. Without one the decision covers the stored import only.
pub fn trust_for_revision_apply(
    working_directory: &Path,
    install: &InstallRecord,
    subpath: &str,
    incoming_root: Option<&Path>,
    review: Option<&TrustReviewRecord>,
) -> Result<SkillTrust, AppError> {
    // The stored import holds the whole source, so the recorded subpath
    // selects the part that projects as the effective skill.
    let stored_root = crate::source::stored_import_root(
        install.skill.scope,
        working_directory,
        &install.skill.skill_id,
    )?
    .join(subpath);
    let contains_scripts = directory_contains_scripts(&stored_root)?;
    let contains_scripts = if contains_scripts {
        contains_scripts
    } else {
        match incoming_root {
            Some(root) => directory_contains_scripts(&root.join(subpath))?,
            None => false,
        }
    };

    if install.source_url.starts_with("builtin://") || install.detached || install.forked {
        return Ok(SkillTrust::local(contains_scripts));
    }

    Ok(SkillTrust::imported(
        install.skill.skill_id.as_str(),
        contains_scripts,
        review,
    ))
}

/// Build trust for one current install record using a known effective file root.
///
/// The record itself says whether the skill is locally owned: a `builtin://`
/// source, a detached import, or a fork is local, so no caller needs to pass
/// that fact in.
pub fn trust_for_install_record(
    install: &InstallRecord,
    effective_root: Option<&Path>,
    review: Option<&TrustReviewRecord>,
) -> Result<SkillTrust, AppError> {
    let contains_scripts = match effective_root {
        Some(root) => directory_contains_scripts(root)?,
        None => false,
    };

    if install.source_url.starts_with("builtin://") || install.detached || install.forked {
        return Ok(SkillTrust::local(contains_scripts));
    }

    Ok(SkillTrust::imported(
        install.skill.skill_id.as_str(),
        contains_scripts,
        review,
    ))
}

/// Return whether a resolved candidate includes files under `scripts/`.
pub fn candidate_contains_scripts(candidate: &ResolvedSkillCandidate) -> bool {
    candidate
        .files
        .iter()
        .any(|file| path_is_scripts_path(&file.relative_path))
}

/// Return whether a relative path is inside the top-level `scripts/` directory.
pub fn path_is_scripts_path(path: &Path) -> bool {
    path.components()
        .next()
        .is_some_and(|component| component.as_os_str() == "scripts")
}

/// Return whether a skill root contains a top-level `scripts/` directory.
pub fn directory_contains_scripts(root: &Path) -> Result<bool, AppError> {
    let scripts_root = root.join("scripts");
    match fs::metadata(&scripts_root) {
        Ok(metadata) => Ok(metadata.is_dir()),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(AppError::FilesystemOperation {
            action: "inspect trust metadata",
            path: scripts_root,
            source,
        }),
    }
}

/// Map a resolved skill scope onto the managed scope that owns its state.
fn managed_scope_from_skill(scope: SkillScope) -> ManagedScope {
    match scope {
        SkillScope::Workspace => ManagedScope::Workspace,
        SkillScope::User => ManagedScope::User,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_discovery_origins_are_trusted_for_local_indexes() {
        for url in [
            "http://127.0.0.1:8799",
            "http://localhost/.well-known/agent-skills/index.json",
            "http://[::1]:9000/index.json",
            "https://127.0.0.1",
        ] {
            assert_eq!(
                discovery_origin_trust(url),
                DiscoveryOriginTrust::Loopback,
                "{url} should classify as a loopback origin",
            );
        }
    }

    #[test]
    fn verified_public_discovery_publishers_are_trusted_over_https() {
        let url = "https://www.russ.cloud/.well-known/agent-skills/index.json";
        let trust = discovery_origin_trust(url);

        assert_eq!(trust, DiscoveryOriginTrust::VerifiedPublicHttps);
        assert!(trust.permits_acquisition());
        assert_eq!(discovery_origin_warning(trust, url), None);
    }

    #[test]
    fn unverified_https_origins_permit_acquisition_with_a_warning() {
        let url = "https://skills.internal.example/index.json";
        let trust = discovery_origin_trust(url);

        assert_eq!(trust, DiscoveryOriginTrust::UnverifiedRemote);
        assert!(trust.permits_acquisition());
        let warning =
            discovery_origin_warning(trust, url).expect("an unverified origin warns on use");
        assert!(
            warning.contains("verified public publisher list"),
            "unexpected warning: {warning}",
        );
    }

    #[test]
    fn credentialed_discovery_origins_stay_unverified() {
        assert_eq!(
            discovery_origin_trust("https://token@www.russ.cloud/index.json"),
            DiscoveryOriginTrust::UnverifiedRemote,
            "credentials name a private deployment even on a verified host",
        );
    }

    #[test]
    fn plain_http_remote_origins_are_refused() {
        for url in [
            "http://skills.example.com",
            "http://192.0.2.10/.well-known/agent-skills/index.json",
            "ftp://skills.example.com/index.json",
            "not a url",
        ] {
            let trust = discovery_origin_trust(url);
            assert_eq!(trust, DiscoveryOriginTrust::InsecureTransport, "{url}");
            assert!(!trust.permits_acquisition());
            let error = ensure_discovery_origin_permits_acquisition(url)
                .expect_err("acquisition from the origin is refused");
            assert!(
                error
                    .to_string()
                    .contains("origin trust policy refuses this discovery origin"),
                "unexpected refusal: {error}",
            );
        }
    }

    #[test]
    fn approved_origins_pass_the_acquisition_gate() {
        for url in [
            "http://127.0.0.1:8799",
            "https://www.russ.cloud/.well-known/agent-skills/index.json",
            "https://skills.internal.example/index.json",
        ] {
            assert!(
                ensure_discovery_origin_permits_acquisition(url).is_ok(),
                "{url} should pass the gate",
            );
        }
    }

    fn approved_review(effective_version_hash: &str) -> TrustReviewRecord {
        TrustReviewRecord {
            skill: ManagedSkillRef::new(ManagedScope::Workspace, "delta"),
            decision: ReviewDecision::Approved,
            resolved_revision: "0123456789abcdef".to_string(),
            effective_version_hash: effective_version_hash.to_string(),
            contains_scripts: true,
            reviewed_at: "2026-11-06T10:00:00Z".to_string(),
            note: None,
        }
    }

    fn git_install(revision: &str) -> InstallRecord {
        InstallRecord {
            skill: ManagedSkillRef::new(ManagedScope::Workspace, "delta"),
            source_kind: crate::source::SourceKind::Git,
            source_url: "https://example.com/skills.git".to_string(),
            source_subpath: "skills/delta".to_string(),
            resolved_revision: revision.to_string(),
            upstream_revision: None,
            content_hash: "sha256:content".to_string(),
            overlay_hash: "sha256:overlay".to_string(),
            effective_version_hash: "sha256:effective".to_string(),
            installed_at: "2026-11-07T10:00:00Z".to_string(),
            updated_at: "2026-11-07T10:00:00Z".to_string(),
            detached: false,
            forked: false,
        }
    }

    fn recorded_check(
        install: &InstallRecord,
        outcome: UpdateCheckOutcome,
    ) -> crate::state::UpdateCheckRecord {
        crate::state::UpdateCheckRecord {
            id: None,
            skill: install.skill.clone(),
            checked_at: "2026-11-07T10:05:00Z".to_string(),
            pinned_revision: install.resolved_revision.clone(),
            latest_revision: None,
            outcome,
            overlay_detected: false,
            local_modification_detected: false,
            apply_blocked_by: None,
            notes: None,
        }
    }

    #[test]
    fn without_a_recorded_check_the_revision_move_stays_unknown() {
        let mut store = crate::state::LocalStateStore::open_in_memory().expect("store opens");
        let install = git_install("0123456789abcdef");
        store
            .upsert_install_record(&install)
            .expect("install records");

        // Nothing vouches for the pin, so advice that must not name a refused
        // command has to assume a revision may be waiting.
        assert_eq!(
            revision_move_state(&store, &install, None).expect("state reads"),
            RevisionMove::Unknown,
        );
    }

    #[test]
    fn a_check_for_the_current_pin_settles_the_revision_move() {
        let mut store = crate::state::LocalStateStore::open_in_memory().expect("store opens");
        let install = git_install("0123456789abcdef");
        store
            .upsert_install_record(&install)
            .expect("install records");

        for outcome in [
            UpdateCheckOutcome::UpToDate,
            UpdateCheckOutcome::CommitPinned,
        ] {
            store
                .record_update_check(&recorded_check(&install, outcome))
                .expect("check records");
            assert_eq!(
                revision_move_state(&store, &install, None).expect("state reads"),
                RevisionMove::NotPending,
                "a check that observed no move settles {outcome:?}",
            );
        }

        for outcome in [
            UpdateCheckOutcome::UpdateAvailable,
            UpdateCheckOutcome::Blocked,
        ] {
            store
                .record_update_check(&recorded_check(&install, outcome))
                .expect("check records");
            assert_eq!(
                revision_move_state(&store, &install, None).expect("state reads"),
                RevisionMove::Pending,
                "a check that observed a move settles {outcome:?}",
            );
        }
    }

    #[test]
    fn a_check_for_an_earlier_pin_no_longer_settles_the_revision_move() {
        let mut store = crate::state::LocalStateStore::open_in_memory().expect("store opens");
        let mut install = git_install("0123456789abcdef");
        store
            .upsert_install_record(&install)
            .expect("install records");
        store
            .record_update_check(&recorded_check(&install, UpdateCheckOutcome::UpToDate))
            .expect("check records");

        // The import moved after the check, so the observation describes a pin
        // the workspace no longer holds.
        install.resolved_revision = "1111111111111111".to_string();
        store
            .upsert_install_record(&install)
            .expect("install records");
        assert_eq!(
            revision_move_state(&store, &install, None).expect("state reads"),
            RevisionMove::Unknown,
        );
    }

    #[test]
    fn an_immutable_pin_settles_the_revision_move_without_a_check() {
        let mut store = crate::state::LocalStateStore::open_in_memory().expect("store opens");
        let install = git_install("0123456789abcdef");
        store
            .upsert_install_record(&install)
            .expect("install records");
        store
            .upsert_pin_record(&crate::state::PinRecord {
                skill: install.skill.clone(),
                requested_reference: install.resolved_revision.clone(),
                resolved_revision: install.resolved_revision.clone(),
                effective_version_hash: None,
                pinned_at: "2026-11-07T10:05:00Z".to_string(),
            })
            .expect("pin records");

        // The pin names the commit itself, which no upstream ref can move on
        // from, so the answer needs no recorded check.
        assert_eq!(
            revision_move_state(&store, &install, None).expect("state reads"),
            RevisionMove::NotPending,
        );
    }

    fn well_known_install(digest: &str) -> InstallRecord {
        InstallRecord {
            skill: ManagedSkillRef::new(ManagedScope::Workspace, "code-review"),
            source_kind: crate::source::SourceKind::WellKnown,
            source_url: "https://skills.example.com/.well-known/agent-skills/index.json"
                .to_string(),
            source_subpath: "skills/code-review".to_string(),
            resolved_revision: digest.to_string(),
            upstream_revision: None,
            content_hash: "sha256:content".to_string(),
            overlay_hash: "sha256:overlay".to_string(),
            effective_version_hash: "sha256:effective".to_string(),
            installed_at: "2026-11-07T10:00:00Z".to_string(),
            updated_at: "2026-11-07T10:00:00Z".to_string(),
            detached: false,
            forked: false,
        }
    }

    #[test]
    fn a_well_known_digest_pin_does_not_settle_the_revision_move() {
        let mut store = crate::state::LocalStateStore::open_in_memory().expect("store opens");
        let install = well_known_install("sha256:0123456789abcdef");
        store
            .upsert_install_record(&install)
            .expect("install records");

        // Every well-known import names its digest as the requested reference,
        // yet the next index entry can declare another one, so only a recorded
        // check settles the question.
        assert_eq!(
            revision_move_state(&store, &install, None).expect("state reads"),
            RevisionMove::Unknown,
            "a digest reference must not read as an immutable git pin",
        );

        let mut check = recorded_check(&install, UpdateCheckOutcome::UpToDate);
        check.latest_revision = Some(install.resolved_revision.clone());
        store.record_update_check(&check).expect("check records");
        assert_eq!(
            revision_move_state(&store, &install, None).expect("state reads"),
            RevisionMove::NotPending,
            "a recorded check still settles a well-known import",
        );
    }

    /// Advice for one script-free git import. The fixture writes no stored
    /// import, so the install record stands in for the effective content and
    /// the recorded check decides.
    fn script_free_advice(
        store: &crate::state::LocalStateStore,
        install: &InstallRecord,
    ) -> crate::apply::RepairCommands {
        let working_directory = std::env::temp_dir();
        drift_repair_commands(store, &install.skill, None, &working_directory)
            .expect("repair commands build")
    }

    #[test]
    fn a_check_that_saw_the_incoming_revision_add_scripts_gates_a_script_free_import() {
        let mut store = crate::state::LocalStateStore::open_in_memory().expect("store opens");
        let install = git_install("0123456789abcdef");
        store
            .upsert_install_record(&install)
            .expect("install records");

        // Without a check the repair keeps the pinned revision, so no local
        // evidence names a revision the gate could refuse.
        assert_eq!(
            script_free_advice(&store, &install),
            crate::apply::RepairCommands::UpdateStrategies,
            "an import no check observed keeps the update strategies",
        );

        let mut check = recorded_check(&install, UpdateCheckOutcome::Blocked);
        check.latest_revision = Some("def456".to_string());
        check.apply_blocked_by = Some(UpdateCheckBlockReason::IncomingScripts);
        store.record_update_check(&check).expect("check records");

        assert_eq!(
            script_free_advice(&store, &install),
            crate::apply::RepairCommands::GatedUpdateStrategies,
            "the repair would move the import to a revision the gate refuses",
        );
    }

    #[test]
    fn a_waiting_revision_the_check_did_not_flag_keeps_the_update_strategies() {
        let mut store = crate::state::LocalStateStore::open_in_memory().expect("store opens");
        let install = git_install("0123456789abcdef");
        store
            .upsert_install_record(&install)
            .expect("install records");

        for outcome in [
            UpdateCheckOutcome::UpdateAvailable,
            UpdateCheckOutcome::Blocked,
        ] {
            let mut check = recorded_check(&install, outcome);
            check.latest_revision = Some("def456".to_string());
            store.record_update_check(&check).expect("check records");

            // The check looked at the waiting revision and found no scripts, so
            // the repair apply moves the import without meeting the gate.
            assert_eq!(
                script_free_advice(&store, &install),
                crate::apply::RepairCommands::UpdateStrategies,
                "a script-free waiting revision does not gate the {outcome:?} repair",
            );
        }
    }

    #[test]
    fn an_installed_scripts_reason_on_the_row_does_not_gate_a_script_free_import() {
        let mut store = crate::state::LocalStateStore::open_in_memory().expect("store opens");
        let install = git_install("0123456789abcdef");
        store
            .upsert_install_record(&install)
            .expect("install records");
        let mut check = recorded_check(&install, UpdateCheckOutcome::UpdateAvailable);
        check.apply_blocked_by = Some(UpdateCheckBlockReason::InstalledScripts);
        store.record_update_check(&check).expect("check records");

        // The local state is authoritative for installed content, and it holds
        // no scripts, so only the incoming reason can gate the repair.
        assert_eq!(
            script_free_advice(&store, &install),
            crate::apply::RepairCommands::UpdateStrategies,
        );
    }

    #[test]
    fn a_check_for_an_earlier_pin_cannot_gate_a_script_free_import() {
        let mut store = crate::state::LocalStateStore::open_in_memory().expect("store opens");
        let install = git_install("0123456789abcdef");
        store
            .upsert_install_record(&install)
            .expect("install records");
        let mut check = recorded_check(&install, UpdateCheckOutcome::Blocked);
        check.latest_revision = Some("def456".to_string());
        check.apply_blocked_by = Some(UpdateCheckBlockReason::IncomingScripts);
        store.record_update_check(&check).expect("check records");

        // The import moved after the check, so the block describes a revision
        // the workspace already left behind.
        let mut moved = install.clone();
        moved.resolved_revision = "1111111111111111".to_string();
        store
            .upsert_install_record(&moved)
            .expect("install records");
        assert_eq!(
            script_free_advice(&store, &moved),
            crate::apply::RepairCommands::UpdateStrategies,
        );
    }

    #[test]
    fn an_approval_recorded_after_the_check_licenses_the_repair() {
        let mut store = crate::state::LocalStateStore::open_in_memory().expect("store opens");
        let install = git_install("0123456789abcdef");
        store
            .upsert_install_record(&install)
            .expect("install records");
        let mut check = recorded_check(&install, UpdateCheckOutcome::UpdateAvailable);
        check.latest_revision = Some("def456".to_string());
        check.apply_blocked_by = Some(UpdateCheckBlockReason::IncomingScripts);
        store.record_update_check(&check).expect("check records");

        crate::history::HistoryLedger::new(&mut store)
            .record_trust_review(&approved_review("sha256:effective"))
            .expect("review records");
        assert_eq!(
            script_free_advice(&store, &install),
            crate::apply::RepairCommands::UpdateStrategies,
            "a recorded approval licenses the move the check held back",
        );
    }

    #[test]
    fn an_immutable_pin_leaves_a_script_free_repair_ungated_after_a_gated_check() {
        let mut store = crate::state::LocalStateStore::open_in_memory().expect("store opens");
        let install = git_install("0123456789abcdef");
        store
            .upsert_install_record(&install)
            .expect("install records");
        let mut check = recorded_check(&install, UpdateCheckOutcome::UpdateAvailable);
        check.latest_revision = Some("def456".to_string());
        check.apply_blocked_by = Some(UpdateCheckBlockReason::IncomingScripts);
        store.record_update_check(&check).expect("check records");
        assert_eq!(
            script_free_advice(&store, &install),
            crate::apply::RepairCommands::GatedUpdateStrategies,
            "the fixture starts in the gated state",
        );

        // The pin names the commit the install already holds, so the repair
        // keeps it and no apply can move to the revision the check saw.
        store
            .upsert_pin_record(&crate::state::PinRecord {
                skill: install.skill.clone(),
                requested_reference: install.resolved_revision.clone(),
                resolved_revision: install.resolved_revision.clone(),
                effective_version_hash: None,
                pinned_at: "2026-11-07T10:10:00Z".to_string(),
            })
            .expect("pin records");
        assert_eq!(
            script_free_advice(&store, &install),
            crate::apply::RepairCommands::UpdateStrategies,
            "a repair that keeps a commit pin is not gated",
        );
    }

    #[test]
    fn an_elevated_import_blocks_only_the_gated_action() {
        let trust = SkillTrust::imported_unreviewed("delta", true).block_apply_update("delta");

        assert!(trust.blocks(TrustBlockedAction::ApplyUpdate));
        assert_eq!(trust.blocked_actions, vec![TrustBlockedAction::ApplyUpdate]);
    }

    #[test]
    fn a_normal_risk_decision_blocks_nothing() {
        let clean = SkillTrust::imported_unreviewed("delta", false).block_apply_update("delta");
        let local = SkillTrust::local(true).block_apply_update("delta");

        assert!(!clean.blocks(TrustBlockedAction::ApplyUpdate));
        assert!(!local.blocks(TrustBlockedAction::ApplyUpdate));
    }

    #[test]
    fn only_an_elevated_import_gates_an_update_apply() {
        assert!(SkillTrust::imported_unreviewed("delta", true).gates_update_apply("delta"));
        assert!(!SkillTrust::imported_unreviewed("delta", false).gates_update_apply("delta"));
        assert!(!SkillTrust::local(true).gates_update_apply("delta"));
    }

    #[test]
    fn an_approval_makes_the_imported_state_reviewed_and_ungated() {
        let review = approved_review("sha256:effective");
        let trust = SkillTrust::imported("delta", true, Some(&review)).block_apply_update("delta");

        assert_eq!(trust.effective_state, TrustState::ImportedReviewed);
        assert_eq!(trust.source_state, TrustState::ImportedReviewed);
        assert_eq!(trust.risk_level, TrustRiskLevel::Normal);
        assert!(!trust.review_required);
        assert!(!trust.blocks(TrustBlockedAction::ApplyUpdate));
        assert_eq!(trust.review.as_ref(), Some(&review));
    }

    #[test]
    fn a_reviewed_import_warns_about_nothing() {
        let trust = SkillTrust::imported_reviewed(true, &approved_review("sha256:effective"));

        // The elevated messaging holds only until a review exists, so a
        // reviewed import reports its facts through `contains_scripts` and
        // `review` instead of through a warning.
        assert!(trust.contains_scripts);
        assert!(trust.warnings.is_empty());
        assert_eq!(
            trust.review.as_ref(),
            Some(&approved_review("sha256:effective"))
        );
    }

    #[test]
    fn a_revoked_or_absent_decision_keeps_the_import_unreviewed() {
        let mut revoked = approved_review("sha256:effective");
        revoked.decision = ReviewDecision::Revoked;

        for review in [None, Some(&revoked)] {
            let trust = SkillTrust::imported("delta", true, review);
            assert_eq!(trust.effective_state, TrustState::ImportedUnreviewed);
            assert_eq!(trust.risk_level, TrustRiskLevel::Elevated);
            assert!(trust.review.is_none());
        }
    }

    #[test]
    fn an_approval_only_covers_its_own_effective_version() {
        let mut index = ReviewIndex::default();
        let record = approved_review("sha256:effective");
        index
            .0
            .insert((ManagedScope::Workspace, "delta".to_string()), record);

        let skill = ManagedSkillRef::new(ManagedScope::Workspace, "delta");
        assert!(index.approved(&skill, "sha256:effective").is_some());
        assert!(
            index.approved(&skill, "sha256:next").is_none(),
            "content the review never covered must stay unreviewed",
        );
        assert!(
            index
                .approved(
                    &ManagedSkillRef::new(ManagedScope::User, "delta"),
                    "sha256:effective"
                )
                .is_none()
        );
    }

    #[test]
    fn a_revision_that_introduces_scripts_blocks_an_unreviewed_apply() {
        let trust = SkillTrust::imported_unreviewed("delta", false)
            .block_apply_update_for_incoming_scripts("delta", "def456");

        // The installed content keeps reporting its own facts, so the decision
        // stays honest about the content the workspace holds.
        assert!(!trust.contains_scripts);
        assert_eq!(trust.risk_level, TrustRiskLevel::Normal);
        assert!(trust.blocks(TrustBlockedAction::ApplyUpdate));
        assert_eq!(trust.warnings.len(), 1);
        let warning = &trust.warnings[0];
        assert!(
            warning.contains("trust gate is blocking update apply for 'delta'"),
            "unexpected warning: {warning}",
        );
        assert!(
            warning.contains("the incoming revision def456 adds a top-level scripts/ directory"),
            "the warning names the content that arrives: {warning}",
        );
        assert!(
            warning.contains("skillctl review delta --approve"),
            "the warning names the way past the gate: {warning}",
        );
    }

    #[test]
    fn the_installed_scripts_block_comes_without_the_incoming_clause() {
        let trust = SkillTrust::imported_unreviewed("delta", true).block_apply_update("delta");

        assert!(trust.contains_scripts);
        assert_eq!(trust.risk_level, TrustRiskLevel::Elevated);
        assert!(trust.blocks(TrustBlockedAction::ApplyUpdate));
        assert!(
            !trust.warnings[0].contains("incoming revision"),
            "the installed block names the installed content only: {}",
            trust.warnings[0],
        );
    }

    #[test]
    fn a_revision_apply_sees_the_scripts_of_both_trees() {
        let working_directory = std::env::temp_dir();
        let install = git_install("0123456789abcdef");
        let staged = tempfile::TempDir::new().expect("staging directory exists");
        let incoming = staged.path().join("checkout");

        // Neither tree holds scripts, so the apply moves an unreviewed import
        // without meeting the gate.
        let clean = trust_for_revision_apply(
            &working_directory,
            &install,
            "skills/delta",
            Some(&incoming),
            None,
        )
        .expect("trust builds");
        assert!(!clean.contains_scripts);
        assert!(!clean.gates_update_apply("delta"));

        // The incoming revision alone introduces scripts, which is the first
        // arrival of executable content the check-time gate cannot see.
        fs::create_dir_all(incoming.join("skills/delta/scripts"))
            .expect("incoming scripts directory exists");
        let gated = trust_for_revision_apply(
            &working_directory,
            &install,
            "skills/delta",
            Some(&incoming),
            None,
        )
        .expect("trust builds");
        assert!(gated.contains_scripts);
        assert!(gated.gates_update_apply("delta"));

        // A recorded approval of the installed content licenses the move, the
        // same license the installed-scripts block accepts.
        let reviewed = trust_for_revision_apply(
            &working_directory,
            &install,
            "skills/delta",
            Some(&incoming),
            Some(&approved_review("sha256:effective")),
        )
        .expect("trust builds");
        assert!(reviewed.contains_scripts);
        assert!(!reviewed.gates_update_apply("delta"));
    }

    #[test]
    fn a_revision_apply_without_a_staged_tree_covers_the_stored_import_only() {
        let working_directory = std::env::temp_dir();
        let install = git_install("0123456789abcdef");

        let trust =
            trust_for_revision_apply(&working_directory, &install, "skills/delta", None, None)
                .expect("trust builds");
        assert!(!trust.contains_scripts);
        assert_eq!(trust.effective_state, TrustState::ImportedUnreviewed);
    }

    #[test]
    fn an_unreachable_upstream_revision_answers_no_scripts() {
        // The gate for the incoming revision is deferred to the apply, which
        // checks out the tree it installs, so a source that cannot be reached
        // here must not turn the check into a failure.
        assert!(!upstream_revision_contains_scripts(
            "file:///tmp/skillctl-missing-upstream-for-tests",
            "0123456789abcdef",
            "skills/delta",
        ));
    }
}
