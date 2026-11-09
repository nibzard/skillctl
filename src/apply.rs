//! One guarded apply engine shared by every mutating skillctl operation.
//!
//! The engine owns three invariants that every mutation must respect:
//!
//! 1. A mutation runs from a typed, immutable plan. The plan records the state
//!    a workspace was in when it was built, what the operation intends to
//!    change, and where recovery material lives. Applying a plan re-verifies
//!    the recorded state first, so a plan built against different local state,
//!    or a plan document edited after it was written, fails before any
//!    mutation happens.
//! 2. Canonical bundle identity travels with every mutation. A mutation that
//!    changes imported content recomputes the bundle digest and retains the
//!    artifact. A mutation that only changes local bookkeeping preserves the
//!    recorded digest instead of dropping it.
//! 3. Unrecorded local edits are never silently discarded. Generated status
//!    proves ownership of a directory, not permission to delete user content.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    app::AppContext,
    artifact::ArtifactStore,
    bundle::BundleManifest,
    error::AppError,
    lifecycle::{self, LifecycleTransaction},
    lockfile::{LockedBundle, WorkspaceLockfile},
    manifest::{ManifestScope, ProjectionMode, WorkspaceManifest},
    materialize::PROJECTION_METADATA_FILE,
    resolver::{self, ResolvedSkillCandidate, SkillScope},
    source::{current_timestamp, hash_directory_contents, stored_import_root},
    state::{InstallRecord, LocalStateStore, ManagedScope, ManagedSkillRef},
};

/// Directory below the skillctl home that holds durable pre-discard backups.
pub const DISCARDS_STORE_RELATIVE_PATH: &str = ".skillctl/store/discards";
/// Schema version of a serialized plan document.
pub const PLAN_SCHEMA_VERSION: u32 = 1;

/// Lifecycle operations executed by the shared engine.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum MutationOperation {
    /// Install one or more skills from a source.
    Install,
    /// Apply an approved upstream revision.
    Update,
    /// Pin an import to an exact revision.
    Pin,
    /// Roll an import back to a recorded revision.
    Rollback,
    /// Rebuild generated projections.
    Sync,
    /// Remove generated projections and unused cached state.
    Clean,
    /// Re-enable a managed import.
    Enable,
    /// Disable a managed import.
    Disable,
    /// Remove a managed skill.
    Remove,
    /// Create or reuse a managed overlay.
    Override,
    /// Detach an import into local canonical ownership.
    Fork,
    /// Record a local review decision for an import.
    Review,
}

impl MutationOperation {
    /// Return the disable operation when `enabled` is false.
    pub const fn or_disable(self, enabled: bool) -> Self {
        if enabled { self } else { Self::Disable }
    }

    /// Stable identifier used as the lifecycle journal operation name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Install => "install",
            Self::Update => "update-apply",
            Self::Pin => "pin",
            Self::Rollback => "rollback",
            Self::Sync => "sync",
            Self::Clean => "clean",
            Self::Enable => "enable",
            Self::Disable => "disable",
            Self::Remove => "remove",
            Self::Override => "override",
            Self::Fork => "fork",
            Self::Review => "review",
        }
    }
}

/// Expected pre-state of one managed import when a plan was built.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ExpectedImport {
    /// Import identifier used as the lockfile key.
    pub import_id: String,
    /// Scope that owns the import.
    pub scope: ManagedScope,
    /// Immutable revision the import was pinned to.
    pub revision: String,
    /// Source content hash recorded in the lockfile.
    pub content_hash: String,
    /// Overlay hash recorded in the lockfile.
    pub overlay_hash: String,
    /// Effective version hash recorded in the lockfile.
    pub effective_version_hash: String,
    /// Canonical bundle digest recorded in the lockfile, when one exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle_digest: Option<String>,
    /// Whether the state store held an install record for this import when
    /// the plan was built.
    ///
    /// A fresh checkout carries the committed manifest and lockfile but no
    /// install record, because the import cache and the state store are local
    /// to each machine. A plan built in that state verifies the lockfile entry
    /// alone. Plans written before this field existed default to requiring the
    /// record, which keeps older plan documents as strict as they were.
    #[serde(default = "expected_install_record_present")]
    pub install_record_present: bool,
}

impl ExpectedImport {
    /// Whether the plan requires a recorded install for this import.
    pub const fn requires_install_record(&self) -> bool {
        self.install_record_present
    }
}

/// Serde default that keeps plans written before the field existed strict.
const fn expected_install_record_present() -> bool {
    true
}

/// State an operation intends to move one managed import to.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProposedImport {
    /// Import identifier used as the lockfile key.
    pub import_id: String,
    /// Revision the operation moves to, when it moves content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    /// Canonical bundle digest the mutation must leave behind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle_digest: Option<String>,
    /// Whether the mutation recomputes or preserves the recorded bundle.
    pub bundle_policy: BundlePolicy,
    /// One-line description of the intended change.
    pub summary: String,
}

/// How a mutation treats the canonical bundle identity of an import.
///
/// The policy is descriptive: it tells a reader of a plan document how the
/// mutation treats the bundle digest. The engine derives the real outcome from
/// the operations the plan applies, so a wrong declaration changes the document
/// a reader sees, not what the apply does.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum BundlePolicy {
    /// Recompute the digest from the new content and retain the artifact.
    Recompute,
    /// Preserve the recorded digest because content does not change.
    Preserve,
    /// Drop the import record and keep the retained artifact as recovery.
    Release,
}

/// What the engine will do with one filesystem target.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum TargetAction {
    /// Create a path that does not exist yet.
    Create,
    /// Replace the recorded content of a path.
    Replace,
    /// Update a path in place.
    Update,
    /// Remove a path.
    Remove,
}

/// One filesystem target a plan is allowed to change.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PlanTarget {
    /// Absolute path the operation may change.
    pub path: String,
    /// Action applied to the path.
    ///
    /// The action is descriptive: it tells a reader of a plan document what the
    /// operation intends. The guard backs up and restores every declared target
    /// the same way, so the engine does not branch on this value.
    pub action: TargetAction,
}

/// Concise difference between two content trees.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ChangeDiff {
    /// Files present only in the new tree.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub added: Vec<String>,
    /// Files present only in the previous tree.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub removed: Vec<String>,
    /// Files whose bytes or mode changed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub modified: Vec<String>,
}

/// One proposed change a plan will make.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PlannedChange {
    /// Short human-readable description.
    pub summary: String,
    /// Concise file difference, when the change moves content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<ChangeDiff>,
}

/// Recovery material an operation leaves behind.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct RecoveryRefs {
    /// Lifecycle operation that owns the journal for this plan.
    ///
    /// The name is descriptive: it tells a reader of a plan document where the
    /// journal lives. The journal itself is opened by the operation that runs
    /// the plan, so the engine does not consult this value.
    pub journal_operation: String,
    /// Directory that holds durable pre-discard backups.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discard_root: Option<String>,
    /// Retained artifacts an operation keeps as recovery material.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub retained_artifacts: Vec<String>,
}

/// Typed immutable description of one guarded mutation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct MutationPlan {
    /// Plan schema version.
    pub schema_version: u32,
    /// Stable plan identifier.
    pub id: String,
    /// Operation that applies this plan.
    pub operation: MutationOperation,
    /// Timestamp recorded when the plan was built.
    pub created_at: String,
    /// Absolute workspace path the plan was built against.
    pub workspace: String,
    /// Managed skill the plan targets, when it targets one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill: Option<ManagedSkillRef>,
    /// Pre-state the plan was built against.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expected: Vec<ExpectedImport>,
    /// Changes the plan intends to make.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub proposed: Vec<ProposedImport>,
    /// Filesystem targets the plan may change.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub targets: Vec<PlanTarget>,
    /// Human- and machine-readable change list.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changes: Vec<PlannedChange>,
    /// Recovery references recorded with the plan.
    #[serde(default)]
    pub recovery: RecoveryRefs,
    /// Self digest covering every other field.
    pub digest: String,
}

/// Incremental builder for a [`MutationPlan`].
#[derive(Clone, Debug)]
pub struct MutationPlanBuilder {
    operation: MutationOperation,
    created_at: String,
    workspace: String,
    skill: Option<ManagedSkillRef>,
    expected: Vec<ExpectedImport>,
    proposed: Vec<ProposedImport>,
    targets: Vec<PlanTarget>,
    changes: Vec<PlannedChange>,
    recovery: RecoveryRefs,
}

impl MutationPlanBuilder {
    /// Start a plan for one operation in the given workspace.
    pub fn new(operation: MutationOperation, working_directory: &Path) -> Self {
        Self {
            operation,
            created_at: current_timestamp(),
            workspace: working_directory.display().to_string(),
            skill: None,
            expected: Vec::new(),
            proposed: Vec::new(),
            targets: Vec::new(),
            changes: Vec::new(),
            recovery: RecoveryRefs {
                journal_operation: operation.as_str().to_string(),
                discard_root: None,
                retained_artifacts: Vec::new(),
            },
        }
    }

    /// Record the managed skill this plan targets.
    pub fn skill(mut self, skill: ManagedSkillRef) -> Self {
        self.skill = Some(skill);
        self
    }

    /// Record the pre-state of one managed import.
    pub fn expect_import(mut self, expected: ExpectedImport) -> Self {
        self.expected.push(expected);
        self
    }

    /// Record every managed import pre-state at once.
    pub fn expect_imports(mut self, expected: Vec<ExpectedImport>) -> Self {
        self.expected.extend(expected);
        self
    }

    /// Record one intended import change.
    pub fn propose(mut self, proposed: ProposedImport) -> Self {
        self.proposed.push(proposed);
        self
    }

    /// Declare one filesystem target by path.
    pub fn target(mut self, path: impl AsRef<Path>, action: TargetAction) -> Self {
        self.targets.push(PlanTarget {
            path: path.as_ref().display().to_string(),
            action,
        });
        self
    }

    /// Declare several filesystem targets at once.
    pub fn targets(
        mut self,
        paths: impl IntoIterator<Item = PathBuf>,
        action: TargetAction,
    ) -> Self {
        for path in paths {
            self.targets.push(PlanTarget {
                path: path.display().to_string(),
                action,
            });
        }
        self
    }

    /// Describe one change the plan makes.
    pub fn change(mut self, summary: impl Into<String>, diff: Option<ChangeDiff>) -> Self {
        self.changes.push(PlannedChange {
            summary: summary.into(),
            diff,
        });
        self
    }

    /// Record the durable pre-discard backup root the plan may use.
    pub fn discard_root(mut self, root: impl Into<String>) -> Self {
        self.recovery.discard_root = Some(root.into());
        self
    }

    /// Record retained artifacts the plan keeps as recovery material.
    pub fn retained_artifacts(mut self, digests: Vec<String>) -> Self {
        self.recovery.retained_artifacts.extend(digests);
        self
    }

    /// Compute the plan digest and finish the plan.
    pub fn seal(self) -> MutationPlan {
        let id = match &self.skill {
            Some(skill) => format!("{}-{}", self.operation.as_str(), skill.skill_id),
            None => self.operation.as_str().to_string(),
        };
        let plan = MutationPlan {
            schema_version: PLAN_SCHEMA_VERSION,
            id,
            operation: self.operation,
            created_at: self.created_at,
            workspace: self.workspace,
            skill: self.skill,
            expected: self.expected,
            proposed: self.proposed,
            targets: self.targets,
            changes: self.changes,
            recovery: self.recovery,
            digest: String::new(),
        };
        let digest = compute_plan_digest(&plan);
        MutationPlan { digest, ..plan }
    }
}

/// Compute the self digest of a plan, excluding the digest field itself.
pub fn compute_plan_digest(plan: &MutationPlan) -> String {
    let mut value = serde_json::to_value(plan).unwrap_or(serde_json::Value::Null);
    if let serde_json::Value::Object(fields) = &mut value {
        fields.remove("digest");
    }

    let payload = serde_json::to_vec(&value).unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(payload);
    format!("sha256:{:x}", hasher.finalize())
}

impl MutationPlan {
    /// Verify the recorded digest still covers the plan content.
    pub fn verify_digest(&self) -> Result<(), AppError> {
        let computed = compute_plan_digest(self);
        if computed == self.digest {
            return Ok(());
        }

        Err(AppError::TamperedPlan {
            plan_id: self.id.clone(),
            message: format!(
                "the plan document digests to {computed} but claims '{}'; rebuild the plan with \
                 'skillctl update --plan' instead of editing it",
                self.digest
            ),
        })
    }

    /// Verify the workspace still holds the pre-state this plan was built from.
    ///
    /// The check compares lockfile revision, content, overlay, and effective
    /// version hashes, the canonical bundle digest, and the install record. It
    /// reads state only, so it runs before any mutation.
    pub fn verify_against(&self, context: &AppContext) -> Result<(), AppError> {
        self.verify_digest()?;
        if self.expected.is_empty() {
            return Ok(());
        }

        let lockfile = load_lockfile(&context.working_directory)?;
        let store = LocalStateStore::open_default_for(&context.working_directory)?;

        for expected in &self.expected {
            let entry = lockfile
                .imports
                .get(&expected.import_id)
                .ok_or_else(|| stale_import(self, expected, "the lockfile entry was removed"))?;

            self.compare(
                "revision.resolved",
                &expected.revision,
                &entry.revision.resolved,
                expected.import_id.as_str(),
            )?;
            self.compare(
                "hashes.content",
                &expected.content_hash,
                &entry.hashes.content,
                expected.import_id.as_str(),
            )?;
            self.compare(
                "hashes.overlay",
                &expected.overlay_hash,
                &entry.hashes.overlay,
                expected.import_id.as_str(),
            )?;
            self.compare(
                "hashes.effective_version",
                &expected.effective_version_hash,
                &entry.hashes.effective_version,
                expected.import_id.as_str(),
            )?;
            self.compare(
                "bundle.digest",
                expected.bundle_digest.as_deref().unwrap_or("<none>"),
                entry.bundle_digest().unwrap_or("<none>"),
                expected.import_id.as_str(),
            )?;

            let skill = ManagedSkillRef::new(expected.scope, expected.import_id.clone());
            match store.install_record(&skill)? {
                Some(record) => {
                    self.compare(
                        "install.effective_version_hash",
                        &expected.effective_version_hash,
                        &record.effective_version_hash,
                        expected.import_id.as_str(),
                    )?;
                    self.compare(
                        "install.resolved_revision",
                        &expected.revision,
                        &record.resolved_revision,
                        expected.import_id.as_str(),
                    )?;
                }
                None if !expected.requires_install_record() => {}
                None => {
                    return Err(stale_import(
                        self,
                        expected,
                        "the install record was removed",
                    ));
                }
            }
        }

        Ok(())
    }

    fn compare(
        &self,
        field: &str,
        expected: &str,
        observed: &str,
        import_id: &str,
    ) -> Result<(), AppError> {
        if expected == observed {
            return Ok(());
        }

        Err(AppError::StalePlan {
            plan_id: self.id.clone(),
            message: format!(
                "imports.{import_id}.{field} is '{observed}' but the plan was built against \
                 '{expected}'; local state changed after planning, so rebuild the plan with \
                 'skillctl update --plan' before applying it"
            ),
        })
    }
}

fn stale_import(plan: &MutationPlan, expected: &ExpectedImport, reason: &str) -> AppError {
    AppError::StalePlan {
        plan_id: plan.id.clone(),
        message: format!(
            "imports.{} was expected at revision '{}' but {reason}; rebuild the plan with \
             'skillctl update --plan' before applying it",
            expected.import_id, expected.revision
        ),
    }
}

fn load_lockfile(working_directory: &Path) -> Result<WorkspaceLockfile, AppError> {
    match WorkspaceLockfile::load_from_workspace(working_directory) {
        Ok(lockfile) => Ok(lockfile),
        Err(AppError::FilesystemOperation {
            action: "read lockfile",
            path,
            source,
        }) if source.kind() == std::io::ErrorKind::NotFound => {
            Ok(WorkspaceLockfile::default_at(path))
        }
        Err(error) => Err(error),
    }
}

/// Capture the current pre-state of every managed import in one scope.
pub fn capture_expected_imports(
    context: &AppContext,
    scope: Option<ManagedScope>,
) -> Result<Vec<ExpectedImport>, AppError> {
    let lockfile = load_lockfile(&context.working_directory)?;
    let store = LocalStateStore::open_default_for(&context.working_directory)?;
    let mut expected = Vec::new();

    for record in store.list_install_records()? {
        if scope.is_some_and(|scope| record.skill.scope != scope) {
            continue;
        }
        if record.detached || record.forked {
            continue;
        }

        // A bundled install is locally owned, not an import. The lockfile keys
        // imports by name without a scope, so the only entry its name can match
        // is a same-named import's; pairing the two mixes two skills' identity
        // into one expectation the plan can never verify again.
        if crate::builtin::is_bundled_install(&record) {
            continue;
        }

        // Installs without a lockfile entry are bundled or locally owned and
        // carry no dependency identity to guard.
        let Some(entry) = lockfile.imports.get(&record.skill.skill_id) else {
            continue;
        };
        expected.push(ExpectedImport {
            import_id: record.skill.skill_id.clone(),
            scope: record.skill.scope,
            revision: entry.revision.resolved.clone(),
            content_hash: entry.hashes.content.clone(),
            overlay_hash: entry.hashes.overlay.clone(),
            effective_version_hash: record.effective_version_hash.clone(),
            bundle_digest: entry.bundle_digest().map(str::to_string),
            install_record_present: true,
        });
    }

    expected.sort_by(|left, right| left.import_id.cmp(&right.import_id));
    Ok(expected)
}

/// Capture the expected pre-state of one managed import.
pub fn capture_expected_import(
    context: &AppContext,
    record: &InstallRecord,
) -> Result<ExpectedImport, AppError> {
    let lockfile = load_lockfile(&context.working_directory)?;
    let entry = lockfile.imports.get(&record.skill.skill_id);
    Ok(ExpectedImport {
        import_id: record.skill.skill_id.clone(),
        scope: record.skill.scope,
        revision: entry.map_or_else(
            || record.resolved_revision.clone(),
            |entry| entry.revision.resolved.clone(),
        ),
        content_hash: entry.map_or_else(
            || record.content_hash.clone(),
            |entry| entry.hashes.content.clone(),
        ),
        overlay_hash: entry.map_or_else(
            || record.overlay_hash.clone(),
            |entry| entry.hashes.overlay.clone(),
        ),
        effective_version_hash: record.effective_version_hash.clone(),
        bundle_digest: entry.and_then(|entry| entry.bundle_digest().map(str::to_string)),
        install_record_present: true,
    })
}

/// Capture the pre-state of one import from the committed lockfile alone.
///
/// A fresh checkout holds the manifest and the lockfile but no install record,
/// because the state store is local to each machine. A mutation that repairs
/// such an import still guards the recorded dependency identity, so it can
/// fail before touching anything when the lockfile moved underneath it.
pub fn capture_expected_import_from_lockfile(
    skill: &ManagedSkillRef,
    entry: &crate::lockfile::LockedImport,
) -> ExpectedImport {
    ExpectedImport {
        import_id: skill.skill_id.clone(),
        scope: skill.scope,
        revision: entry.revision.resolved.clone(),
        content_hash: entry.hashes.content.clone(),
        overlay_hash: entry.hashes.overlay.clone(),
        effective_version_hash: entry.hashes.effective_version.clone(),
        bundle_digest: entry.bundle_digest().map(str::to_string),
        install_record_present: false,
    }
}

/// Run one mutation behind the shared guard.
///
/// The guard verifies the plan against the workspace, stages a durable
/// journal backup of every declared target, and only then runs `work`. A plan
/// that no longer matches local state fails before any mutation happens.
pub(crate) fn run_guarded<T>(
    plan: &MutationPlan,
    context: &AppContext,
    work: impl FnOnce(&mut LifecycleTransaction) -> Result<T, AppError>,
) -> Result<T, AppError> {
    let operation = plan.operation.as_str();
    lifecycle::run_transaction(operation, |transaction| {
        plan.verify_against(context)?;
        for target in &plan.targets {
            transaction.track_path(&target.path)?;
        }
        let result = work(transaction)?;
        transaction.checkpoint("after-state")?;
        Ok(result)
    })
}

// ---------------------------------------------------------------------------
// Plan documents
// ---------------------------------------------------------------------------

/// Serialized form of one or more plans plus the apply strategy they expect.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PlanDocument {
    /// Plan schema version.
    pub schema_version: u32,
    /// When the document was written.
    pub created_at: String,
    /// Absolute workspace path the plans were built against.
    pub workspace: String,
    /// Strategy the plans were built for.
    pub strategy: String,
    /// Plans in this document, in apply order.
    pub plans: Vec<MutationPlan>,
}

impl PlanDocument {
    /// Build a document from finished plans.
    pub fn new(working_directory: &Path, strategy: &str, plans: Vec<MutationPlan>) -> Self {
        Self {
            schema_version: PLAN_SCHEMA_VERSION,
            created_at: current_timestamp(),
            workspace: working_directory.display().to_string(),
            strategy: strategy.to_string(),
            plans,
        }
    }

    /// Verify every plan digest in the document.
    pub fn verify_digests(&self) -> Result<(), AppError> {
        for plan in &self.plans {
            plan.verify_digest()?;
        }
        Ok(())
    }

    /// Render the document as pretty JSON text.
    pub fn to_json_string(&self) -> Result<String, AppError> {
        Ok(format!(
            "{}\n",
            serde_json::to_string_pretty(self).map_err(|source| AppError::JsonRender { source })?
        ))
    }

    /// Parse a document and verify every plan digest.
    pub fn from_json_str(contents: &str) -> Result<Self, AppError> {
        let document: PlanDocument =
            serde_json::from_str(contents).map_err(|source| AppError::TamperedPlan {
                plan_id: "<document>".to_string(),
                message: format!("the plan document is not valid JSON: {source}"),
            })?;
        if document.schema_version != PLAN_SCHEMA_VERSION {
            return Err(AppError::TamperedPlan {
                plan_id: "<document>".to_string(),
                message: format!(
                    "plan document schema version {} is not supported; this skillctl supports \
                     {PLAN_SCHEMA_VERSION}",
                    document.schema_version
                ),
            });
        }
        document.verify_digests()?;
        Ok(document)
    }

    /// Load and verify a document from disk.
    pub fn load_from_path(path: &Path) -> Result<Self, AppError> {
        let contents =
            fs::read_to_string(path).map_err(|source| AppError::FilesystemOperation {
                action: "read plan document",
                path: path.to_path_buf(),
                source,
            })?;
        Self::from_json_str(&contents)
    }
}

// ---------------------------------------------------------------------------
// Durable pre-discard backups
// ---------------------------------------------------------------------------

/// Durable store for content preserved before an explicit discard.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscardStore {
    root: PathBuf,
}

impl DiscardStore {
    /// Open the default store below the skillctl home directory.
    pub fn open_default() -> Result<Self, AppError> {
        let home =
            crate::state::resolve_home_directory().ok_or(AppError::HomeDirectoryUnavailable)?;
        Ok(Self {
            root: home.join(DISCARDS_STORE_RELATIVE_PATH),
        })
    }

    /// Root directory of the store.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Retain one tree durably before it is discarded.
    ///
    /// The source is copied, never moved, so the caller keeps control of the
    /// original path until it chooses to replace it. The backup name counts up
    /// when an earlier backup already holds it, because a pinned clock
    /// (`SOURCE_DATE_EPOCH`) or a repeated process id can reproduce a name
    /// without any state having been lost, so an existing sibling never blocks
    /// the discard it is asked to back up.
    pub fn retain(&self, label: &str, source: &Path) -> Result<PathBuf, AppError> {
        let label_root = self.root.join(sanitize_component(label));
        let base_name = retained_backup_base_name();
        let mut destination = label_root.join(&base_name);
        let mut attempt = 1;
        while destination.exists() {
            attempt += 1;
            destination = label_root.join(format!("{base_name}-{attempt}"));
        }

        fs::create_dir_all(&destination).map_err(|source| AppError::FilesystemOperation {
            action: "create retained backup directory",
            path: destination.clone(),
            source,
        })?;
        copy_retained_tree(source, &destination)?;
        Ok(destination)
    }
}

/// Copy one retained projection tree, keeping links as links.
///
/// A generated copy may project its files through symlinks, and a retained
/// backup exists so its owner can recover exactly what was discarded. Links are
/// therefore recreated instead of resolved. Import sources keep their own
/// stricter copier, which rejects symlinks to keep the stored imports plain.
fn copy_retained_tree(source_root: &Path, destination_root: &Path) -> Result<(), AppError> {
    let mut entries = match fs::read_dir(source_root) {
        Ok(entries) => entries,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(AppError::FilesystemOperation {
                action: "read retained projection",
                path: source_root.to_path_buf(),
                source,
            });
        }
    }
    .collect::<Result<Vec<_>, _>>()
    .map_err(|source| AppError::FilesystemOperation {
        action: "read retained projection entry",
        path: source_root.to_path_buf(),
        source,
    })?;
    entries.sort_by_key(fs::DirEntry::file_name);

    for entry in entries {
        let source_path = entry.path();
        let destination_path = destination_root.join(entry.file_name());
        let metadata =
            fs::symlink_metadata(&source_path).map_err(|source| AppError::FilesystemOperation {
                action: "inspect retained projection entry",
                path: source_path.clone(),
                source,
            })?;

        if metadata.file_type().is_symlink() {
            let target =
                fs::read_link(&source_path).map_err(|source| AppError::FilesystemOperation {
                    action: "read retained symlink target",
                    path: source_path.clone(),
                    source,
                })?;
            create_retained_symlink(&target, &destination_path)?;
            continue;
        }

        if metadata.is_dir() {
            fs::create_dir_all(&destination_path).map_err(|source| {
                AppError::FilesystemOperation {
                    action: "create retained backup directory",
                    path: destination_path.clone(),
                    source,
                }
            })?;
            copy_retained_tree(&source_path, &destination_path)?;
            continue;
        }

        fs::copy(&source_path, &destination_path).map_err(|source| {
            AppError::FilesystemOperation {
                action: "copy retained projection file",
                path: destination_path,
                source,
            }
        })?;
    }

    Ok(())
}

#[cfg(unix)]
fn create_retained_symlink(target: &Path, destination: &Path) -> Result<(), AppError> {
    std::os::unix::fs::symlink(target, destination).map_err(|source| {
        AppError::FilesystemOperation {
            action: "recreate retained symlink",
            path: destination.to_path_buf(),
            source,
        }
    })
}

#[cfg(windows)]
fn create_retained_symlink(target: &Path, destination: &Path) -> Result<(), AppError> {
    std::os::windows::fs::symlink_file(target, destination).map_err(|source| {
        AppError::FilesystemOperation {
            action: "recreate retained symlink",
            path: destination.to_path_buf(),
            source,
        }
    })
}

fn sanitize_component(value: &str) -> String {
    let mut sanitized = String::with_capacity(value.len());
    for character in value.chars() {
        if character.is_ascii_alphanumeric() || character == '-' {
            sanitized.push(character);
        } else {
            sanitized.push('-');
        }
    }
    if sanitized.is_empty() {
        "unnamed".to_string()
    } else {
        sanitized
    }
}

/// The base name every retained backup directory derives its identity from.
///
/// The runtime stamp is sanitized before it becomes a path component,
/// because its RFC 3339 colons are legal on Unix but rejected by Windows
/// everywhere except the drive-letter position.
fn retained_backup_base_name() -> String {
    sanitize_component(&format!("{}-{}", current_timestamp(), std::process::id()))
}

// ---------------------------------------------------------------------------
// Dirty-content protection for generated projections
// ---------------------------------------------------------------------------

/// Why a generated projection must not be replaced or pruned.
///
/// The serialized form of each variant equals its [`DriftReason::as_str`]
/// identifier, so every report names one drift reason one way.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum DriftReason {
    /// The generated copy differs from the content skillctl recorded for it.
    #[serde(rename = "dirty-projected-copy")]
    DirtyProjection,
    /// The copy predates recorded digests and no longer matches the plan.
    #[serde(rename = "unverifiable-projected-copy")]
    UnverifiableProjection,
    /// The import store behind the copy's links no longer matches the pin.
    ///
    /// Symlink mode links a projected file straight into the import store, so
    /// an ordinary edit through the runtime copy writes the stored bytes the
    /// lockfile pins. The copy's own links are unchanged, which is why the
    /// divergence is discovered by hashing the store against the pin.
    #[serde(rename = "diverged-import-store")]
    DivergedImportStore,
}

impl DriftReason {
    /// Stable identifier used in reports.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DirtyProjection => "dirty-projected-copy",
            Self::UnverifiableProjection => "unverifiable-projected-copy",
            Self::DivergedImportStore => "diverged-import-store",
        }
    }

    /// Parse the stable identifier a report serialized back into a reason.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "dirty-projected-copy" => Some(Self::DirtyProjection),
            "unverifiable-projected-copy" => Some(Self::UnverifiableProjection),
            "diverged-import-store" => Some(Self::DivergedImportStore),
            _ => None,
        }
    }

    /// Human-readable explanation of the reason.
    pub const fn detail(self) -> &'static str {
        match self {
            Self::DirtyProjection => "the generated copy contains edits skillctl did not record",
            Self::UnverifiableProjection => {
                "the generated copy carries no recorded content digest and no longer matches \
                 the planned content"
            }
            Self::DivergedImportStore => {
                "the import store behind this copy no longer hashes to the pinned content"
            }
        }
    }

    /// Actionable repair action for the reason.
    ///
    /// `commands` says which repairs can run for this skill. The `overlay` and
    /// `discard` repairs move an import to a new upstream revision inside one
    /// apply when a revision waits, so the gate refuses them too, and both need
    /// an install record to act on. The gated advice names the fork that takes
    /// local ownership instead, which the gate always allows, and the explicit
    /// `sync --discard`, which regenerates the copy without touching the
    /// import.
    ///
    /// `scope` is the management scope the advice observes. A plain `skillctl
    /// sync` reads workspace scope, so advice for a user-scope copy must name
    /// the qualified command [`scoped_sync_command`] returns.
    pub fn repair(self, skill: &str, commands: RepairCommands, scope: ManagedScope) -> String {
        let sync = scoped_sync_command(scope);
        match (self, commands) {
            // A diverged store is repaired in the store, not in the copy, so
            // the command classification does not apply: every update
            // strategy reads the same diverged store to plan its move, so no
            // strategy may be named. The fork that keeps the edit is the
            // standalone command, because the update apply that carries the
            // fork strategy resolves through the store it would leave behind.
            (Self::DivergedImportStore, _) => resolver::store_divergence_repair(skill),
            (Self::DirtyProjection, RepairCommands::UpdateStrategies) => format!(
                "run 'skillctl update {skill} --apply --strategy overlay' to keep the edits, or \
                 'skillctl update {skill} --apply --strategy discard' to back them up and drop \
                 them"
            ),
            (Self::DirtyProjection, RepairCommands::GatedUpdateStrategies) => format!(
                "run 'skillctl update {skill} --apply --strategy fork' to keep the edits as local \
                 ownership, or '{sync} --discard' to back them up and drop them"
            ),
            (Self::DirtyProjection, RepairCommands::DiscardOnly) => format!(
                "run '{sync} --discard' to back the edits up and drop them, or move them \
                 into a canonical skill and run '{sync}'; no install record holds \
                 '{skill}', so an update cannot repair this copy"
            ),
            (Self::UnverifiableProjection, RepairCommands::UpdateStrategies) => format!(
                "run '{sync} --discard' to back the copy up and regenerate it, or move \
                 the edits into an overlay with 'skillctl update {skill} --apply --strategy \
                 overlay'"
            ),
            (Self::UnverifiableProjection, RepairCommands::GatedUpdateStrategies) => format!(
                "run '{sync} --discard' to back the copy up and regenerate it, or keep \
                 the edits as local ownership with 'skillctl update {skill} --apply --strategy \
                 fork'"
            ),
            (Self::UnverifiableProjection, RepairCommands::DiscardOnly) => format!(
                "run '{sync} --discard' to back the copy up and drop it, or move the \
                 edits into a canonical skill and run '{sync}'; no install record \
                 holds '{skill}', so an update cannot repair this copy"
            ),
        }
    }
}

/// The `sync` command advice that observes `scope` must name.
///
/// A plain `skillctl sync` reads workspace scope, so advice that observes user
/// scope must qualify the command: a plain run cannot prune, replace, or
/// materialize a root only `skillctl --scope user sync` plans.
pub fn scoped_sync_command(scope: ManagedScope) -> &'static str {
    match scope {
        ManagedScope::Workspace => "skillctl sync",
        ManagedScope::User => "skillctl --scope user sync",
    }
}

/// Which repair commands the advice for one drifted copy may name.
///
/// Every `update --apply` strategy that resolves unrecorded edits needs an
/// install record to act on, and the trust gate can refuse such a strategy even
/// when one exists. Advice built on this classification cannot name a command
/// that exits without repairing the copy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepairCommands {
    /// The update strategies can run, and the trust gate allows them.
    UpdateStrategies,
    /// The update strategies can run, but the trust gate refuses them.
    ///
    /// This holds for an unreviewed import the local state cannot clear of a
    /// waiting upstream revision that carries executable content: an installed
    /// `scripts/` tree no recorded review covers, or a revision the newest
    /// recorded check saw introduce one. The gate holds back every apply that
    /// moves such an import upstream, and the repair strategies do. An import
    /// whose state shows no such revision is classified as
    /// [`Self::UpdateStrategies`] instead, because its repair keeps the pinned
    /// revision.
    GatedUpdateStrategies,
    /// No install record holds the skill, so `update` cannot repair the copy.
    ///
    /// This holds for a copy left behind by a removed import and for the
    /// generated copy of a canonical local skill, which skillctl never
    /// installs. Only the explicit discard repairs such a copy.
    DiscardOnly,
}

/// One generated projection that must not be replaced.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ProjectionDrift {
    /// Absolute path of the generated copy.
    pub path: PathBuf,
    /// Why the copy must be preserved.
    pub reason: DriftReason,
}

/// State of one projection directory relative to what skillctl recorded.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProjectionState {
    /// Nothing exists at the path yet.
    Absent,
    /// The directory was not written by skillctl.
    HandAuthored,
    /// The directory matches the content skillctl recorded for it.
    Clean,
    /// The directory holds content skillctl did not record.
    Drift(ProjectionDrift),
}

/// Inspect one projection directory against its recorded content digest.
///
/// `desired_digest` is the digest of the content the caller is about to write.
/// It is only consulted for copies that predate recorded digests.
pub fn inspect_projection(
    dir: &Path,
    desired_digest: Option<&str>,
) -> Result<ProjectionState, AppError> {
    match fs::metadata(dir) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => {
            return Err(AppError::PathConflict {
                path: dir.to_path_buf(),
                expected: "directory",
            });
        }
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ProjectionState::Absent);
        }
        Err(source) => {
            return Err(AppError::FilesystemOperation {
                action: "inspect projected skill directory",
                path: dir.to_path_buf(),
                source,
            });
        }
    }

    match read_projection_content_digest(dir)? {
        Some(recorded) => {
            let observed = hash_projection_directory(dir)?;
            if observed == recorded {
                Ok(ProjectionState::Clean)
            } else {
                Ok(ProjectionState::Drift(ProjectionDrift {
                    path: dir.to_path_buf(),
                    reason: DriftReason::DirtyProjection,
                }))
            }
        }
        None => {
            if !crate::materialize::is_skillctl_generated_directory(dir)? {
                return Ok(ProjectionState::HandAuthored);
            }
            // Copies written before digests were recorded carry no baseline.
            // When the caller knows the desired content they are compared
            // against it; without a desired state there is nothing to compare
            // and no evidence of edits, so the copy is treated as clean.
            match desired_digest {
                Some(desired) => {
                    let observed = hash_projection_directory(dir)?;
                    if observed == desired {
                        Ok(ProjectionState::Clean)
                    } else {
                        Ok(ProjectionState::Drift(ProjectionDrift {
                            path: dir.to_path_buf(),
                            reason: DriftReason::UnverifiableProjection,
                        }))
                    }
                }
                None => Ok(ProjectionState::Clean),
            }
        }
    }
}

/// Read the content digest recorded in one projection metadata file.
///
/// `None` covers both a directory without projection metadata and one whose
/// metadata names no digest, because neither leaves a baseline to compare a
/// copy against.
fn read_projection_content_digest(dir: &Path) -> Result<Option<String>, AppError> {
    Ok(
        crate::materialize::read_projection_metadata(dir)?.and_then(|parsed| {
            parsed
                .get("content_digest")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        }),
    )
}

/// Hash the content of one projection directory, ignoring the metadata file.
///
/// Only files and symlinks contribute, so the digest of a freshly materialized
/// copy equals the digest of the plan that produced it.
pub fn hash_projection_directory(dir: &Path) -> Result<String, AppError> {
    let mut entries = Vec::new();
    collect_projection_entries(dir, Path::new(""), &mut entries)?;
    entries.sort();

    let mut hasher = Sha256::new();
    for relative in entries {
        let full_path = dir.join(&relative);
        let metadata =
            fs::symlink_metadata(&full_path).map_err(|source| AppError::FilesystemOperation {
                action: "inspect projected content for hashing",
                path: full_path.clone(),
                source,
            })?;

        if metadata.file_type().is_symlink() {
            let target =
                fs::read_link(&full_path).map_err(|source| AppError::FilesystemOperation {
                    action: "read projected symlink target",
                    path: full_path,
                    source,
                })?;
            hasher.update(b"symlink\0");
            hasher.update(relative.to_string_lossy().as_bytes());
            hasher.update(b"\0");
            hasher.update(target.to_string_lossy().as_bytes());
            hasher.update(b"\0");
            continue;
        }

        hasher.update(b"file\0");
        hasher.update(relative.to_string_lossy().as_bytes());
        hasher.update(b"\0");
        hasher.update(file_digest(&full_path)?.as_bytes());
        hasher.update(b"\0");
    }

    Ok(format!("sha256:{:x}", hasher.finalize()))
}

/// Digest of the content a winner would materialize into a projection.
///
/// Symlink mode links to the source files, so the digest covers link targets
/// instead of file bytes.
pub fn desired_projection_digest(
    winner: &ResolvedSkillCandidate,
    symlink_mode: bool,
) -> Result<String, AppError> {
    let mut files: Vec<_> = winner
        .files
        .iter()
        .filter(|file| file.relative_path.as_path() != Path::new(PROJECTION_METADATA_FILE))
        .collect();
    files.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));

    let mut hasher = Sha256::new();
    for file in files {
        let relative = file.relative_path.to_string_lossy();
        if symlink_mode {
            hasher.update(b"symlink\0");
            hasher.update(relative.as_bytes());
            hasher.update(b"\0");
            hasher.update(file.source_path.to_string_lossy().as_bytes());
            hasher.update(b"\0");
            continue;
        }

        hasher.update(b"file\0");
        hasher.update(relative.as_bytes());
        hasher.update(b"\0");
        hasher.update(file_digest(&file.source_path)?.as_bytes());
        hasher.update(b"\0");
    }

    Ok(finish_digest(hasher))
}

/// Digest of an in-memory file set a caller is about to materialize.
pub fn desired_projection_digest_for_files<'a>(
    files: impl IntoIterator<Item = (&'a str, &'a [u8])>,
) -> Result<String, AppError> {
    let mut files: Vec<_> = files
        .into_iter()
        .filter(|(path, _)| *path != PROJECTION_METADATA_FILE)
        .collect();
    files.sort_by(|left, right| left.0.cmp(right.0));

    let mut hasher = Sha256::new();
    for (relative, contents) in files {
        hasher.update(b"file\0");
        hasher.update(relative.as_bytes());
        hasher.update(b"\0");
        let mut file_hasher = Sha256::new();
        file_hasher.update(contents);
        hasher.update(format!("{:x}", file_hasher.finalize()).as_bytes());
        hasher.update(b"\0");
    }

    Ok(finish_digest(hasher))
}

fn finish_digest(hasher: Sha256) -> String {
    format!("sha256:{:x}", hasher.finalize())
}

fn file_digest(path: &Path) -> Result<String, AppError> {
    let contents = fs::read(path).map_err(|source| AppError::FilesystemOperation {
        action: "read projected file",
        path: path.to_path_buf(),
        source,
    })?;
    let mut hasher = Sha256::new();
    hasher.update(&contents);
    Ok(format!("{:x}", hasher.finalize()))
}

fn collect_projection_entries(
    root: &Path,
    relative: &Path,
    entries: &mut Vec<PathBuf>,
) -> Result<(), AppError> {
    let current = if relative.as_os_str().is_empty() {
        root.to_path_buf()
    } else {
        root.join(relative)
    };
    let directory_entries = match fs::read_dir(&current) {
        Ok(entries) => entries,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(AppError::FilesystemOperation {
                action: "read projected skill directory",
                path: current,
                source,
            });
        }
    }
    .collect::<Result<Vec<_>, _>>()
    .map_err(|source| AppError::FilesystemOperation {
        action: "read projected skill entry",
        path: current.clone(),
        source,
    })?;

    for entry in directory_entries {
        let name = entry.file_name();
        if relative.as_os_str().is_empty() && name.as_os_str() == PROJECTION_METADATA_FILE {
            continue;
        }
        let next_relative = relative.join(&name);
        let path = entry.path();
        let metadata =
            fs::symlink_metadata(&path).map_err(|source| AppError::FilesystemOperation {
                action: "inspect projected skill path",
                path: path.clone(),
                source,
            })?;
        if metadata.is_dir() {
            collect_projection_entries(root, &next_relative, entries)?;
            continue;
        }
        entries.push(next_relative);
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Shared revision change
// ---------------------------------------------------------------------------

/// Immutable revision an operation moves one import to.
#[derive(Clone, Debug)]
pub(crate) struct RevisionTarget<'a> {
    /// Requested reference recorded in the manifest, when the operation pins one.
    pub manifest_reference: Option<&'a str>,
    /// Immutable revision the import moves to.
    pub resolved_revision: &'a str,
    /// Staged source tree that contains the skill subtree.
    pub source_root: &'a Path,
    /// Discovery entry the index now declares, for a well-known move.
    ///
    /// The entry carries the artifact URL and digest the revision pins, so the
    /// lockfile's discovery block moves with the import it describes.
    pub discovery: Option<crate::lockfile::LockedDiscoveryEntry>,
}

/// Result of moving one import to a new revision.
#[derive(Clone, Debug)]
pub(crate) struct RevisionOutcome {
    /// Revision now recorded in the lockfile.
    pub resolved_revision: String,
    /// Content hash of the imported subtree.
    pub content_hash: String,
    /// Overlay hash after the change.
    pub overlay_hash: String,
    /// Effective version hash after the change.
    pub effective_version_hash: String,
    /// Canonical bundle identity now recorded in the lockfile.
    pub bundle: LockedBundle,
    /// Concise difference between the previous and the new content.
    pub diff: ChangeDiff,
}

/// Move one managed import to a new revision through the shared path.
///
/// This is the single place that rewrites import content, and it always leaves
/// the lockfile with a canonical bundle identity and retains the artifact.
pub(crate) fn apply_import_revision(
    context: &AppContext,
    transaction: &mut LifecycleTransaction,
    manifest: &mut WorkspaceManifest,
    lockfile: &mut WorkspaceLockfile,
    managed_skill: &ManagedSkillRef,
    target: &RevisionTarget<'_>,
) -> Result<RevisionOutcome, AppError> {
    let import_index = manifest
        .imports
        .iter()
        .position(|import| {
            import.id == managed_skill.skill_id
                && import.scope == manifest_scope(managed_skill.scope)
        })
        .ok_or_else(|| AppError::ResolutionValidation {
            message: format!(
                "skill '{}' is not a managed import in the workspace manifest",
                managed_skill.skill_id
            ),
        })?;
    let import_id = manifest.imports[import_index].id.clone();
    let stored_root =
        stored_import_root(managed_skill.scope, &context.working_directory, &import_id)?;
    transaction.track_path(&stored_root)?;

    let lockfile_entry =
        lockfile
            .imports
            .get_mut(&import_id)
            .ok_or_else(|| AppError::ResolutionValidation {
                message: format!(
                    "managed import '{}' is missing from the lockfile",
                    import_id
                ),
            })?;
    let subpath = lockfile_entry.source.subpath.as_str().to_string();

    let new_skill_root = target.source_root.join(&subpath);
    ensure_directory(&new_skill_root, "inspect imported skill")?;
    ensure_overlay_maps_into_tree(
        manifest,
        &context.working_directory,
        &import_id,
        &new_skill_root,
        target.resolved_revision,
    )?;
    let new_bundle = BundleManifest::capture(&new_skill_root)?;
    let content_hash = hash_directory_contents(&new_skill_root)?;
    let overlay_hash =
        crate::history::overlay_hash(manifest, &context.working_directory, &import_id)?;
    let effective_version_hash = crate::source::compute_effective_version_hash(
        target.resolved_revision,
        &content_hash,
        &overlay_hash,
    );

    let previous_root = stored_root.join(&subpath);
    let diff = match fs::metadata(&previous_root) {
        Ok(metadata) if metadata.is_dir() => {
            change_diff(&BundleManifest::capture(&previous_root)?, &new_bundle)
        }
        Ok(_) => {
            return Err(AppError::PathConflict {
                path: previous_root,
                expected: "directory",
            });
        }
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => ChangeDiff {
            added: new_bundle
                .entries
                .iter()
                .filter(|entry| entry.digest.is_some())
                .map(|entry| entry.path.clone())
                .collect(),
            removed: Vec::new(),
            modified: Vec::new(),
        },
        Err(source) => {
            return Err(AppError::FilesystemOperation {
                action: "inspect stored import",
                path: previous_root,
                source,
            });
        }
    };

    if let Some(reference) = target.manifest_reference {
        manifest.imports[import_index].ref_spec = reference.to_string();
    }
    lockfile_entry.revision.resolved = target.resolved_revision.to_string();
    lockfile_entry.revision.upstream = crate::source::observed_upstream_revision(
        target.manifest_reference,
        target.resolved_revision,
    );
    // A well-known import pins the digest its discovery entry declares, so the
    // entry moves with the revision it names. Every other source kind carries
    // no entry, and the lockfile refuses one on those kinds.
    if let Some(entry) = target.discovery.as_ref() {
        lockfile_entry.discovery = Some(entry.clone());
    }
    lockfile_entry.bundle = Some(LockedBundle {
        format_version: new_bundle.format_version,
        digest: new_bundle.digest(),
        files: new_bundle.file_count() as u64,
        bytes: new_bundle.total_bytes(),
    });
    lockfile_entry.hashes.content = content_hash.clone();
    lockfile_entry.hashes.overlay = overlay_hash.clone();
    lockfile_entry.hashes.effective_version = effective_version_hash.clone();

    manifest.write_to_path()?;
    lockfile.write_to_path()?;

    crate::source::store_selected_subtree(target.source_root, &subpath, &stored_root)?;
    ArtifactStore::open_default()?.store(&stored_root.join(&subpath))?;

    Ok(RevisionOutcome {
        resolved_revision: target.resolved_revision.to_string(),
        content_hash,
        overlay_hash,
        effective_version_hash,
        bundle: LockedBundle {
            format_version: new_bundle.format_version,
            digest: new_bundle.digest(),
            files: new_bundle.file_count() as u64,
            bytes: new_bundle.total_bytes(),
        },
        diff,
    })
}

/// Refuse a revision move whose incoming tree deletes an overlay file's base.
///
/// An overlay file only resolves against a base the imported tree provides, so
/// a revision that removes the file makes every later sync fail while the move
/// itself rolls back to a tree where the overlay maps again — a dead end no
/// strategy escapes and no diagnosis explains. The refusal runs before the
/// first write and names the overlay files whose base the revision drops, with
/// the two exits that resolve it.
fn ensure_overlay_maps_into_tree(
    manifest: &WorkspaceManifest,
    working_directory: &Path,
    import_id: &str,
    incoming_root: &Path,
    resolved_revision: &str,
) -> Result<(), AppError> {
    let Some(overlay_path) = manifest.overrides.get(import_id) else {
        return Ok(());
    };
    let overlay_root = working_directory.join(overlay_path.as_str());
    let mut overlay_files = Vec::new();
    match fs::metadata(&overlay_root) {
        Ok(metadata) if metadata.is_dir() => {
            collect_projection_files_for_overlay(&overlay_root, Path::new(""), &mut overlay_files)?;
        }
        Ok(_) => {
            return Err(AppError::PathConflict {
                path: overlay_root,
                expected: "directory",
            });
        }
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(AppError::FilesystemOperation {
                action: "inspect overlay root",
                path: overlay_root,
                source,
            });
        }
    }

    let unmapped: Vec<PathBuf> = overlay_files
        .into_iter()
        .filter(|relative| {
            !fs::metadata(incoming_root.join(relative)).is_ok_and(|metadata| metadata.is_file())
        })
        .collect();
    if unmapped.is_empty() {
        return Ok(());
    }

    Err(AppError::DirtyContent {
        path: overlay_root.join(unmapped.first().cloned().unwrap_or_default()),
        message: format!(
            "the managed overlay edits {}, which revision '{}' no longer contains; delete the \
             overlay file to apply the revision, or run 'skillctl fork {import_id}' to keep the \
             edit as local ownership",
            quoted_relative_names(&unmapped),
            resolved_revision
        ),
    })
}

/// Build a concise diff between two captured trees.
pub fn change_diff(previous: &BundleManifest, next: &BundleManifest) -> ChangeDiff {
    let previous_files: BTreeMap<&str, &crate::bundle::BundleEntry> = previous
        .entries
        .iter()
        .filter(|entry| entry.digest.is_some())
        .map(|entry| (entry.path.as_str(), entry))
        .collect();
    let next_files: BTreeMap<&str, &crate::bundle::BundleEntry> = next
        .entries
        .iter()
        .filter(|entry| entry.digest.is_some())
        .map(|entry| (entry.path.as_str(), entry))
        .collect();

    let mut diff = ChangeDiff::default();
    for (path, entry) in &next_files {
        match previous_files.get(path) {
            None => diff.added.push((*path).to_string()),
            Some(previous_entry) => {
                if previous_entry.digest != entry.digest || previous_entry.size != entry.size {
                    diff.modified.push((*path).to_string());
                }
            }
        }
    }
    for path in previous_files.keys() {
        if !next_files.contains_key(path) {
            diff.removed.push((*path).to_string());
        }
    }

    diff
}

fn ensure_directory(path: &Path, action: &'static str) -> Result<(), AppError> {
    match fs::metadata(path) {
        Ok(metadata) if metadata.is_dir() => Ok(()),
        Ok(_) => Err(AppError::PathConflict {
            path: path.to_path_buf(),
            expected: "directory",
        }),
        Err(source) => Err(AppError::FilesystemOperation {
            action,
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn manifest_scope(scope: ManagedScope) -> ManifestScope {
    match scope {
        ManagedScope::Workspace => ManifestScope::Workspace,
        ManagedScope::User => ManifestScope::User,
    }
}

/// Strategy an update apply uses when local edits block a clean apply.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ApplyStrategy {
    /// Apply only when no unmanaged local edits exist.
    #[default]
    Clean,
    /// Move unmanaged edits into the managed overlay, then apply.
    Overlay,
    /// Back unmanaged edits up durably, then discard them and apply.
    Discard,
    /// Detach into local canonical ownership instead of applying.
    Fork,
    /// Keep the current pin and record the decision.
    Skip,
}

impl ApplyStrategy {
    /// Stable identifier used in CLI, MCP, and plan documents.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Clean => "clean",
            Self::Overlay => "overlay",
            Self::Discard => "discard",
            Self::Fork => "fork",
            Self::Skip => "skip",
        }
    }

    /// Parse a strategy identifier.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "clean" => Some(Self::Clean),
            "overlay" => Some(Self::Overlay),
            "discard" => Some(Self::Discard),
            "fork" => Some(Self::Fork),
            "skip" => Some(Self::Skip),
            _ => None,
        }
    }
}

/// Machine-readable summary of one preserved generated copy.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PreservedCopy {
    /// Display path of the preserved copy.
    pub path: String,
    /// Skill the copy belongs to, when known.
    pub skill: Option<String>,
    /// Why the copy was preserved.
    pub reason: String,
    /// Human-readable explanation.
    pub detail: String,
    /// Actionable repair action.
    pub repair: String,
}

/// Machine-readable summary of one durable pre-discard backup.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DiscardedCopy {
    /// Display path of the discarded copy.
    pub path: String,
    /// Skill the copy belonged to.
    pub skill: String,
    /// Durable backup that holds the discarded content.
    pub backup: String,
}

impl ProjectionDrift {
    /// Build the machine-readable summary for this drift.
    ///
    /// `commands` decides which repair commands the summary names, and `scope`
    /// decides how the `sync` it names is qualified; see
    /// [`DriftReason::repair`].
    pub fn summary(
        &self,
        context: &AppContext,
        skill: Option<&str>,
        commands: RepairCommands,
        scope: ManagedScope,
    ) -> PreservedCopy {
        PreservedCopy {
            path: crate::planner::display_path(context, &self.path),
            skill: skill.map(str::to_string),
            reason: self.reason.as_str().to_string(),
            detail: self.reason.detail().to_string(),
            repair: self
                .reason
                .repair(skill.unwrap_or("<skill>"), commands, scope),
        }
    }
}

// ---------------------------------------------------------------------------
// Update apply
// ---------------------------------------------------------------------------

/// One skill an update apply intends to move to a new revision.
#[derive(Clone, Debug)]
pub struct UpdateApplyTarget {
    /// Managed skill the apply targets.
    pub skill: ManagedSkillRef,
    /// Upstream URL the update check observed for the revision.
    ///
    /// A revision apply checks out this URL, so the apply fetches the source
    /// the check or plan document saw instead of whatever the install record
    /// happens to name now.
    pub source_url: String,
    /// Immutable revision the apply moves to.
    pub target_revision: String,
    /// Pre-state the apply was planned against.
    pub expected: ExpectedImport,
    /// Check plan that produced this target.
    pub summary: crate::planner::SkillUpdatePlan,
}

/// Result of one applied update.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct AppliedUpdate {
    /// Skill that moved to a new revision.
    pub skill: String,
    /// Scope that owns the skill.
    pub scope: ManagedScope,
    /// Revision the skill moved from.
    pub previous_revision: String,
    /// Revision now recorded.
    pub resolved_revision: String,
    /// Effective version hash now recorded.
    pub effective_version_hash: String,
    /// Canonical bundle identity now recorded in the lockfile.
    pub bundle: Option<LockedBundle>,
    /// Whether the retained artifact was created or already present.
    pub artifact: String,
    /// Concise difference between the previous and new content.
    pub diff: ChangeDiff,
    /// How the apply treated local edits.
    pub strategy: String,
    /// Projections written by the apply.
    pub projection: serde_json::Value,
}

/// Reason one skill did not move during an apply.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SkippedUpdate {
    /// Skill that stayed on its current revision.
    pub skill: String,
    /// Scope that owns the skill.
    pub scope: ManagedScope,
    /// Why the skill was skipped.
    pub reason: String,
}

/// Machine-readable result of one update apply run.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct UpdateApplyReport {
    /// Strategy the run used.
    pub strategy: String,
    /// Skills that moved to a new revision, in apply order.
    pub applied: Vec<AppliedUpdate>,
    /// Skills that kept their current revision.
    pub skipped: Vec<SkippedUpdate>,
    /// Local edits preserved as overlay content.
    pub preserved: Vec<PreservedCopy>,
    /// Local edits backed up durably and dropped.
    pub discarded: Vec<DiscardedCopy>,
}

/// Build the immutable plan for one update target.
pub fn build_update_mutation_plan(
    context: &AppContext,
    target: &UpdateApplyTarget,
    strategy: ApplyStrategy,
) -> Result<MutationPlan, AppError> {
    let manifest = WorkspaceManifest::load_from_workspace(&context.working_directory)?;
    let lockfile = load_lockfile(&context.working_directory)?;
    let scoped_context = crate::history::context_for_scope(context, target.skill.scope);

    let mut builder =
        MutationPlanBuilder::new(MutationOperation::Update, &context.working_directory)
            .skill(target.skill.clone())
            .expect_import(target.expected.clone())
            .propose(ProposedImport {
                import_id: target.skill.skill_id.clone(),
                revision: Some(target.target_revision.clone()),
                bundle_digest: None,
                bundle_policy: BundlePolicy::Recompute,
                summary: format!(
                    "apply upstream revision {} with the {} strategy",
                    target.target_revision,
                    strategy.as_str()
                ),
            })
            .target(manifest.path.clone(), TargetAction::Update)
            .target(lockfile.path, TargetAction::Update)
            .targets(
                crate::materialize::sync_mutation_root_paths(
                    &scoped_context,
                    &manifest,
                    crate::adapter::TargetScope::from(target.skill.scope),
                )?,
                TargetAction::Replace,
            );

    if strategy == ApplyStrategy::Discard {
        builder = builder.discard_root(DiscardStore::open_default()?.root().display().to_string());
    }

    Ok(builder.seal())
}

/// Whether one target's apply resolves edits instead of moving the revision.
///
/// A local source tracks no upstream revision to move to, an up-to-date check
/// leaves a git import on the revision an update would move it to, and a
/// commit-pinned git import names no upstream ref an update could observe. All
/// three contribute a target only because their generated copies hold unrecorded
/// edits, so the apply repairs those edits and keeps the pinned source content.
/// A well-known import behaves like the git one: its check moves the pinned
/// digest when the index declares a new one, so only a check that observed
/// nothing to move to contributes an edit repair.
pub fn repairs_unrecorded_edits(target: &UpdateApplyTarget) -> bool {
    match target.summary.source.kind {
        crate::source::SourceKind::LocalPath | crate::source::SourceKind::Archive => true,
        crate::source::SourceKind::Git | crate::source::SourceKind::WellKnown => matches!(
            target.summary.outcome,
            crate::state::UpdateCheckOutcome::UpToDate
                | crate::state::UpdateCheckOutcome::CommitPinned
        ),
    }
}

/// Build the immutable plan for one edit-repair target.
///
/// The repair keeps the pinned revision and only resolves the unrecorded edits
/// in the generated copies of the import.
pub(crate) fn build_edit_repair_mutation_plan(
    context: &AppContext,
    target: &UpdateApplyTarget,
    strategy: ApplyStrategy,
) -> Result<MutationPlan, AppError> {
    let manifest = WorkspaceManifest::load_from_workspace(&context.working_directory)?;
    let lockfile = load_lockfile(&context.working_directory)?;
    let scoped_context = crate::history::context_for_scope(context, target.skill.scope);

    let mut builder =
        MutationPlanBuilder::new(MutationOperation::Update, &context.working_directory)
            .skill(target.skill.clone())
            .expect_import(target.expected.clone())
            .propose(ProposedImport {
                import_id: target.skill.skill_id.clone(),
                revision: None,
                bundle_digest: target.expected.bundle_digest.clone(),
                bundle_policy: BundlePolicy::Preserve,
                summary: format!(
                    "resolve unrecorded edits on pinned revision {} with the {} strategy",
                    target.expected.revision,
                    strategy.as_str()
                ),
            })
            .target(manifest.path.clone(), TargetAction::Update)
            .target(lockfile.path, TargetAction::Update)
            .targets(
                crate::materialize::sync_mutation_root_paths(
                    &scoped_context,
                    &manifest,
                    crate::adapter::TargetScope::from(target.skill.scope),
                )?,
                TargetAction::Replace,
            );

    if strategy == ApplyStrategy::Discard {
        builder = builder.discard_root(DiscardStore::open_default()?.root().display().to_string());
    }

    Ok(builder.seal())
}

/// Refuse an apply the trust gate blocks for one target.
///
/// `fork` and `skip` stay available because they are the documented ways past
/// the gate: a fork takes local ownership of the current content, and a skip
/// keeps the current pin. Every other strategy moves the import to a new
/// upstream revision, which is exactly what the gate holds back.
fn ensure_trust_gate_allows(
    target: &UpdateApplyTarget,
    strategy: ApplyStrategy,
) -> Result<(), AppError> {
    if matches!(strategy, ApplyStrategy::Fork | ApplyStrategy::Skip) {
        return Ok(());
    }

    let Some(trust) = target.summary.trust.as_ref() else {
        return Ok(());
    };
    if trust.blocks(crate::trust::TrustBlockedAction::ApplyUpdate) {
        return Err(AppError::TrustGateBlocked {
            skill: target.skill.skill_id.clone(),
            operation: crate::error::TrustGateOperation::UpdateApply,
            detail: incoming_scripts_detail(trust.contains_scripts, &target.target_revision),
        });
    }

    Ok(())
}

/// Refusal clause for a block the installed content does not explain.
///
/// A blocked decision that reports no installed `scripts/` tree can only come
/// from the revision the apply would move to, so the refusal names that
/// revision instead of leaving the reader to guess what was gated.
fn incoming_scripts_detail(installed_contains_scripts: bool, revision: &str) -> Option<String> {
    (!installed_contains_scripts).then(|| {
        format!(
            "; the incoming revision {revision} adds a top-level scripts/ directory no recorded \
             review covers"
        )
    })
}

/// Refuse a revision move the trust gate blocks for the content it installs.
///
/// [`ensure_trust_gate_allows`] enforces the decision the update check
/// recorded, and a plan document rebuilds that decision from the install state
/// alone. Both describe content the workspace already holds, while a revision
/// move joins that content with a tree the workspace has never seen, so the
/// engine derives the gate from the two trees itself. `pin` and `rollback`
/// install the same tree an update apply does, so they pass their operation
/// through here and the three commands cannot diverge. The refusal runs inside
/// the transaction and before the first write, so a blocked move leaves the
/// import exactly as it is.
///
/// `install.skill` identifies the import everywhere: every caller resolves the
/// record through the same managed skill it names.
pub(crate) fn ensure_revision_apply_is_allowed(
    context: &AppContext,
    store: &LocalStateStore,
    lockfile: &WorkspaceLockfile,
    install: &InstallRecord,
    target_revision: &str,
    source_root: &Path,
    operation: crate::error::TrustGateOperation,
) -> Result<(), AppError> {
    if target_revision == install.resolved_revision {
        return Ok(());
    }

    let skill = &install.skill;
    // The lockfile entry names the subpath the apply installs, so the gate
    // evaluates the same tree. A missing entry is refused by the apply itself.
    let subpath = lockfile.imports.get(&skill.skill_id).map_or_else(
        || install.source_subpath.clone(),
        |entry| entry.source.subpath.as_str().to_string(),
    );

    let reviews = crate::trust::ReviewIndex::load(store)?;
    let trust = crate::trust::trust_for_revision_apply(
        &context.working_directory,
        install,
        &subpath,
        Some(source_root),
        reviews.approved_for_install(install),
    )?;
    if !trust.gates_update_apply(skill.skill_id.as_str()) {
        return Ok(());
    }

    // The stored import carries the scripts the check-time gate already
    // reported, so a refusal that survives to here is about the revision the
    // move would install.
    let stored_root = crate::source::stored_import_root(
        skill.scope,
        &context.working_directory,
        &skill.skill_id,
    )?
    .join(&subpath);
    let detail = incoming_scripts_detail(
        crate::trust::directory_contains_scripts(&stored_root)?,
        target_revision,
    );

    Err(AppError::TrustGateBlocked {
        skill: skill.skill_id.clone(),
        operation,
        detail,
    })
}

/// Apply a set of update targets through the shared guarded engine.
///
/// Each target runs in its own guarded transaction, so a failure or an
/// interruption leaves that skill untouched and earlier skills applied.
pub fn run_update_apply(
    context: &AppContext,
    targets: Vec<UpdateApplyTarget>,
    strategy: ApplyStrategy,
) -> Result<UpdateApplyReport, AppError> {
    let mut report = UpdateApplyReport {
        strategy: strategy.as_str().to_string(),
        ..UpdateApplyReport::default()
    };
    if targets.is_empty() {
        return Ok(report);
    }

    // The trust gate is enforced by the engine instead of by its callers so
    // that every apply path, including a plan document, shares it. The check
    // runs before the first target changes anything.
    for target in &targets {
        ensure_trust_gate_allows(target, strategy)?;
    }

    // Fork detaches into local canonical ownership, which is its own guarded
    // mutation, so it runs instead of a revision apply. The fork moves the
    // unrecorded edits of every generated copy into its overlay and merges
    // them into the local canonical copy inside that one transaction, so a
    // fork that fails leaves no declared overlay behind.
    if strategy == ApplyStrategy::Fork {
        for target in targets {
            let (response, preservation) = crate::overlay::run_fork(
                context,
                crate::overlay::ForkRequest::new(target.skill.skill_id.clone()),
            )?;
            for file in &preservation.preserved {
                report.preserved.push(PreservedCopy {
                    path: format!("{}/{file}", preservation.overlay_root),
                    skill: Some(target.skill.skill_id.clone()),
                    reason: "moved-to-overlay".to_string(),
                    detail: OVERLAY_PRESERVED_DETAIL.to_string(),
                    repair: overlay_preserved_repair(target.skill.scope),
                });
            }
            report.discarded.extend(preservation.replaced);
            report.applied.push(AppliedUpdate {
                skill: target.skill.skill_id.clone(),
                scope: target.skill.scope,
                previous_revision: target.expected.revision.clone(),
                resolved_revision: target.expected.revision.clone(),
                effective_version_hash: target.expected.effective_version_hash.clone(),
                bundle: None,
                artifact: "released".to_string(),
                diff: ChangeDiff::default(),
                strategy: strategy.as_str().to_string(),
                projection: response.data,
            });
        }
        return Ok(report);
    }

    if strategy == ApplyStrategy::Skip {
        for target in targets {
            report.skipped.push(SkippedUpdate {
                skill: target.skill.skill_id.clone(),
                scope: target.skill.scope,
                reason: "apply strategy was skip".to_string(),
            });
        }
        return Ok(report);
    }

    let discard_store = if strategy == ApplyStrategy::Discard {
        Some(DiscardStore::open_default()?)
    } else {
        None
    };

    for target in &targets {
        if repairs_unrecorded_edits(target) {
            apply_edit_repair(context, target, strategy, &mut report)?;
            continue;
        }

        let plan = build_update_mutation_plan(context, target, strategy)?;
        let timestamp = current_timestamp();

        // The staged revision is what the trust gate evaluates, so it is
        // acquired before the guarded transaction opens and the gate decides
        // on the tree the apply would install. Staging clones the git source
        // or fetches the discovery artifact over the network, so it must not
        // run while the coordination lock is held: a download that outlasts
        // the lock timeout would fail every other command while this one is
        // healthy. The guarded closure re-reads the install record under the
        // lock, and the plan verification re-checks the pre-state, so a
        // workspace that moved underneath the staged tree refuses before
        // anything changes.
        let staging_store = LocalStateStore::open_default_for(&context.working_directory)?;
        let staging_install = staging_store
            .install_record(&target.skill)?
            .ok_or_else(|| AppError::ResolutionValidation {
                message: format!(
                    "skill '{}' does not have an installed state record",
                    target.skill.skill_id
                ),
            })?;
        ensure_update_supported(&staging_install, &target.target_revision)?;
        let staged = stage_incoming_revision(
            &staging_install,
            &target.source_url,
            &target.target_revision,
        )?;

        let mut outcome_report: Option<AppliedUpdate> = None;

        run_guarded(&plan, context, |transaction| {
            let mut manifest = WorkspaceManifest::load_from_workspace(&context.working_directory)?;
            let mut lockfile = load_lockfile(&context.working_directory)?;
            let mut store = LocalStateStore::open_default_for(&context.working_directory)?;
            transaction.track_state_database()?;

            let scoped_context = crate::history::context_for_scope(context, target.skill.scope);
            let install = store.install_record(&target.skill)?.ok_or_else(|| {
                AppError::ResolutionValidation {
                    message: format!(
                        "skill '{}' does not have an installed state record",
                        target.skill.skill_id
                    ),
                }
            })?;
            ensure_update_supported(&install, &target.target_revision)?;

            // The staged revision was acquired before the transaction opened,
            // so the gate decides on the tree the apply would install and the
            // download never ran under the coordination lock.
            ensure_revision_apply_is_allowed(
                context,
                &store,
                &lockfile,
                &install,
                &target.target_revision,
                staged.root(),
                crate::error::TrustGateOperation::UpdateApply,
            )?;

            let dirty = dirty_projection_copies(context, &manifest, &target.skill)?;
            match strategy {
                ApplyStrategy::Clean => {
                    if let Some(drift) = dirty.first() {
                        return Err(AppError::DirtyContent {
                            path: drift.path.clone(),
                            message: format!(
                                "a clean apply refuses to replace it; {}",
                                // The gate was cleared for this target before the
                                // transaction opened, so the ungated strategies
                                // named here are the ones that work.
                                drift.reason.repair(
                                    &target.skill.skill_id,
                                    RepairCommands::UpdateStrategies,
                                    target.skill.scope,
                                )
                            ),
                        });
                    }
                }
                ApplyStrategy::Overlay => {
                    for drift in &dirty {
                        let preservation = preserve_edits_as_overlay(
                            context,
                            transaction,
                            &mut manifest,
                            &target.skill,
                            &drift.path,
                            OverlayPurpose::Managed,
                        )?;
                        // The edits now live in the overlay, so the generated
                        // copy is rebuilt from the new effective content. The
                        // journal holds the pre-state of the root, so a failed
                        // apply restores the edited copy.
                        remove_directory(&drift.path, "remove overlaid generated copy")?;
                        let overlay_root = manifest
                            .overlay_path_for(&target.skill.skill_id)
                            .as_str()
                            .to_owned();
                        for file in &preservation.preserved {
                            report.preserved.push(PreservedCopy {
                                path: format!("{overlay_root}/{file}"),
                                skill: Some(target.skill.skill_id.clone()),
                                reason: "moved-to-overlay".to_string(),
                                detail: OVERLAY_PRESERVED_DETAIL.to_string(),
                                repair: overlay_preserved_repair(target.skill.scope),
                            });
                        }
                        report.discarded.extend(preservation.replaced);
                    }
                }
                ApplyStrategy::Discard | ApplyStrategy::Fork | ApplyStrategy::Skip => {}
            }

            let previous_revision = install.resolved_revision.clone();
            let outcome = apply_import_revision(
                context,
                transaction,
                &mut manifest,
                &mut lockfile,
                &target.skill,
                &RevisionTarget {
                    manifest_reference: staged.manifest_reference().as_deref(),
                    resolved_revision: &target.target_revision,
                    source_root: staged.root(),
                    discovery: staged.discovery(),
                },
            )?;

            // A well-known import pins the digest its index declares, which
            // names no upstream ref, so the move observes nothing upstream —
            // the same rule a move to an immutable reference follows.
            let upstream_revision = match install.source_kind {
                crate::source::SourceKind::WellKnown => None,
                _ => Some(outcome.resolved_revision.clone()),
            };
            let updated_install = InstallRecord {
                resolved_revision: outcome.resolved_revision.clone(),
                upstream_revision,
                content_hash: outcome.content_hash.clone(),
                overlay_hash: outcome.overlay_hash.clone(),
                effective_version_hash: outcome.effective_version_hash.clone(),
                updated_at: timestamp.clone(),
                detached: false,
                forked: false,
                ..install
            };
            store.upsert_install_record(&updated_install)?;

            // The pin tracks the revision the import holds, so the apply moves
            // it the way `pin` and `rollback` move it. A pin left on the
            // previous revision stops matching the install, which drops the
            // tracked reference from the next update check and reports a
            // revision the install no longer holds.
            if let Some(mut pin_record) = store.pin_record(&target.skill)? {
                pin_record.resolved_revision = outcome.resolved_revision.clone();
                pin_record.effective_version_hash = Some(outcome.effective_version_hash.clone());
                store.upsert_pin_record(&pin_record)?;
            }

            let sync_report = crate::materialize::sync_workspace_with_policy(
                &scoped_context,
                &drift_policy_for_update(strategy, discard_store.as_ref(), &target.skill.skill_id),
            )?;
            collect_projection_drift(&sync_report, &mut report);

            let discarded_for_skill: Vec<DiscardedCopy> = report
                .discarded
                .iter()
                .filter(|discarded| discarded.skill == target.skill.skill_id)
                .cloned()
                .collect();
            let applied_install = updated_install.clone();
            let applied_skill = target.skill.clone();
            crate::materialize::refresh_projection_state_for_scope(
                &mut store,
                context,
                target.skill.scope,
                &sync_report,
                &timestamp,
                |ledger| {
                    ledger.record_update_applied(&previous_revision, &applied_install)?;
                    for discarded in &discarded_for_skill {
                        ledger.record_cleanup(
                            Some(&applied_skill),
                            &format!(
                                "{} (discarded local edits backed up at {})",
                                discarded.path, discarded.backup
                            ),
                            &timestamp,
                        )?;
                    }
                    Ok(())
                },
            )?;

            outcome_report = Some(AppliedUpdate {
                skill: target.skill.skill_id.clone(),
                scope: target.skill.scope,
                previous_revision,
                resolved_revision: outcome.resolved_revision.clone(),
                effective_version_hash: outcome.effective_version_hash.clone(),
                bundle: Some(outcome.bundle.clone()),
                artifact: "retained".to_string(),
                diff: outcome.diff,
                strategy: strategy.as_str().to_string(),
                projection: serde_json::to_value(&sync_report)?,
            });
            transaction.checkpoint("applied-skill")?;

            Ok(())
        })?;

        if let Some(applied) = outcome_report {
            report.applied.push(applied);
        }
    }

    Ok(report)
}

/// Drift policy an update apply passes to projection materialization.
///
/// Only the skill being applied may lose its unrecorded edits; copies of other
/// skills that share the root stay preserved.
fn drift_policy_for_update(
    strategy: ApplyStrategy,
    store: Option<&DiscardStore>,
    skill_id: &str,
) -> crate::materialize::DriftPolicy {
    crate::materialize::DriftPolicy {
        discard: strategy == ApplyStrategy::Discard,
        store: store.cloned(),
        discard_filter: (strategy == ApplyStrategy::Discard).then(|| skill_id.to_string()),
    }
}

/// Resolve the unrecorded edits of one import whose revision does not move.
///
/// A local source tracks no upstream revision, and an up-to-date check leaves a
/// git import on the revision it pins, so an apply can only repair the edits in
/// its generated copies: `overlay` moves them into the managed overlay,
/// `discard` backs them up and drops them, and `clean` refuses to guess. The
/// pinned source content and its canonical bundle stay untouched, so the
/// recorded bundle identity is preserved.
fn apply_edit_repair(
    context: &AppContext,
    target: &UpdateApplyTarget,
    strategy: ApplyStrategy,
    report: &mut UpdateApplyReport,
) -> Result<(), AppError> {
    let plan = build_edit_repair_mutation_plan(context, target, strategy)?;
    let timestamp = current_timestamp();
    let discard_store = if strategy == ApplyStrategy::Discard {
        Some(DiscardStore::open_default()?)
    } else {
        None
    };
    let mut outcome_report: Option<AppliedUpdate> = None;
    let mut skipped: Option<SkippedUpdate> = None;
    let mut overlay_created: Option<String> = None;

    run_guarded(&plan, context, |transaction| {
        let mut manifest = WorkspaceManifest::load_from_workspace(&context.working_directory)?;
        let mut lockfile = load_lockfile(&context.working_directory)?;
        let mut store = LocalStateStore::open_default_for(&context.working_directory)?;
        transaction.track_state_database()?;

        let scoped_context = crate::history::context_for_scope(context, target.skill.scope);
        let install =
            store
                .install_record(&target.skill)?
                .ok_or_else(|| AppError::ResolutionValidation {
                    message: format!(
                        "skill '{}' does not have an installed state record",
                        target.skill.skill_id
                    ),
                })?;
        ensure_edit_repair_supported(target, &install)?;

        let dirty = dirty_projection_copies(context, &manifest, &target.skill)?;
        let Some(first_drift) = dirty.first().cloned() else {
            skipped = Some(SkippedUpdate {
                skill: target.skill.skill_id.clone(),
                scope: target.skill.scope,
                reason: format!(
                    "no generated copy of '{}' still holds unrecorded edits; '{}' backs up and \
                     resolves any copy this repair cannot see",
                    target.skill.skill_id,
                    scoped_sync_command(target.skill.scope)
                ),
            });
            return Ok(());
        };

        match strategy {
            ApplyStrategy::Clean => {
                // The repair keeps the pinned revision, so the trust gate, which
                // holds back applies that move an import upstream, never refuses
                // it: a local source tracks no upstream revision, and an
                // up-to-date check leaves a git import where it is.
                return Err(AppError::DirtyContent {
                    path: first_drift.path.clone(),
                    message: format!(
                        "'{}' keeps its pinned revision under this apply, so a clean \
                         strategy cannot refresh the copy; {}",
                        target.skill.skill_id,
                        first_drift.reason.repair(
                            &target.skill.skill_id,
                            RepairCommands::UpdateStrategies,
                            target.skill.scope,
                        )
                    ),
                });
            }
            ApplyStrategy::Overlay => {
                for drift in &dirty {
                    let preservation = preserve_edits_as_overlay(
                        context,
                        transaction,
                        &mut manifest,
                        &target.skill,
                        &drift.path,
                        OverlayPurpose::Managed,
                    )?;
                    // The edits now live in the overlay, so the generated copy
                    // is rebuilt from the new effective content.
                    remove_directory(&drift.path, "remove overlaid generated copy")?;
                    let overlay_root = manifest
                        .overlay_path_for(&target.skill.skill_id)
                        .as_str()
                        .to_owned();
                    for file in &preservation.preserved {
                        report.preserved.push(PreservedCopy {
                            path: format!("{overlay_root}/{file}"),
                            skill: Some(target.skill.skill_id.clone()),
                            reason: "moved-to-overlay".to_string(),
                            detail: OVERLAY_PRESERVED_DETAIL.to_string(),
                            repair: overlay_preserved_repair(target.skill.scope),
                        });
                    }
                    report.discarded.extend(preservation.replaced);
                    if !preservation.preserved.is_empty() {
                        overlay_created = Some(overlay_root);
                    }
                }
            }
            ApplyStrategy::Discard | ApplyStrategy::Fork | ApplyStrategy::Skip => {}
        }

        // The revision does not move, so the overlay, when one was created, is
        // the change the ledger must explain.
        if let Some(overlay_root) = overlay_created.as_ref() {
            let mut ledger = crate::history::HistoryLedger::new(&mut store);
            ledger.record_overlay_created(&target.skill, overlay_root, &timestamp)?;
        }

        // Moving edits into the overlay changes the effective version even
        // though the pinned source content does not move, so the hashes that
        // depend on the overlay are recomputed.
        let overlay_hash = crate::history::overlay_hash(
            &manifest,
            &context.working_directory,
            &target.skill.skill_id,
        )?;
        let effective_version_hash = crate::source::compute_effective_version_hash(
            &install.resolved_revision,
            &install.content_hash,
            &overlay_hash,
        );
        let entry = lockfile
            .imports
            .get_mut(&target.skill.skill_id)
            .ok_or_else(|| AppError::ResolutionValidation {
                message: format!(
                    "managed import '{}' is missing from the lockfile",
                    target.skill.skill_id
                ),
            })?;
        entry.hashes.overlay = overlay_hash.clone();
        entry.hashes.effective_version = effective_version_hash.clone();
        lockfile.write_to_path()?;
        manifest.write_to_path()?;

        let updated_install = InstallRecord {
            overlay_hash,
            effective_version_hash: effective_version_hash.clone(),
            updated_at: timestamp.clone(),
            ..install
        };
        store.upsert_install_record(&updated_install)?;
        if let Some(mut pin_record) = store.pin_record(&target.skill)? {
            pin_record.effective_version_hash = Some(effective_version_hash.clone());
            store.upsert_pin_record(&pin_record)?;
        }

        let sync_report = crate::materialize::sync_workspace_with_policy(
            &scoped_context,
            &drift_policy_for_update(strategy, discard_store.as_ref(), &target.skill.skill_id),
        )?;
        collect_projection_drift(&sync_report, report);

        let discarded_for_skill: Vec<DiscardedCopy> = report
            .discarded
            .iter()
            .filter(|discarded| discarded.skill == target.skill.skill_id)
            .cloned()
            .collect();
        let applied_install = updated_install.clone();
        let applied_skill = target.skill.clone();
        crate::materialize::refresh_projection_state_for_scope(
            &mut store,
            context,
            target.skill.scope,
            &sync_report,
            &timestamp,
            |ledger| {
                ledger
                    .record_update_applied(&applied_install.resolved_revision, &applied_install)?;
                for discarded in &discarded_for_skill {
                    ledger.record_cleanup(
                        Some(&applied_skill),
                        &format!(
                            "{} (discarded local edits backed up at {})",
                            discarded.path, discarded.backup
                        ),
                        &timestamp,
                    )?;
                }
                Ok(())
            },
        )?;

        outcome_report = Some(AppliedUpdate {
            skill: target.skill.skill_id.clone(),
            scope: target.skill.scope,
            previous_revision: updated_install.resolved_revision.clone(),
            resolved_revision: updated_install.resolved_revision.clone(),
            effective_version_hash,
            bundle: lockfile
                .imports
                .get(&target.skill.skill_id)
                .and_then(|entry| entry.bundle.clone()),
            artifact: "preserved".to_string(),
            diff: ChangeDiff::default(),
            strategy: strategy.as_str().to_string(),
            projection: serde_json::to_value(&sync_report)?,
        });
        transaction.checkpoint("applied-skill")?;

        Ok(())
    })?;

    if let Some(skipped) = skipped {
        report.skipped.push(skipped);
    }
    if let Some(applied) = outcome_report {
        report.applied.push(applied);
    }
    Ok(())
}

/// Reject an edit repair that the recorded state does not support.
///
/// A local source tracks no upstream revision, and a git or well-known import
/// reaches the repair only when its check observed nothing to move to — an
/// up-to-date check or a commit pin — so the revision the record pins is the one
/// the repair keeps.
fn ensure_edit_repair_supported(
    target: &UpdateApplyTarget,
    install: &InstallRecord,
) -> Result<(), AppError> {
    if install.detached || install.forked {
        return Err(AppError::ResolutionValidation {
            message: format!(
                "skill '{}' is detached from upstream lifecycle management",
                install.skill.skill_id
            ),
        });
    }
    match (install.source_kind, target.summary.outcome) {
        (crate::source::SourceKind::LocalPath | crate::source::SourceKind::Archive, _) => Ok(()),
        (
            crate::source::SourceKind::Git | crate::source::SourceKind::WellKnown,
            crate::state::UpdateCheckOutcome::UpToDate
            | crate::state::UpdateCheckOutcome::CommitPinned,
        ) => Ok(()),
        (crate::source::SourceKind::Git | crate::source::SourceKind::WellKnown, _) => {
            Err(AppError::ResolutionValidation {
                message: format!(
                    "skill '{}' tracks an upstream source, so it is applied as a revision update",
                    install.skill.skill_id
                ),
            })
        }
    }
}

/// Edits a fork ferried into the managed overlay before it detached.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ForkEditPreservation {
    /// Workspace-relative overlay root the edits moved through.
    pub(crate) overlay_root: String,
    /// Overlay-relative paths whose edits moved into the overlay.
    pub(crate) preserved: Vec<String>,
    /// Overlay variants a different edit replaced, backed up durably.
    pub(crate) replaced: Vec<DiscardedCopy>,
}

/// Move the unrecorded edits of every generated copy of one skill into the
/// overlay its fork is about to merge.
///
/// Both fork entries — `skillctl fork <skill>` and `update --apply --strategy
/// fork` — call this from inside the guarded transaction that performs the
/// fork, so one conflict-aware move, one deletion refusal, and one commit
/// boundary serve them. The overlay writes ride the fork's own transaction, so
/// a fork that fails later rolls them back instead of leaving a declared
/// overlay the resolver cannot map.
pub(crate) fn preserve_edits_for_fork(
    context: &AppContext,
    transaction: &mut LifecycleTransaction,
    manifest: &mut WorkspaceManifest,
    skill: &ManagedSkillRef,
) -> Result<ForkEditPreservation, AppError> {
    let mut preservation = ForkEditPreservation::default();
    // A fork installs exactly the stored bytes it finds, and the divergence
    // repair names the standalone fork as the way to keep changed bytes, so
    // this scan reports on the store as it is instead of refusing on it.
    for drift in dirty_projection_copies_with_divergence(
        context,
        manifest,
        skill,
        resolver::StoreDivergencePolicy::Report,
    )? {
        let moved = preserve_edits_as_overlay(
            context,
            transaction,
            manifest,
            skill,
            &drift.path,
            OverlayPurpose::Detach,
        )?;
        preservation.preserved.extend(moved.preserved);
        preservation.replaced.extend(moved.replaced);
    }
    preservation.overlay_root = manifest
        .overlay_path_for(&skill.skill_id)
        .as_str()
        .to_owned();
    Ok(preservation)
}

/// Copy one sync report's drift outcome into an apply report.
fn collect_projection_drift(
    sync_report: &crate::materialize::MaterializationReport,
    report: &mut UpdateApplyReport,
) {
    for preserved in &sync_report.preserved {
        report.preserved.push(PreservedCopy {
            path: preserved.path.clone(),
            skill: Some(preserved.skill.clone()),
            reason: preserved.reason.clone(),
            detail: preserved.detail.clone(),
            repair: preserved.repair.clone(),
        });
    }
    for discarded in &sync_report.discarded {
        report.discarded.push(DiscardedCopy {
            path: discarded.path.clone(),
            skill: discarded.skill.clone(),
            backup: discarded.backup.clone(),
        });
    }
}

/// One staged incoming revision an apply moves an import to.
///
/// The staged tree stays alive for the whole apply, because it is both the
/// tree the trust gate evaluates and the tree the apply installs.
enum StagedRevision {
    /// A git checkout of the revision the tracked reference names.
    Git(crate::history::CheckedOutSource),
    /// The verified discovery artifact the index still declares.
    WellKnown(crate::source::StagedWellKnownRevision),
}

impl StagedRevision {
    /// The staged tree the gate evaluates and the apply installs.
    fn root(&self) -> &Path {
        match self {
            Self::Git(checked_out) => &checked_out.root,
            Self::WellKnown(staged) => &staged.root,
        }
    }

    /// Reference the manifest records for the move, when one applies.
    fn manifest_reference(&self) -> Option<String> {
        match self {
            Self::Git(_) => None,
            Self::WellKnown(staged) => Some(staged.manifest_reference.clone()),
        }
    }

    /// Discovery entry a well-known move pins in the lockfile.
    fn discovery(&self) -> Option<crate::lockfile::LockedDiscoveryEntry> {
        match self {
            Self::Git(_) => None,
            Self::WellKnown(staged) => Some(staged.discovery.clone()),
        }
    }
}

/// Stage the incoming revision one apply target moves an import to.
///
/// A git import checks out the revision its remote names, from the URL the
/// update check observed (`observed_source_url`). A well-known import re-reads
/// the discovery index, requires the entry it now declares to still name the
/// digest the check saw, and stages the artifact that digest verifies, so an
/// index that moved again refuses the apply instead of installing content the
/// check never saw.
fn stage_incoming_revision(
    install: &InstallRecord,
    observed_source_url: &str,
    target_revision: &str,
) -> Result<StagedRevision, AppError> {
    match install.source_kind {
        crate::source::SourceKind::Git => Ok(StagedRevision::Git(
            crate::history::checkout_git_revision(observed_source_url, target_revision)?,
        )),
        crate::source::SourceKind::WellKnown => Ok(StagedRevision::WellKnown(
            crate::source::stage_well_known_revision(install, target_revision)?,
        )),
        crate::source::SourceKind::LocalPath | crate::source::SourceKind::Archive => {
            Err(AppError::ResolutionValidation {
                message: format!(
                    "skill '{}' only supports apply for git and well-known imports",
                    install.skill.skill_id
                ),
            })
        }
    }
}

/// Reject an apply that the recorded state does not support.
fn ensure_update_supported(install: &InstallRecord, target_revision: &str) -> Result<(), AppError> {
    if install.detached || install.forked {
        return Err(AppError::ResolutionValidation {
            message: format!(
                "skill '{}' is detached from upstream lifecycle management",
                install.skill.skill_id
            ),
        });
    }
    if !matches!(
        install.source_kind,
        crate::source::SourceKind::Git | crate::source::SourceKind::WellKnown
    ) {
        return Err(AppError::ResolutionValidation {
            message: format!(
                "skill '{}' only supports apply for git and well-known imports",
                install.skill.skill_id
            ),
        });
    }
    if install.resolved_revision == target_revision {
        return Err(AppError::ResolutionValidation {
            message: format!(
                "skill '{}' is already at revision '{}'",
                install.skill.skill_id, target_revision
            ),
        });
    }
    Ok(())
}

/// Explanation used when an edit moves into the managed overlay.
const OVERLAY_PRESERVED_DETAIL: &str =
    "the edit now lives in the managed overlay and survives the update";
/// Repair hint used when an edit moves into the managed overlay.
///
/// The hint names the `sync` command qualified for the scope the apply
/// targets, because a plain run cannot rebuild a root another scope plans.
fn overlay_preserved_repair(scope: ManagedScope) -> String {
    format!(
        "edit the file under the overlay root and run '{}'",
        scoped_sync_command(scope)
    )
}

/// Remove one directory together with everything below it.
fn remove_directory(path: &Path, action: &'static str) -> Result<(), AppError> {
    match fs::metadata(path) {
        Ok(metadata) if metadata.is_dir() => {
            fs::remove_dir_all(path).map_err(|source| AppError::FilesystemOperation {
                action,
                path: path.to_path_buf(),
                source,
            })
        }
        Ok(_) => Err(AppError::PathConflict {
            path: path.to_path_buf(),
            expected: "directory",
        }),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(AppError::FilesystemOperation {
            action: "inspect generated copy",
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// Return every generated copy of one skill that holds unrecorded edits.
///
/// The scan covers every root a sync of the scope may change, not only the
/// planned ones, and it reads the content digest each copy records, exactly as
/// the sync prune pass does. A copy that predates recorded digests is judged
/// against the content the resolver produces, which is the same baseline the
/// update check compares it against. Anything less would let the check report a
/// copy the repair cannot resolve, which turns the advertised command into a
/// no-op.
fn dirty_projection_copies(
    context: &AppContext,
    manifest: &WorkspaceManifest,
    skill: &ManagedSkillRef,
) -> Result<Vec<ProjectionDrift>, AppError> {
    dirty_projection_copies_with_divergence(
        context,
        manifest,
        skill,
        crate::materialize::store_divergence_policy(manifest),
    )
}

/// [`dirty_projection_copies`] with an explicit store-divergence policy.
///
/// A fork passes [`resolver::StoreDivergencePolicy::Report`], because it
/// installs exactly the stored bytes it finds and the divergence repair names
/// the fork as the way to keep them. Every other caller keeps the mode-derived
/// policy, which refuses a diverged copy-mode store before an update touches
/// it.
fn dirty_projection_copies_with_divergence(
    context: &AppContext,
    manifest: &WorkspaceManifest,
    skill: &ManagedSkillRef,
    divergence: resolver::StoreDivergencePolicy,
) -> Result<Vec<ProjectionDrift>, AppError> {
    let scoped_context = crate::history::context_for_scope(context, skill.scope);
    let roots = crate::materialize::edit_repair_root_paths(
        &scoped_context,
        manifest,
        crate::adapter::TargetScope::from(skill.scope),
    )?;
    let desired = desired_copy_digest_with_divergence(context, manifest, skill, divergence)?;

    let mut drift = Vec::new();
    for root in roots {
        let dir = root.join(&skill.skill_id);
        // The bundled skill follows its own projection plan, which covers every
        // documented user root, so its copies are never this import's output
        // even where the import carries the same name.
        if crate::materialize::is_bundled_projection_directory(&dir)? {
            continue;
        }
        if let ProjectionState::Drift(found) = inspect_projection(&dir, desired.as_deref())? {
            drift.push(found);
        }
    }
    Ok(drift)
}

/// Content digest the next materialization of one import records.
///
/// The repair runs while the import sits on the revision its copies were
/// written from, so this digest is the baseline those copies were written
/// against: a copy that differs from it holds edits, and a copy that matches it
/// holds none. The digest follows the projection mode, because a symlink
/// projection hashes its link targets instead of its content. A disabled import
/// resolves to no candidate, so its copies carry no baseline and stay judged by
/// the digest they record alone.
/// Content digest the next materialization of one import records.
fn desired_copy_digest_with_divergence(
    context: &AppContext,
    manifest: &WorkspaceManifest,
    skill: &ManagedSkillRef,
    divergence: resolver::StoreDivergencePolicy,
) -> Result<Option<String>, AppError> {
    let lockfile = load_lockfile(&context.working_directory)?;
    let request = resolver::ResolveWorkspaceRequest::new(
        &context.working_directory,
        manifest.clone(),
        lockfile,
    )
    .with_store_divergence_policy(divergence);
    let graph = resolver::build_effective_skill_graph(&request)?;
    let scope = match skill.scope {
        ManagedScope::Workspace => SkillScope::Workspace,
        ManagedScope::User => SkillScope::User,
    };
    let candidate = graph.candidates.into_iter().find(|candidate| {
        candidate
            .import
            .as_ref()
            .is_some_and(|import| import.id == skill.skill_id && import.scope == scope)
    });

    candidate
        .map(|candidate| {
            desired_projection_digest(
                &candidate,
                manifest.projection.mode == ProjectionMode::Symlink,
            )
        })
        .transpose()
}

/// Why unrecorded edits move into the managed overlay.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OverlayPurpose {
    /// The import stays managed after the move, so a sync resolves the overlay
    /// inside the same transaction: every preserved file must map to an
    /// imported file, because the resolver refuses an overlay file without a
    /// base.
    Managed,
    /// The skill detaches right after the move, so the overlay only ferries
    /// the edits into the local canonical copy before it is removed.
    Detach,
}

/// Outcome of moving one dirty copy's edits into the managed overlay.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct OverlayPreservation {
    /// Relative paths whose edits now live in the overlay.
    preserved: Vec<String>,
    /// Overlay variants a different edit replaced, backed up durably so the
    /// losing edit stays recoverable.
    replaced: Vec<DiscardedCopy>,
}

/// Move the edited files of one generated copy into the managed overlay.
///
/// Every copy of one skill shares the overlay root, so two copies that edited
/// the same file differently would each claim the overlay's copy of it, and a
/// later copy would silently overwrite the earlier edit. The move refuses an
/// overlay a sync cannot resolve — a file the import does not provide, or a
/// file the copy deleted that an overlay entry cannot express — and when one
/// overlay file would hold two different edits, the variant already in the
/// overlay is backed up durably before the newer edit replaces it.
fn preserve_edits_as_overlay(
    context: &AppContext,
    transaction: &mut LifecycleTransaction,
    manifest: &mut WorkspaceManifest,
    skill: &ManagedSkillRef,
    dirty_dir: &Path,
    purpose: OverlayPurpose,
) -> Result<OverlayPreservation, AppError> {
    let overlay_path = manifest
        .overrides
        .get(&skill.skill_id)
        .cloned()
        .unwrap_or_else(|| manifest.overlay_path_for(&skill.skill_id));
    let overlay_root = context.working_directory.join(overlay_path.as_str());
    transaction.track_path(&overlay_root)?;
    fs::create_dir_all(&overlay_root).map_err(|source| AppError::FilesystemOperation {
        action: "create overlay directory",
        path: overlay_root.clone(),
        source,
    })?;

    let source_root = stored_import_root(skill.scope, &context.working_directory, &skill.skill_id)?
        .join(import_subpath(manifest, skill)?);

    let mut copy_files = Vec::new();
    collect_projection_files_for_overlay(dirty_dir, Path::new(""), &mut copy_files)?;
    copy_files.sort();
    let mut import_files = Vec::new();
    collect_projection_files_for_overlay(&source_root, Path::new(""), &mut import_files)?;
    import_files.sort();

    // A file only the copy holds has no imported base an overlay entry could
    // replace, and the resolver refuses an overlay file without a base, so a
    // managed apply must refuse it before the in-transaction sync meets an
    // overlay it cannot resolve. A detach keeps it: the overlay only ferries
    // the content into the local canonical copy.
    let unmanaged: Vec<PathBuf> = copy_files
        .iter()
        .filter(|file| !import_files.contains(file))
        .cloned()
        .collect();
    if purpose == OverlayPurpose::Managed && !unmanaged.is_empty() {
        return Err(AppError::DirtyContent {
            path: dirty_dir.join(unmanaged.first().cloned().unwrap_or_default()),
            message: format!(
                "the copy holds files the imported skill does not provide ({}), which the \
                 managed overlay cannot map; run 'skillctl update {} --apply --strategy fork' to \
                 keep them as local ownership, or 'skillctl update {} --apply --strategy \
                 discard' to back them up and drop them",
                quoted_relative_names(&unmanaged),
                skill.skill_id,
                skill.skill_id
            ),
        });
    }

    // An overlay entry replaces a file, so a deletion cannot survive the move:
    // the copy is rebuilt from the import and the deleted file returns. The
    // refusal names the deletion instead of silently reverting the edit.
    let deleted: Vec<PathBuf> = import_files
        .iter()
        .filter(|file| !copy_files.contains(file))
        .cloned()
        .collect();
    if !deleted.is_empty() {
        return Err(AppError::DirtyContent {
            path: dirty_dir.to_path_buf(),
            message: format!(
                "the copy deletes files the imported skill still provides ({}), which the \
                 managed overlay cannot express; restore the files and re-run, or run 'skillctl \
                 update {} --apply --strategy discard' to back the edits up and drop them",
                quoted_relative_names(&deleted),
                skill.skill_id
            ),
        });
    }

    let mut preservation = OverlayPreservation::default();
    let mut conflict_backup: Option<PathBuf> = None;
    for relative in copy_files {
        let edited = dirty_dir.join(&relative);
        let upstream = source_root.join(&relative);
        let edited_contents =
            fs::read(&edited).map_err(|source| AppError::FilesystemOperation {
                action: "read edited file for overlay preservation",
                path: edited.clone(),
                source,
            })?;
        let base_contents = match fs::read(&upstream) {
            Ok(contents) => Some(contents),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => None,
            Err(source) => {
                return Err(AppError::FilesystemOperation {
                    action: "read imported file for overlay comparison",
                    path: upstream,
                    source,
                });
            }
        };
        let differs = match &base_contents {
            Some(base) => *base != edited_contents,
            // A file without an imported base is unmanaged content a detach
            // still ferries into the local canonical copy.
            None => true,
        };
        if !differs {
            continue;
        }

        let destination = overlay_root.join(&relative);
        let incumbent = match fs::read(&destination) {
            Ok(contents) => Some(contents),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => None,
            Err(source) => {
                return Err(AppError::FilesystemOperation {
                    action: "read overlay file for conflict comparison",
                    path: destination,
                    source,
                });
            }
        };
        if let Some(incumbent) = incumbent {
            if incumbent == edited_contents {
                // The overlay already holds this edit, so the copy matches the
                // effective content and nothing needs to move.
                preservation
                    .preserved
                    .push(relative.to_string_lossy().into_owned());
                continue;
            }
            if base_contents.is_none()
                || base_contents
                    .as_deref()
                    .is_some_and(|base| incumbent.as_slice() != base)
            {
                // The overlay holds one edit and the copy another, either both
                // against the same imported file or both user-authored for a
                // file the import does not provide. The copy's edit wins the
                // overlay and the replaced variant is backed up durably, so
                // neither edit is silently discarded.
                let backup = match conflict_backup.as_ref() {
                    Some(backup) => backup.clone(),
                    None => {
                        let store = DiscardStore::open_default()?;
                        let backup = store.retain(&overlay_backup_label(skill), &overlay_root)?;
                        conflict_backup = Some(backup.clone());
                        backup
                    }
                };
                preservation.replaced.push(DiscardedCopy {
                    path: crate::planner::display_path(context, &destination),
                    skill: skill.skill_id.clone(),
                    backup: backup.display().to_string(),
                });
            }
        }

        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).map_err(|source| AppError::FilesystemOperation {
                action: "create overlay parent directory",
                path: parent.to_path_buf(),
                source,
            })?;
        }
        fs::copy(&edited, &destination).map_err(|source| AppError::FilesystemOperation {
            action: "copy edited file into the overlay",
            path: destination,
            source,
        })?;
        preservation
            .preserved
            .push(relative.to_string_lossy().into_owned());
    }

    if !manifest.overrides.contains_key(&skill.skill_id) {
        manifest
            .overrides
            .insert(skill.skill_id.clone(), overlay_path);
    }

    Ok(preservation)
}

/// Stable label for the durable backup that keeps a replaced overlay variant.
fn overlay_backup_label(skill: &ManagedSkillRef) -> String {
    format!("{}-overlay", skill.skill_id)
}

/// Quote a list of relative paths for a refusal that names the files.
fn quoted_relative_names(files: &[PathBuf]) -> String {
    files
        .iter()
        .map(|file| format!("'{}'", file.to_string_lossy()))
        .collect::<Vec<_>>()
        .join(", ")
}

fn import_subpath(
    manifest: &WorkspaceManifest,
    skill: &ManagedSkillRef,
) -> Result<String, AppError> {
    let scope = match skill.scope {
        ManagedScope::Workspace => ManifestScope::Workspace,
        ManagedScope::User => ManifestScope::User,
    };
    let import = manifest
        .imports
        .iter()
        .find(|import| import.id == skill.skill_id && import.scope == scope)
        .ok_or_else(|| AppError::ResolutionValidation {
            message: format!(
                "skill '{}' is not a managed import in the workspace manifest",
                skill.skill_id
            ),
        })?;

    Ok(import.path.as_str().to_string())
}

fn collect_projection_files_for_overlay(
    root: &Path,
    relative: &Path,
    files: &mut Vec<PathBuf>,
) -> Result<(), AppError> {
    let current = if relative.as_os_str().is_empty() {
        root.to_path_buf()
    } else {
        root.join(relative)
    };
    for entry in fs::read_dir(&current)
        .map_err(|source| AppError::FilesystemOperation {
            action: "read generated copy",
            path: current.clone(),
            source,
        })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| AppError::FilesystemOperation {
            action: "read generated copy entry",
            path: current.clone(),
            source,
        })?
    {
        let name = entry.file_name();
        if relative.as_os_str().is_empty() && name.as_os_str() == PROJECTION_METADATA_FILE {
            continue;
        }
        let next_relative = relative.join(&name);
        let path = entry.path();
        let metadata =
            fs::symlink_metadata(&path).map_err(|source| AppError::FilesystemOperation {
                action: "inspect generated copy entry",
                path: path.clone(),
                source,
            })?;
        if metadata.is_dir() {
            collect_projection_files_for_overlay(root, &next_relative, files)?;
            continue;
        }
        files.push(next_relative);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sealed_plan(operation: MutationOperation) -> MutationPlan {
        MutationPlanBuilder::new(operation, Path::new("/tmp/workspace"))
            .skill(ManagedSkillRef::new(
                ManagedScope::Workspace,
                "release-notes",
            ))
            .expect_import(ExpectedImport {
                import_id: "release-notes".to_string(),
                scope: ManagedScope::Workspace,
                revision: "abc123".to_string(),
                content_hash: "sha256:content".to_string(),
                overlay_hash: "sha256:none".to_string(),
                effective_version_hash: "sha256:effective".to_string(),
                bundle_digest: Some("sha256:bundle".to_string()),
                install_record_present: true,
            })
            .propose(ProposedImport {
                import_id: "release-notes".to_string(),
                revision: Some("def456".to_string()),
                bundle_digest: Some("sha256:new-bundle".to_string()),
                bundle_policy: BundlePolicy::Recompute,
                summary: "apply upstream revision def456".to_string(),
            })
            .target("/tmp/workspace/.agents/skillctl.lock", TargetAction::Update)
            .change("apply revision def456", None)
            .seal()
    }

    #[test]
    fn a_sealed_plan_verifies_its_own_digest() {
        let plan = sealed_plan(MutationOperation::Update);
        plan.verify_digest().expect("sealed plan verifies");
    }

    #[test]
    fn gated_drift_repair_names_strategies_the_trust_gate_allows() {
        for reason in [
            DriftReason::DirtyProjection,
            DriftReason::UnverifiableProjection,
        ] {
            let gated = reason.repair(
                "delta",
                RepairCommands::GatedUpdateStrategies,
                ManagedScope::Workspace,
            );
            assert!(
                gated.contains("update delta --apply --strategy fork"),
                "the gated advice must name the allowed escape hatch: {gated}",
            );
            assert!(
                gated.contains("skillctl sync --discard"),
                "the gated advice must name the ungated discard: {gated}",
            );
            assert!(
                !gated.contains("--strategy overlay") && !gated.contains("--strategy discard"),
                "the gated advice must not name refused strategies: {gated}",
            );
        }
    }

    #[test]
    fn ungated_drift_repair_keeps_the_update_strategies() {
        let dirty = DriftReason::DirtyProjection.repair(
            "delta",
            RepairCommands::UpdateStrategies,
            ManagedScope::Workspace,
        );
        assert!(
            dirty.contains("--strategy overlay") && dirty.contains("--strategy discard"),
            "an ungated skill keeps both repair strategies: {dirty}",
        );
        let unverifiable = DriftReason::UnverifiableProjection.repair(
            "delta",
            RepairCommands::UpdateStrategies,
            ManagedScope::Workspace,
        );
        assert!(
            unverifiable.contains("--strategy overlay") && unverifiable.contains("sync --discard"),
            "an ungated copy keeps the overlay repair: {unverifiable}",
        );
    }

    #[test]
    fn a_copy_no_install_record_holds_is_repaired_by_the_discard_alone() {
        for reason in [
            DriftReason::DirtyProjection,
            DriftReason::UnverifiableProjection,
        ] {
            let advice = reason.repair(
                "delta",
                RepairCommands::DiscardOnly,
                ManagedScope::Workspace,
            );
            assert!(
                advice.contains("skillctl sync --discard"),
                "advice for a skill no install records must name the discard: {advice}",
            );
            assert!(
                !advice.contains("skillctl update"),
                "advice must not name an update that exits with 'skill 'delta' is not \
                 installed': {advice}",
            );
            assert!(
                !advice.contains("--strategy"),
                "advice must not name any update strategy: {advice}",
            );
        }
    }

    #[test]
    fn drift_repair_qualifies_the_sync_command_with_the_observed_scope() {
        for reason in [
            DriftReason::DirtyProjection,
            DriftReason::UnverifiableProjection,
        ] {
            let gated = reason.repair(
                "delta",
                RepairCommands::GatedUpdateStrategies,
                ManagedScope::User,
            );
            let discard_only =
                reason.repair("delta", RepairCommands::DiscardOnly, ManagedScope::User);
            for advice in [gated, discard_only] {
                assert!(
                    advice.contains("'skillctl --scope user sync --discard'"),
                    "a user-scope copy is refreshed only by the qualified sync: {advice}",
                );
                assert!(
                    !advice.contains("'skillctl sync"),
                    "a plain sync reads workspace scope and must not be advised: {advice}",
                );
            }
        }

        let unverifiable = DriftReason::UnverifiableProjection.repair(
            "delta",
            RepairCommands::UpdateStrategies,
            ManagedScope::User,
        );
        assert!(
            unverifiable.contains("'skillctl --scope user sync --discard'"),
            "an ungated copy still names the qualified discard first: {unverifiable}",
        );
    }

    #[test]
    fn an_edited_plan_is_rejected_as_tampered() {
        let mut plan = sealed_plan(MutationOperation::Update);
        plan.proposed.clear();

        let error = plan.verify_digest().expect_err("tampering is detected");
        assert!(
            error.to_string().contains("digests to"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn plan_documents_verify_every_plan_digest() {
        let plan = sealed_plan(MutationOperation::Update);
        let document = PlanDocument::new(Path::new("/tmp/workspace"), "clean", vec![plan]);
        let serialized = document.to_json_string().expect("document serializes");
        let parsed = PlanDocument::from_json_str(&serialized).expect("document verifies");
        assert_eq!(parsed, document);
    }

    #[test]
    fn an_edited_plan_document_is_rejected() {
        let plan = sealed_plan(MutationOperation::Update);
        let document = PlanDocument::new(Path::new("/tmp/workspace"), "clean", vec![plan]);
        let serialized = document.to_json_string().expect("document serializes");
        let tampered = serialized.replace("def456", "def457");

        let error =
            PlanDocument::from_json_str(&tampered).expect_err("tampered document is rejected");
        assert!(
            error.to_string().contains("digests to"),
            "unexpected error: {error}"
        );
    }

    /// One apply target with an explicit trust decision.
    fn gated_target(trust: Option<crate::trust::SkillTrust>) -> UpdateApplyTarget {
        UpdateApplyTarget {
            skill: ManagedSkillRef::new(ManagedScope::Workspace, "delta"),
            source_url: "file:///source".to_string(),
            target_revision: "def456".to_string(),
            expected: ExpectedImport {
                import_id: "delta".to_string(),
                scope: ManagedScope::Workspace,
                revision: "abc123".to_string(),
                content_hash: "sha256:content".to_string(),
                overlay_hash: "sha256:none".to_string(),
                effective_version_hash: "sha256:effective".to_string(),
                bundle_digest: None,
                install_record_present: true,
            },
            summary: crate::planner::SkillUpdatePlan {
                skill: "delta".to_string(),
                scope: ManagedScope::Workspace,
                checked_at: "2026-11-05T00:00:00Z".to_string(),
                source: crate::planner::UpdateSourceSummary {
                    kind: crate::source::SourceKind::Git,
                    url: "file:///source".to_string(),
                    subpath: String::new(),
                },
                pinned_revision: "abc123".to_string(),
                latest_revision: Some("def456".to_string()),
                outcome: crate::state::UpdateCheckOutcome::UpdateAvailable,
                overlay_detected: false,
                overlay_changed_since_recorded_state: false,
                local_modification_detected: false,
                recommended_action: crate::planner::UpdateAction::Skip,
                available_actions: vec![crate::planner::UpdateAction::Skip],
                modifications: Vec::new(),
                missing_projections: Vec::new(),
                trust,
                notes: Vec::new(),
                store_diverged: false,
            },
        }
    }

    fn blocked_trust() -> crate::trust::SkillTrust {
        crate::trust::SkillTrust::imported_unreviewed("delta", true).block_apply_update("delta")
    }

    #[test]
    fn a_blocked_trust_gate_refuses_every_revision_apply_strategy() {
        let target = gated_target(Some(blocked_trust()));

        for strategy in [
            ApplyStrategy::Clean,
            ApplyStrategy::Overlay,
            ApplyStrategy::Discard,
        ] {
            let error =
                ensure_trust_gate_allows(&target, strategy).expect_err("the gate must refuse");
            assert!(
                matches!(error, AppError::TrustGateBlocked { .. }),
                "unexpected error for {:?}: {error}",
                strategy,
            );
            assert_eq!(
                error.exit_status(),
                crate::error::ExitStatus::TrustGateBlocked
            );
        }
    }

    #[test]
    fn the_trust_gate_keeps_the_fork_and_skip_escape_hatches() {
        let target = gated_target(Some(blocked_trust()));

        ensure_trust_gate_allows(&target, ApplyStrategy::Fork).expect("a fork detaches instead");
        ensure_trust_gate_allows(&target, ApplyStrategy::Skip).expect("a skip changes nothing");
    }

    #[test]
    fn a_target_without_a_trust_decision_is_not_refused() {
        let target = gated_target(None);

        ensure_trust_gate_allows(&target, ApplyStrategy::Clean)
            .expect("no recorded gate means no refusal");
    }

    #[test]
    fn an_ungated_trust_decision_allows_the_apply() {
        let trust = crate::trust::SkillTrust::imported_unreviewed("delta", false);
        let target = gated_target(Some(trust));

        ensure_trust_gate_allows(&target, ApplyStrategy::Clean).expect("normal risk applies");
    }

    /// One apply target whose check found nothing to move upstream.
    fn pinned_target(
        outcome: crate::state::UpdateCheckOutcome,
        kind: crate::source::SourceKind,
    ) -> UpdateApplyTarget {
        let mut target = gated_target(None);
        target.target_revision = target.expected.revision.clone();
        target.summary.latest_revision = Some(target.expected.revision.clone());
        target.summary.outcome = outcome;
        target.summary.source.kind = kind;
        target
    }

    #[test]
    fn a_target_that_repairs_edits_keeps_its_pinned_revision() {
        assert!(
            repairs_unrecorded_edits(&pinned_target(
                crate::state::UpdateCheckOutcome::UpToDate,
                crate::source::SourceKind::Git,
            )),
            "an up-to-date git import is repaired without moving its revision",
        );
        assert!(
            repairs_unrecorded_edits(&pinned_target(
                crate::state::UpdateCheckOutcome::CommitPinned,
                crate::source::SourceKind::Git,
            )),
            "a commit-pinned git import is repaired without moving its revision",
        );
        assert!(
            repairs_unrecorded_edits(&pinned_target(
                crate::state::UpdateCheckOutcome::LocalSource,
                crate::source::SourceKind::LocalPath,
            )),
            "a local source is repaired without moving its revision",
        );
        assert!(
            !repairs_unrecorded_edits(&gated_target(None)),
            "a git import with a newer revision moves to it instead",
        );
        assert!(
            !repairs_unrecorded_edits(&pinned_target(
                crate::state::UpdateCheckOutcome::Detached,
                crate::source::SourceKind::Git,
            )),
            "a detached import contributes no target at all",
        );
    }

    #[test]
    fn strategy_identifiers_round_trip() {
        for strategy in [
            ApplyStrategy::Clean,
            ApplyStrategy::Overlay,
            ApplyStrategy::Discard,
            ApplyStrategy::Fork,
            ApplyStrategy::Skip,
        ] {
            assert_eq!(
                ApplyStrategy::parse(strategy.as_str()),
                Some(strategy),
                "{} did not round trip",
                strategy.as_str()
            );
        }
        assert_eq!(ApplyStrategy::parse("nonsense"), None);
    }

    #[test]
    fn change_diff_summarizes_added_removed_and_modified_files() {
        let previous = BundleManifest::from_json_str(
            r#"{"format_version":1,"entries":[
                {"path":"SKILL.md","mode":"file","digest":"sha256:1111111111111111111111111111111111111111111111111111111111111111","size":1},
                {"path":"old.md","mode":"file","digest":"sha256:2222222222222222222222222222222222222222222222222222222222222222","size":2},
                {"path":"shared.md","mode":"file","digest":"sha256:3333333333333333333333333333333333333333333333333333333333333333","size":3}
            ]}"#,
        )
        .expect("previous manifest parses");
        let next = BundleManifest::from_json_str(
            r#"{"format_version":1,"entries":[
                {"path":"SKILL.md","mode":"file","digest":"sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc","size":1},
                {"path":"new.md","mode":"file","digest":"sha256:4444444444444444444444444444444444444444444444444444444444444444","size":4},
                {"path":"shared.md","mode":"file","digest":"sha256:3333333333333333333333333333333333333333333333333333333333333333","size":3}
            ]}"#,
        )
        .expect("next manifest parses");

        let diff = change_diff(&previous, &next);
        assert_eq!(diff.added, vec!["new.md".to_string()]);
        assert_eq!(diff.removed, vec!["old.md".to_string()]);
        assert_eq!(diff.modified, vec!["SKILL.md".to_string()]);
    }

    #[test]
    fn drift_reasons_serialize_as_their_reported_identifier() {
        for reason in [
            DriftReason::DirtyProjection,
            DriftReason::UnverifiableProjection,
        ] {
            let serialized = serde_json::to_value(reason).expect("reason serializes");
            assert_eq!(
                serialized,
                serde_json::json!(reason.as_str()),
                "the serialized form must match the reported identifier"
            );
        }
    }

    #[test]
    fn a_retained_backup_never_refuses_an_existing_destination() {
        let root = std::env::temp_dir().join(format!(
            "skillctl-retain-test-{}-{}",
            std::process::id(),
            sanitize_component(&current_timestamp())
        ));
        let store = DiscardStore { root: root.clone() };

        let source = root.join("source");
        fs::create_dir_all(&source).expect("source tree exists");
        fs::write(source.join("SKILL.md"), "edited").expect("source file exists");

        // Occupy the exact name the first retain would compute, the way a
        // pinned clock (SOURCE_DATE_EPOCH) plus a repeated process id does.
        let occupied = store
            .root
            .join("test-skill")
            .join(retained_backup_base_name());
        fs::create_dir_all(&occupied).expect("colliding backup name exists");

        let backup = store
            .retain("test-skill", &source)
            .expect("an occupied name counts up instead of refusing");
        assert_ne!(backup, occupied, "the backup moved to a free name");
        assert!(
            fs::read_to_string(backup.join("SKILL.md"))
                .expect("the backup holds the tree")
                .contains("edited"),
            "the retained content is intact at {}",
            backup.display()
        );

        fs::remove_dir_all(&root).ok();
    }
}
