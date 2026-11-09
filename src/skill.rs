//! Skill inventory, parsing, and inspection domain entry points.

use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs, io,
    path::{Component, Path, PathBuf},
};

use serde::Serialize;
use serde_json::json;
use serde_yaml::Value;

use crate::{
    adapter::{AdapterRegistry, TargetScope},
    app::AppContext,
    builtin,
    cli::Scope,
    doctor,
    error::AppError,
    history::{self, HistoryLedger},
    lockfile::WorkspaceLockfile,
    manifest::{ImportDefinition, ManifestScope, WorkspaceManifest},
    materialize, planner,
    response::AppResponse,
    source::{current_timestamp, stored_import_root},
    state::{LocalStateStore, ManagedScope, ManagedSkillRef},
};

/// Default relative path to canonical workspace skills.
pub const DEFAULT_SKILLS_DIR: &str = ".agents/skills";

/// Standard skill manifest file name.
pub const SKILL_MANIFEST_FILE: &str = "SKILL.md";
/// Vendor-specific OpenAI metadata file preserved by `skillctl`.
pub const OPENAI_METADATA_FILE: &str = "agents/openai.yaml";
/// Claude-specific frontmatter fields that should pass through untouched.
pub const CLAUDE_FRONTMATTER_FIELDS: &[&str] = &[
    "disable-model-invocation",
    "user-invocable",
    "context",
    "agent",
    "hooks",
];

const STANDARD_FRONTMATTER_FIELDS: &[&str] = &[
    "name",
    "description",
    "license",
    "compatibility",
    "metadata",
    "capabilities",
    "credentials",
    "dependencies",
    "allowed-tools",
];

/// Strongly typed skill identifier.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct SkillName(pub String);

impl SkillName {
    /// Parse and validate a skill name against the open Agent Skills contract.
    pub fn parse(value: &str, skill_path: &Path) -> Result<Self, AppError> {
        validate_skill_name(value, skill_path)?;
        Ok(Self(value.to_string()))
    }

    /// Borrow the validated skill name as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Parsed frontmatter for a `SKILL.md` document.
#[derive(Clone, Debug, PartialEq)]
pub struct SkillFrontmatter {
    /// Vendor-specific fields that should be passed through unchanged.
    pub vendor_fields: BTreeMap<String, Value>,
}

/// Vendor-specific metadata files associated with a skill directory.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SkillVendorMetadata {
    /// Raw file contents keyed by stable relative path under the skill root.
    pub files: BTreeMap<PathBuf, String>,
}

/// Typed safety metadata parsed from declarative `SKILL.md` fields.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct SkillSafetySummary {
    /// Coarse capability classes declared by the skill author.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<String>,
    /// Structured credential requirements declared by the skill author.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub credentials: Vec<SkillCredentialRequirement>,
    /// Declared external tool requirements.
    #[serde(default, skip_serializing_if = "SkillDependencySummary::is_empty")]
    pub dependencies: SkillDependencySummary,
}

/// One declared credential requirement for a skill.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SkillCredentialRequirement {
    /// Environment variable or stable credential identifier.
    pub name: String,
    /// Whether the credential is optional at runtime.
    #[serde(default, skip_serializing_if = "is_false")]
    pub optional: bool,
    /// Human-facing explanation for why the credential is needed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub purpose: Option<String>,
}

/// Declared dependency requirements for a skill.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct SkillDependencySummary {
    /// External tools expected on the user's PATH.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<SkillToolRequirement>,
}

impl SkillDependencySummary {
    /// Return whether no dependency requirements are present.
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

/// One declared external tool requirement for a skill.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SkillToolRequirement {
    /// Executable name expected on PATH.
    pub name: String,
    /// Whether the tool is optional at runtime.
    #[serde(default, skip_serializing_if = "is_false")]
    pub optional: bool,
    /// Human-facing explanation for why the tool is needed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub purpose: Option<String>,
}

impl SkillSafetySummary {
    /// Return whether no structured safety metadata is present.
    pub fn is_empty(&self) -> bool {
        self.capabilities.is_empty() && self.credentials.is_empty() && self.dependencies.is_empty()
    }

    /// Return any required credentials missing from the current environment.
    pub fn missing_required_credentials(&self) -> Vec<String> {
        self.credentials
            .iter()
            .filter(|credential| !credential.optional)
            .filter(|credential| std::env::var_os(&credential.name).is_none())
            .map(|credential| credential.name.clone())
            .collect()
    }

    /// Return any required tool dependencies missing from the current PATH.
    pub fn missing_required_tools(&self) -> Vec<String> {
        self.dependencies
            .tools
            .iter()
            .filter(|tool| !tool.optional)
            .filter(|tool| !tool_exists_on_path(&tool.name))
            .map(|tool| tool.name.clone())
            .collect()
    }

    /// Build user-visible warnings for imported skills with declared capabilities.
    pub fn imported_warnings(&self, skill_name: &str) -> Vec<String> {
        let mut warnings = Vec::new();

        if !self.capabilities.is_empty() {
            warnings.push(format!(
                "imported skill '{}' declares capabilities: {}",
                skill_name,
                self.capabilities.join(", ")
            ));
        }

        let missing_credentials = self.missing_required_credentials();
        if !missing_credentials.is_empty() {
            warnings.push(format!(
                "imported skill '{}' requires credentials {} that are not set in the current environment",
                skill_name,
                missing_credentials.join(", ")
            ));
        }

        let missing_tools = self.missing_required_tools();
        if !missing_tools.is_empty() {
            warnings.push(format!(
                "imported skill '{}' requires tools {} that are not available on PATH",
                skill_name,
                missing_tools.join(", ")
            ));
        }

        warnings
    }
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn tool_exists_on_path(name: &str) -> bool {
    let Some(path) = env::var_os("PATH") else {
        return false;
    };

    let candidate_names = executable_candidates(name);
    env::split_paths(&path).any(|directory| {
        candidate_names.iter().any(|candidate| {
            fs::metadata(directory.join(candidate)).is_ok_and(|metadata| metadata.is_file())
        })
    })
}

fn executable_candidates(name: &str) -> Vec<String> {
    #[cfg(windows)]
    {
        let has_extension = Path::new(name).extension().is_some();
        if has_extension {
            return vec![name.to_string()];
        }

        let pathext = env::var_os("PATHEXT")
            .map(|value| {
                env::split_paths(&value)
                    .map(|path| path.to_string_lossy().to_string())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_else(|| vec![".EXE".to_string(), ".BAT".to_string(), ".CMD".to_string()]);

        let mut candidates = vec![name.to_string()];
        for extension in pathext {
            candidates.push(format!("{name}{extension}"));
        }
        candidates
    }

    #[cfg(not(windows))]
    {
        vec![name.to_string()]
    }
}

/// Parsed skill directory definition.
#[derive(Clone, Debug, PartialEq)]
pub struct SkillDefinition {
    /// Identifier declared in `SKILL.md`.
    pub name: SkillName,
    /// Human-facing description declared in `SKILL.md`.
    pub description: String,
    /// Filesystem path to the skill root.
    pub root: PathBuf,
    /// Resolved path to the `SKILL.md` file.
    pub manifest_path: PathBuf,
    /// Parsed frontmatter, including vendor-specific passthrough fields.
    pub frontmatter: SkillFrontmatter,
    /// Typed safety metadata derived from declarative frontmatter fields.
    pub safety: SkillSafetySummary,
    /// Supported vendor-specific metadata files preserved alongside the skill.
    pub vendor_metadata: SkillVendorMetadata,
}

impl SkillDefinition {
    /// Load, parse, and validate a skill directory.
    pub fn load_from_dir(root: impl AsRef<Path>) -> Result<Self, AppError> {
        let root = root.as_ref();
        ensure_directory(root)?;

        let manifest_path = root.join(SKILL_MANIFEST_FILE);
        let source = read_skill_manifest(&manifest_path)?;
        let vendor_metadata = load_vendor_metadata(root)?;

        Self::from_source(root, manifest_path, &source, vendor_metadata)
    }

    /// Parse and validate a skill definition from explicit file contents.
    pub fn from_source(
        root: impl AsRef<Path>,
        manifest_path: impl Into<PathBuf>,
        source: &str,
        vendor_metadata: SkillVendorMetadata,
    ) -> Result<Self, AppError> {
        let root = root.as_ref();
        let manifest_path = manifest_path.into();
        let frontmatter = frontmatter_source(source, &manifest_path)?;
        let fields = parse_frontmatter(&frontmatter, &manifest_path)?;

        validate_optional_standard_fields(&fields, &manifest_path)?;
        let safety = parse_safety_summary(&fields, &manifest_path)?;

        let name = SkillName::parse(
            require_string_field(&fields, "name", &manifest_path)?,
            &manifest_path,
        )?;
        let description = require_string_field(&fields, "description", &manifest_path)?;
        validate_description(description, &manifest_path)?;
        validate_directory_name(root, name.as_str(), &manifest_path)?;

        Ok(Self {
            name,
            description: description.to_string(),
            root: root.to_path_buf(),
            manifest_path,
            frontmatter: SkillFrontmatter {
                vendor_fields: extract_vendor_frontmatter(&fields),
            },
            safety,
            vendor_metadata,
        })
    }
}

/// Validate and normalize a relative overlay path used for shadow-file mapping.
pub fn normalize_overlay_relative_path(path: impl AsRef<Path>) -> Result<PathBuf, AppError> {
    let path = path.as_ref();
    if path.as_os_str().is_empty() {
        return Err(skill_validation(path, "overlay path must not be empty"));
    }

    let path_display = path.to_string_lossy();
    if path_display.contains('\\') {
        return Err(skill_validation(
            path,
            format!("overlay path '{}' must use '/' separators", path.display()),
        ));
    }

    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(segment) => normalized.push(segment),
            Component::CurDir
            | Component::ParentDir
            | Component::RootDir
            | Component::Prefix(_) => {
                return Err(skill_validation(
                    path,
                    format!(
                        "overlay path '{}' must be relative and must not contain '.' or '..' segments",
                        path.display()
                    ),
                ));
            }
        }
    }

    if normalized.as_os_str().is_empty() {
        return Err(skill_validation(path, "overlay path must not be empty"));
    }

    Ok(normalized)
}

/// Typed request for `skillctl list`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ListRequest;

/// Typed request for `skillctl remove`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoveRequest {
    /// Managed skill name.
    pub skill: SkillName,
}

impl RemoveRequest {
    /// Create a remove request from parsed CLI arguments.
    pub fn new(skill: String) -> Self {
        Self {
            skill: SkillName(skill),
        }
    }
}

/// Typed request for `skillctl explain`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExplainRequest {
    /// Managed skill name.
    pub skill: SkillName,
}

impl ExplainRequest {
    /// Create an explain request from parsed CLI arguments.
    pub fn new(skill: String) -> Self {
        Self {
            skill: SkillName(skill),
        }
    }
}

/// Typed request for `skillctl inspect`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InspectRequest {
    /// Managed skill name.
    pub skill: SkillName,
}

impl InspectRequest {
    /// Create an inspect request from parsed CLI arguments.
    pub fn new(skill: String) -> Self {
        Self {
            skill: SkillName(skill),
        }
    }
}

/// Typed request for `skillctl enable`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EnableRequest {
    /// Managed skill name.
    pub skill: SkillName,
}

impl EnableRequest {
    /// Create an enable request from parsed CLI arguments.
    pub fn new(skill: String) -> Self {
        Self {
            skill: SkillName(skill),
        }
    }
}

/// Typed request for `skillctl disable`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DisableRequest {
    /// Managed skill name.
    pub skill: SkillName,
}

impl DisableRequest {
    /// Create a disable request from parsed CLI arguments.
    pub fn new(skill: String) -> Self {
        Self {
            skill: SkillName(skill),
        }
    }
}

/// Typed request for `skillctl path`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PathRequest {
    /// Managed skill name.
    pub skill: SkillName,
}

impl PathRequest {
    /// Create a path request from parsed CLI arguments.
    pub fn new(skill: String) -> Self {
        Self {
            skill: SkillName(skill),
        }
    }
}

/// Handle `skillctl list`.
pub fn handle_list(context: &AppContext, _request: ListRequest) -> Result<AppResponse, AppError> {
    let store = LocalStateStore::open_default_for(&context.working_directory)?;
    let installs = store
        .list_install_records()?
        .into_iter()
        .filter(|install| match context.selector.scope {
            Some(Scope::Workspace) => install.skill.scope == ManagedScope::Workspace,
            Some(Scope::User) => install.skill.scope == ManagedScope::User,
            None => true,
        })
        .collect::<Vec<_>>();
    if installs.is_empty() {
        return Ok(AppResponse::success("list")
            .with_summary("No managed skills are installed.")
            .with_data(json!({ "skills": [] })));
    }

    let manifest = load_manifest_or_default(&context.working_directory)?;
    let lockfile = load_lockfile_or_default(&context.working_directory)?;
    let mut skills = Vec::with_capacity(installs.len());

    for install in installs {
        let managed_import = managed_import(&manifest, &install.skill);
        let lockfile_entry = managed_import.and_then(|import| lockfile.imports.get(&import.id));
        let snapshot = store.skill_snapshot(&install.skill)?;

        skills.push(json!({
            "skill": install.skill.skill_id,
            "scope": install.skill.scope.as_str(),
            "builtin": builtin::is_bundled_install(&install),
            "managed_import": managed_import.is_some(),
            "managed_import_enabled": managed_import.map(|import| import.enabled),
            "source": {
                "type": source_type_label(&install),
                "url": install.source_url,
                "subpath": install.source_subpath,
            },
            "resolved_revision": install.resolved_revision,
            "effective_version_hash": install.effective_version_hash,
            "detached": install.detached,
            "forked": install.forked,
            "overlay_root": managed_import
                .and_then(|import| manifest.overrides.get(&import.id))
                .map(|path| path.as_str().to_string()),
            "stored_source_root": lockfile_entry
                .map(|_| stored_import_root(install.skill.scope, &context.working_directory, &install.skill.skill_id))
                .transpose()?
                .map(|root| crate::planner::portable_path(&root)),
            "active_source_root": lockfile_entry
                .map(|entry| stored_import_root(install.skill.scope, &context.working_directory, &install.skill.skill_id).map(|root| root.join(entry.source.subpath.as_str())))
                .transpose()?
                .map(|path| crate::planner::portable_path(&path)),
            "pinned_reference": snapshot.pin.as_ref().map(|pin| pin.requested_reference.clone()),
            // The stored check can predate the apply that moved this install,
            // so the payload carries the fitted view instead of the raw row.
            "latest_update": snapshot
                .latest_update_check
                .clone()
                .map(|check| check.fitted_to(&install))
                .map(|check| json!({
                    "outcome": check.outcome,
                    "checked_at": check.checked_at,
                })),
            "local_modification_count": snapshot.local_modifications.len(),
            "rollback_count": snapshot.rollbacks.len(),
            "projections": snapshot
                .projections
                .iter()
                .map(|projection| planner::projection_view(context, projection))
                .collect::<Result<Vec<_>, _>>()?,
        }));
    }

    let count = skills.len();
    let data = json!({ "skills": skills });
    let detail = crate::human::list_detail_lines(&data);
    Ok(AppResponse::success("list")
        .with_summary(format!(
            "Listed {count} managed skill{}.",
            if count == 1 { "" } else { "s" }
        ))
        .with_detail_lines(detail)
        .with_data(data))
}

/// Handle `skillctl remove`.
pub fn handle_remove(
    context: &AppContext,
    request: RemoveRequest,
) -> Result<AppResponse, AppError> {
    {
        let store = LocalStateStore::open_default_for(&context.working_directory)?;
        match classify_bundled_request(
            context,
            &store,
            request.skill.as_str(),
            context.selector.scope,
        )? {
            // The bundled skill is locally owned, but an unscoped request may
            // also name a stale lockfile entry that shares the bundled name.
            // Removing the bundled skill would leave that entry in place, so
            // the request stays ambiguous until a scope selector resolves it.
            BundledRequest::Bundled
                if context.selector.scope.is_none()
                    && names_undeclared_lockfile_entry(context, request.skill.as_str())? =>
            {
                return Err(bundled_name_and_stale_pin_error(request.skill.as_str()));
            }
            BundledRequest::Bundled => return builtin::handle_remove(context),
            BundledRequest::Ambiguous => return Err(multiple_scopes_error(request.skill.as_str())),
            BundledRequest::Managed => {}
        }
    }

    let manifest = load_manifest_or_default(&context.working_directory)?;
    let lockfile = load_lockfile_or_default(&context.working_directory)?;
    let store = LocalStateStore::open_default_for(&context.working_directory)?;
    let target = resolve_managed_import(
        context,
        &manifest,
        &lockfile,
        &store,
        request.skill.as_str(),
        ImportResolution::Remove,
    )?;
    let managed_skill = target.skill.clone();
    let expected = target.expected_import(context, &lockfile)?;

    let scoped_context = history::context_for_scope(context, managed_skill.scope);
    let stored_source_root = stored_import_root(
        managed_skill.scope,
        &context.working_directory,
        request.skill.as_str(),
    )?;
    let mut builder = crate::apply::MutationPlanBuilder::new(
        crate::apply::MutationOperation::Remove,
        &context.working_directory,
    )
    .skill(managed_skill.clone())
    .propose(crate::apply::ProposedImport {
        import_id: managed_skill.skill_id.clone(),
        revision: None,
        bundle_digest: None,
        bundle_policy: crate::apply::BundlePolicy::Release,
        summary: format!(
            "remove '{}' from {} scope and release its retained artifact",
            managed_skill.skill_id,
            managed_skill.scope.as_str()
        ),
    })
    .target(manifest.path.clone(), crate::apply::TargetAction::Update)
    .target(lockfile.path.clone(), crate::apply::TargetAction::Update)
    .targets(
        materialize::sync_mutation_root_paths(
            &scoped_context,
            &manifest,
            TargetScope::from(managed_skill.scope),
        )?,
        crate::apply::TargetAction::Remove,
    )
    .target(stored_source_root, crate::apply::TargetAction::Remove);

    if let Some(expected) = expected {
        builder = builder.expect_import(expected);
    }

    // Removal drops the lockfile entry, so the retained artifact is the only
    // remaining copy of the approved content. Record it as a recovery
    // reference instead of losing the bundle identity.
    if let Some(digest) = lockfile
        .imports
        .get(request.skill.as_str())
        .and_then(|entry| entry.bundle_digest())
    {
        builder = builder.retained_artifacts(vec![digest.to_string()]);
    }
    let plan = builder.seal();

    crate::apply::run_guarded(&plan, context, |transaction| {
        transaction.track_state_database()?;
        let mut store = LocalStateStore::open_default_for(&context.working_directory)?;
        let mut manifest = load_manifest_or_default(&context.working_directory)?;
        let mut lockfile = load_lockfile_or_default(&context.working_directory)?;
        let target = resolve_managed_import(
            context,
            &manifest,
            &lockfile,
            &store,
            request.skill.as_str(),
            ImportResolution::Remove,
        )?;
        let managed_skill = target.skill.clone();

        let timestamp = current_timestamp();
        let mut removed_paths = Vec::new();
        let mut retained_paths = Vec::new();
        let mut cleared_stale_pin = false;

        if let Some(import_index) = manifest_import_index(&manifest, &managed_skill) {
            manifest.imports.remove(import_index);
            if let Some(overlay_path) = manifest.overrides.remove(request.skill.as_str()) {
                retained_paths.push(overlay_path.as_str().to_string());
            }
            manifest.write_to_path()?;

            lockfile.imports.remove(request.skill.as_str());
            lockfile.write_to_path()?;
        } else if lockfile.imports.remove(request.skill.as_str()).is_some() {
            // The manifest no longer declares the import but the lockfile still
            // pins it, which is the `stale-lockfile-entry` state `doctor` and
            // `explain` report. Clear the pin instead of refusing it, so the
            // entry no longer needs a reinstall that exists only to be removed
            // again.
            if let Some(overlay_path) = manifest.overrides.remove(request.skill.as_str()) {
                retained_paths.push(overlay_path.as_str().to_string());
                manifest.write_to_path()?;
            }
            lockfile.write_to_path()?;
            cleared_stale_pin = true;
        } else if !target.detached_from_upstream() {
            return Err(AppError::ResolutionValidation {
                message: format!(
                    "skill '{}' is not a managed import in the workspace manifest",
                    request.skill.as_str()
                ),
            });
        }

        if managed_skill.scope == ManagedScope::Workspace && target.detached_from_upstream() {
            let local_root = context
                .working_directory
                .join(manifest.layout.skills_dir.as_str())
                .join(request.skill.as_str());
            transaction.track_path(&local_root)?;
            if materialize::remove_directory_if_exists(
                &local_root,
                "remove canonical workspace skill",
            )? {
                removed_paths.push(planner::display_path(context, &local_root));
            }
        }

        let stored_source_root = stored_import_root(
            managed_skill.scope,
            &context.working_directory,
            request.skill.as_str(),
        )?;
        transaction.track_path(&stored_source_root)?;
        if materialize::remove_directory_if_exists(
            &stored_source_root,
            "remove stored import root",
        )? {
            removed_paths.push(crate::planner::portable_path(&stored_source_root));
        }

        // Projection rebuild reads every enabled import, so a checkout that
        // still holds an import it never reconstructed cannot run one. A
        // removal is still correct without it, and refusing it would leave a
        // fresh clone with no way to repair itself.
        let unrestored = materialize::unrestored_imports(context, &manifest)?;
        let mut sync_report = if unrestored.is_empty() {
            Some(materialize::sync_workspace(&scoped_context)?)
        } else {
            None
        };
        // The record this removal is about to delete is what makes the update
        // strategies repairable, so the report the response embeds must not
        // name them for the removed skill.
        if let Some(report) = sync_report.as_mut() {
            report.advise_discard_only_for(managed_skill.skill_id.as_str(), managed_skill.scope);
        }

        {
            let mut ledger = HistoryLedger::new(&mut store);
            if let Some(report) = &sync_report {
                materialize::record_pruned_projection_history(
                    &mut ledger,
                    context,
                    managed_skill.scope,
                    report,
                    &timestamp,
                )?;
            }
            for removed_path in &removed_paths {
                ledger.record_cleanup(Some(&managed_skill), removed_path, &timestamp)?;
            }
        }

        store.delete_current_skill_state(&managed_skill)?;
        if let Some(report) = &sync_report {
            materialize::rebuild_projection_records_for_scope(
                &mut store,
                managed_skill.scope,
                report,
                &timestamp,
            )?;
        }

        let projection = match &sync_report {
            Some(report) => {
                serde_json::to_value(report).map_err(|source| AppError::JsonRender { source })?
            }
            None => skipped_projection_report(&unrestored, managed_skill.scope),
        };

        let mut response = AppResponse::success("remove")
            .with_summary(format!(
                "Removed {} from {} scope.",
                request.skill.as_str(),
                managed_skill.scope.as_str()
            ))
            .with_data(json!({
                "skill": request.skill.as_str(),
                "scope": managed_skill.scope.as_str(),
                "resolved_from": target.resolved_from(),
                "removed_paths": removed_paths,
                "retained_paths": retained_paths,
                "retained_artifacts": plan.recovery.retained_artifacts,
                "projection": projection,
            }));

        match target.origin {
            ImportOrigin::InstallRecord => {
                if cleared_stale_pin {
                    response = response.with_warning(format!(
                        "the workspace manifest no longer declared '{}', so remove cleared the \
                         stale entry .agents/skillctl.lock still pinned",
                        request.skill.as_str()
                    ));
                }
            }
            ImportOrigin::ManifestAndLockfile => {
                response = response.with_warning(format!(
                    "no install record for '{}' existed in this checkout, so remove acted on the \
                     import declared by .agents/skillctl.yaml and .agents/skillctl.lock",
                    request.skill.as_str()
                ));
            }
            ImportOrigin::LockfileOnly => {
                response = response.with_warning(format!(
                    "no install record or manifest import for '{}' existed in this checkout, so \
                     remove cleared the stale entry .agents/skillctl.lock pinned",
                    request.skill.as_str()
                ));
            }
        }
        if !unrestored.is_empty() {
            response =
                response.with_warning(skipped_projection_warning(&unrestored, managed_skill.scope));
        }
        for retained_path in &retained_paths {
            response = response.with_warning(format!(
                "retained overlay directory '{}' for manual reuse",
                retained_path
            ));
        }
        for digest in &plan.recovery.retained_artifacts {
            response = response.with_warning(format!(
                "retained artifact '{digest}' stays in the artifact store for recovery"
            ));
        }
        // A preserved copy outlives the skill it belonged to, so the removal is
        // the last command that can still name the skill. Its warnings carry the
        // repair advice, which the deleted record leaves pointing at the
        // explicit discard alone.
        if let Some(report) = &sync_report {
            for warning in &report.warnings {
                response = response.with_warning(warning.clone());
            }
        }

        Ok(response)
    })
}

/// Handle `skillctl explain`.
pub fn handle_explain(
    context: &AppContext,
    request: ExplainRequest,
) -> Result<AppResponse, AppError> {
    doctor::build_explain_response(context, request.skill.as_str())
}

/// Handle `skillctl inspect`.
pub fn handle_inspect(
    context: &AppContext,
    request: InspectRequest,
) -> Result<AppResponse, AppError> {
    doctor::build_inspect_response(context, request.skill.as_str())
}

/// The managed skill one mutating command acts on.
#[derive(Clone, Debug)]
pub(crate) struct ManagedImportTarget {
    /// Managed skill identity.
    pub(crate) skill: ManagedSkillRef,
    /// Recorded install from the local state store, when one exists.
    pub(crate) install: Option<crate::state::InstallRecord>,
    /// Which layer supplied the import identity.
    origin: ImportOrigin,
}

/// Which layer declared the import a mutating command resolved.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ImportOrigin {
    /// The local state store recorded the install.
    InstallRecord,
    /// The committed manifest declares the import and the lockfile pins it.
    ManifestAndLockfile,
    /// Only the committed lockfile pins the import.
    LockfileOnly,
}

impl ManagedImportTarget {
    /// Label the remove and toggle responses report under `resolved_from`.
    fn resolved_from(&self) -> &'static str {
        match self.origin {
            ImportOrigin::InstallRecord => "install-record",
            ImportOrigin::ManifestAndLockfile => "manifest-and-lockfile",
            ImportOrigin::LockfileOnly => "lockfile-only",
        }
    }

    /// Whether the install stopped tracking upstream lifecycle management.
    fn detached_from_upstream(&self) -> bool {
        self.install
            .as_ref()
            .is_some_and(|record| record.detached || record.forked)
    }

    /// Pre-state the mutation plan guards against.
    ///
    /// An install that was never recorded locally still carries dependency
    /// identity in the lockfile, so the plan fails before it mutates anything
    /// when that entry moved. An import the lockfile does not carry at all has
    /// no identity left to guard.
    fn expected_import(
        &self,
        context: &AppContext,
        lockfile: &WorkspaceLockfile,
    ) -> Result<Option<crate::apply::ExpectedImport>, AppError> {
        match (&self.install, lockfile.imports.get(&self.skill.skill_id)) {
            (Some(record), _) => Ok(Some(crate::apply::capture_expected_import(
                context, record,
            )?)),
            (None, Some(entry)) => Ok(Some(crate::apply::capture_expected_import_from_lockfile(
                &self.skill,
                entry,
            ))),
            (None, None) => Ok(None),
        }
    }

    /// Pre-state an enable or disable guards against.
    ///
    /// The toggle acts on managed imports only. A bundled install is locally
    /// owned rather than imported, a detached or forked install stopped
    /// tracking upstream lifecycle management, and an install without a
    /// lockfile entry is locally owned too, so none carries the dependency
    /// identity the toggle guards. A bundled install must not adopt the
    /// identity of an import that shares its name either, because the lockfile
    /// pins imports by name without a scope.
    fn toggle_expected_import(
        &self,
        context: &AppContext,
        lockfile: &WorkspaceLockfile,
    ) -> Result<Option<crate::apply::ExpectedImport>, AppError> {
        if self
            .install
            .as_ref()
            .is_some_and(builtin::is_bundled_install)
            || self.detached_from_upstream()
            || !lockfile.imports.contains_key(&self.skill.skill_id)
        {
            return Ok(None);
        }
        self.expected_import(context, lockfile)
    }
}

/// The mutating command an import resolution serves.
#[derive(Clone, Copy)]
pub(crate) enum ImportResolution {
    /// `remove`, which may also clear an entry only the lockfile pins.
    Remove,
    /// `enable` and `disable`, which need the manifest declaration to flip.
    Toggle,
    /// `override` and `fork`, which read the stored import source.
    Overlay,
}

impl ImportResolution {
    /// Label the divergence error names.
    fn action(self) -> &'static str {
        match self {
            Self::Remove => "removing",
            Self::Toggle => "enabling or disabling",
            Self::Overlay => "overriding or forking",
        }
    }

    /// Whether an entry the lockfile pins alone is a valid target.
    ///
    /// `remove` clears such an entry, because the `stale-lockfile-entry` state
    /// `doctor` reports has no other single-command repair. A toggle has no
    /// manifest declaration to flip, so it keeps refusing the entry. The
    /// overlay workflow also keeps refusing it, because it has no manifest
    /// declaration to act on either.
    fn accepts_lockfile_entry(self) -> bool {
        matches!(self, Self::Remove)
    }
}

/// Resolve the skill a mutating command acts on.
///
/// An install record is the normal source of truth. A checkout without one,
/// such as a fresh clone whose import cache and state store are still empty,
/// falls back to the import the committed manifest declares, so an import
/// that `restore` cannot reconstruct can still be replaced or toggled without
/// editing `.agents/skillctl.yaml` and `.agents/skillctl.lock` by hand.
///
/// A pin the manifest no longer declares resolves for `remove` alone. That is
/// the genuinely stale entry `doctor` and `explain` report, and clearing it
/// through `remove` replaces the reinstall-then-remove workaround.
pub(crate) fn resolve_managed_import(
    context: &AppContext,
    manifest: &WorkspaceManifest,
    lockfile: &WorkspaceLockfile,
    store: &LocalStateStore,
    skill: &str,
    resolution: ImportResolution,
) -> Result<ManagedImportTarget, AppError> {
    if let Some(managed_skill) = find_installed_skill(store, skill, context.selector.scope)? {
        let install = store.install_record(&managed_skill)?;
        return Ok(ManagedImportTarget {
            skill: managed_skill,
            install,
            origin: ImportOrigin::InstallRecord,
        });
    }

    let Some(managed_skill) = find_manifest_import(manifest, skill, context.selector.scope)? else {
        // The lockfile pins imports by name without a scope, so an entry no
        // declaration at all covers resolves as a workspace import. A manifest
        // declaration in another scope keeps the pin in use, and an explicit
        // user selector keeps refusing the entry instead of guessing.
        if !resolution.accepts_lockfile_entry()
            || find_manifest_import(manifest, skill, None)?.is_some()
            || context.selector.scope == Some(Scope::User)
            || !lockfile.imports.contains_key(skill)
        {
            return Err(AppError::ResolutionValidation {
                message: format!(
                    "skill '{skill}' is not installed and no manifest import declares it"
                ),
            });
        }
        return Ok(ManagedImportTarget {
            skill: ManagedSkillRef::new(ManagedScope::Workspace, skill),
            install: None,
            origin: ImportOrigin::LockfileOnly,
        });
    };
    if !lockfile.imports.contains_key(skill) {
        return Err(AppError::ResolutionValidation {
            message: format!(
                "import '{skill}' is declared by the manifest but missing from the lockfile; \
                 run 'skillctl doctor' to inspect the divergence before {} it",
                resolution.action()
            ),
        });
    }
    Ok(ManagedImportTarget {
        skill: managed_skill,
        install: None,
        origin: ImportOrigin::ManifestAndLockfile,
    })
}

/// Handle `skillctl enable`.
pub fn handle_enable(
    context: &AppContext,
    request: EnableRequest,
) -> Result<AppResponse, AppError> {
    {
        let store = LocalStateStore::open_default_for(&context.working_directory)?;
        match classify_bundled_request(
            context,
            &store,
            request.skill.as_str(),
            context.selector.scope,
        )? {
            // The bundled skill is the only thing enable can act on, but an
            // unscoped request may also name a stale lockfile entry that shares
            // the bundled name. Enabling the bundled skill would report success
            // for a name the stale entry still pins, so the request stays
            // ambiguous until a scope selector resolves it.
            BundledRequest::Bundled
                if context.selector.scope.is_none()
                    && names_undeclared_lockfile_entry(context, request.skill.as_str())? =>
            {
                return Err(bundled_enable_and_stale_pin_error(request.skill.as_str()));
            }
            BundledRequest::Bundled => return builtin::handle_enable(context),
            BundledRequest::Ambiguous => return Err(multiple_scopes_error(request.skill.as_str())),
            BundledRequest::Managed => {}
        }
    }
    toggle_managed_import(context, request.skill.as_str(), true, "enable")
}

/// Handle `skillctl disable`.
pub fn handle_disable(
    context: &AppContext,
    request: DisableRequest,
) -> Result<AppResponse, AppError> {
    {
        let store = LocalStateStore::open_default_for(&context.working_directory)?;
        match classify_bundled_request(
            context,
            &store,
            request.skill.as_str(),
            context.selector.scope,
        )? {
            // Disable toggles a manifest import, and the bundled skill is
            // locally owned, so its only lifecycle exit is an explicit remove.
            BundledRequest::Bundled => return Err(bundled_disable_error(request.skill.as_str())),
            BundledRequest::Ambiguous => return Err(multiple_scopes_error(request.skill.as_str())),
            BundledRequest::Managed => {}
        }
    }
    toggle_managed_import(context, request.skill.as_str(), false, "disable")
}

/// The refusal a bundled skill gets from `skillctl disable`.
///
/// Without this gate, a checkout that also declares an import under the bundled
/// skill's name resolves the toggle to the bundled install record and pairs it
/// with that import's lockfile entry, so the guarded plan always reads as stale.
fn bundled_disable_error(skill: &str) -> AppError {
    AppError::ResolutionValidation {
        message: format!(
            "skill '{skill}' is the bundled user-scope skill, which disable cannot toggle because \
             it is not a managed import; run 'skillctl --scope user remove {skill}' to stop \
             projecting it"
        ),
    }
}

/// Whether the lockfile pins the name while the manifest does not declare it.
///
/// That is the `stale-lockfile-entry` state `remove` clears.
fn names_undeclared_lockfile_entry(context: &AppContext, skill: &str) -> Result<bool, AppError> {
    let manifest = load_manifest_or_default(&context.working_directory)?;
    if find_manifest_import(&manifest, skill, None)?.is_some() {
        return Ok(false);
    }
    let lockfile = load_lockfile_or_default(&context.working_directory)?;
    Ok(lockfile.imports.contains_key(skill))
}

/// The refusal an unscoped bundled-name removal gets when the lockfile also
/// pins the name without a manifest declaration.
///
/// Routing the request to the bundled skill would silently drop an unrelated
/// skill while the stale entry the user asked about stays pinned.
fn bundled_name_and_stale_pin_error(skill: &str) -> AppError {
    AppError::ResolutionValidation {
        message: format!(
            "skill '{skill}' names both the bundled user-scope skill and a lockfile entry the \
             workspace manifest does not declare; re-run with --scope user to remove the bundled \
             skill, or --scope workspace to clear the entry"
        ),
    }
}

/// The refusal an unscoped bundled-name enable gets when the lockfile also
/// pins the name without a manifest declaration.
///
/// Routing the request to the bundled skill reports success for a name the
/// stale entry still pins, so the request stays ambiguous until a scope
/// selector resolves it.
fn bundled_enable_and_stale_pin_error(skill: &str) -> AppError {
    AppError::ResolutionValidation {
        message: format!(
            "skill '{skill}' names both the bundled user-scope skill and a lockfile entry the \
             workspace manifest does not declare; re-run with --scope user to enable the bundled \
             skill, or run 'skillctl --scope workspace remove {skill}' to clear the entry"
        ),
    }
}

/// Handle `skillctl path`.
///
/// An install record answers first. A canonical local skill in the workspace
/// skills root has no install record, because the state store only records
/// imports, so a name without one falls back to that root. `explain` resolves
/// the same skills, and the README lists `path` among the four commands to
/// remember, so the two commands must not disagree about a canonical skill.
pub fn handle_path(context: &AppContext, request: PathRequest) -> Result<AppResponse, AppError> {
    let manifest = load_manifest_or_default(&context.working_directory)?;
    let lockfile = load_lockfile_or_default(&context.working_directory)?;
    let store = LocalStateStore::open_default_for(&context.working_directory)?;
    let Some(managed_skill) =
        find_installed_skill(&store, request.skill.as_str(), context.selector.scope)?
    else {
        return canonical_local_path(context, &manifest, &store, &request);
    };
    let install_record =
        store
            .install_record(&managed_skill)?
            .ok_or_else(|| AppError::ResolutionValidation {
                message: format!(
                    "skill '{}' does not have an installed state record",
                    request.skill.as_str()
                ),
            })?;
    let snapshot = store.skill_snapshot(&managed_skill)?;
    let managed_import = managed_import(&manifest, &managed_skill);
    let lockfile_entry = managed_import.and_then(|import| lockfile.imports.get(&import.id));
    let planned_roots = if builtin::is_bundled_install(&install_record) {
        builtin::planned_root_views(context)?
    } else {
        planned_root_views(
            context,
            &manifest,
            managed_skill.scope,
            request.skill.as_str(),
        )?
    };

    let data = json!({
        "skill": request.skill.as_str(),
        "scope": managed_skill.scope.as_str(),
        "builtin": builtin::is_bundled_install(&install_record),
        "managed_import": managed_import.is_some(),
        "managed_import_enabled": managed_import.map(|import| import.enabled),
        "stored_source_root": lockfile_entry
            .map(|_| stored_import_root(managed_skill.scope, &context.working_directory, request.skill.as_str()))
            .transpose()?
            .map(|root| crate::planner::portable_path(&root)),
        "active_source_root": lockfile_entry
            .map(|entry| stored_import_root(managed_skill.scope, &context.working_directory, request.skill.as_str()).map(|root| root.join(entry.source.subpath.as_str())))
            .transpose()?
            .map(|path| crate::planner::portable_path(&path)),
        "overlay_root": managed_import
            .and_then(|import| manifest.overrides.get(&import.id))
            .map(|path| path.as_str().to_string()),
        "canonical_root": canonical_local_root(context, &manifest, request.skill.as_str())
            .filter(|_| managed_import.is_none() && managed_skill.scope == ManagedScope::Workspace)
            .map(|root| planner::display_path(context, &root)),
        "source": {
            "type": source_type_label(&install_record),
            "url": install_record.source_url,
            "subpath": install_record.source_subpath,
        },
        "resolved_revision": install_record.resolved_revision,
        "effective_version_hash": install_record.effective_version_hash,
        "planned_roots": planned_roots,
        "projections": snapshot
            .projections
            .iter()
            .map(|projection| planner::projection_view(context, projection))
            .collect::<Result<Vec<_>, _>>()?,
    });
    let detail = crate::human::path_detail_lines(&data);

    Ok(AppResponse::success("path")
        .with_summary(format!(
            "Resolved filesystem paths for {}.",
            request.skill.as_str()
        ))
        .with_detail_lines(detail)
        .with_data(data))
}

/// The `skillctl path` report for a name no install record covers.
///
/// A canonical local skill in the workspace skills root never enters the state
/// store, so this is the branch that answers for it: its physical locations are
/// the canonical root and the roots the workspace plan projects it into. An
/// explicit user-scope selector keeps refusing, because canonical local skills
/// belong to the workspace and the selector narrows the request.
fn canonical_local_path(
    context: &AppContext,
    manifest: &WorkspaceManifest,
    store: &LocalStateStore,
    request: &PathRequest,
) -> Result<AppResponse, AppError> {
    let skill = request.skill.as_str();
    if context.selector.scope == Some(Scope::User) {
        return Err(path_refusal(context, manifest, skill));
    }
    let Some(canonical_root) = canonical_local_root(context, manifest, skill) else {
        return Err(path_refusal(context, manifest, skill));
    };
    if materialize::is_skillctl_generated_directory(&canonical_root)? {
        // The directory holds a generated copy of some install, not a skill
        // someone authored, so it is not a canonical root the command reports.
        return Err(AppError::ResolutionValidation {
            message: format!(
                "skill '{skill}' has no install record and '{}' is a generated projection rather \
                 than a canonical local skill; run 'skillctl explain {skill}' to see every \
                 candidate it found",
                planner::display_path(context, &canonical_root),
            ),
        });
    }

    // Canonical local skills keep no projection rows: sync materializes the
    // plan straight from the canonical root, and only installs carry records.
    let snapshot = store.skill_snapshot(&ManagedSkillRef::new(ManagedScope::Workspace, skill))?;
    let planned_roots = planned_root_views(context, manifest, ManagedScope::Workspace, skill)?;
    let data = json!({
        "skill": skill,
        "scope": ManagedScope::Workspace.as_str(),
        "builtin": false,
        "managed_import": false,
        "managed_import_enabled": None::<bool>,
        "stored_source_root": None::<String>,
        "active_source_root": None::<String>,
        "overlay_root": None::<String>,
        "canonical_root": planner::display_path(context, &canonical_root),
        "source": {
            "type": "canonical-local",
            "url": None::<String>,
            "subpath": None::<String>,
        },
        "resolved_revision": None::<String>,
        "effective_version_hash": None::<String>,
        "planned_roots": planned_roots,
        "projections": snapshot
            .projections
            .iter()
            .map(|projection| planner::projection_view(context, projection))
            .collect::<Result<Vec<_>, _>>()?,
    });
    let detail = crate::human::path_detail_lines(&data);

    Ok(AppResponse::success("path")
        .with_summary(format!("Resolved filesystem paths for {skill}."))
        .with_detail_lines(detail)
        .with_data(data))
}

/// The canonical local root of one workspace skill, when the directory exists.
pub(crate) fn canonical_local_root(
    context: &AppContext,
    manifest: &WorkspaceManifest,
    skill: &str,
) -> Option<PathBuf> {
    let root = context
        .working_directory
        .join(manifest.layout.skills_dir.as_str())
        .join(skill);
    fs::metadata(&root)
        .ok()
        .filter(std::fs::Metadata::is_dir)
        .map(|_| root)
}

/// The refusal `skillctl path` reports for a name it cannot place on disk.
///
/// The message names the roots the command consulted, because a bare "not
/// installed" hides that `path` also answers for canonical local skills.
/// `explain` is the command that reports every candidate it found, shadowed or
/// not, so the refusal hands the name over to it.
fn path_refusal(context: &AppContext, manifest: &WorkspaceManifest, skill: &str) -> AppError {
    let skills_root = planner::display_path(
        context,
        &context
            .working_directory
            .join(manifest.layout.skills_dir.as_str()),
    );
    let message = if context.selector.scope == Some(Scope::User) {
        format!(
            "skill '{skill}' is not installed at user scope and canonical local skills live in \
             the workspace at '{skills_root}'; run 'skillctl explain {skill}' to see every \
             candidate it found",
        )
    } else {
        format!(
            "skill '{skill}' is not installed and no canonical local skill exists at \
             '{skills_root}/{skill}'; run 'skillctl explain {skill}' to see every candidate it \
             found",
        )
    };
    AppError::ResolutionValidation { message }
}

pub(crate) fn load_manifest_or_default(
    working_directory: &Path,
) -> Result<WorkspaceManifest, AppError> {
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

pub(crate) fn load_lockfile_or_default(
    working_directory: &Path,
) -> Result<WorkspaceLockfile, AppError> {
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

pub(crate) fn resolve_installed_skill(
    store: &LocalStateStore,
    skill: &str,
    scope: Option<Scope>,
) -> Result<ManagedSkillRef, AppError> {
    find_installed_skill(store, skill, scope)?.ok_or_else(|| AppError::ResolutionValidation {
        message: format!("skill '{}' is not installed", skill),
    })
}

/// Find the recorded install for one skill name, or `None` when absent.
fn find_installed_skill(
    store: &LocalStateStore,
    skill: &str,
    scope: Option<Scope>,
) -> Result<Option<ManagedSkillRef>, AppError> {
    let scopes = match scope {
        Some(Scope::Workspace) => vec![ManagedScope::Workspace],
        Some(Scope::User) => vec![ManagedScope::User],
        None => vec![ManagedScope::Workspace, ManagedScope::User],
    };
    let mut matches = Vec::new();

    for managed_scope in scopes {
        let candidate = ManagedSkillRef::new(managed_scope, skill);
        if store.install_record(&candidate)?.is_some() {
            matches.push(candidate);
        }
    }

    match matches.len() {
        1 => Ok(Some(matches.remove(0))),
        0 => Ok(None),
        _ => Err(multiple_scopes_error(skill)),
    }
}

/// The shared resolution error for a skill name no scope selector narrows.
pub(crate) fn multiple_scopes_error(skill: &str) -> AppError {
    AppError::ResolutionValidation {
        message: format!("skill '{skill}' exists in multiple scopes; re-run with --scope"),
    }
}

/// How a lifecycle request relates to the bundled user-scope skill.
///
/// A repository may commit a workspace import under the bundled skill's own
/// name, which is this repository's configuration. Such a request stays
/// ambiguous until a `--scope` selector resolves it, so a mutating command must
/// refuse it instead of guessing the bundled skill and silently leaving the
/// import in place.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BundledRequest {
    /// The request names the bundled user-scope skill.
    Bundled,
    /// The request names a managed import that normal resolution owns.
    Managed,
    /// Both scopes claim the name and no selector resolves it.
    Ambiguous,
}

/// Decide which skill a request that matches the bundled skill's name names.
///
/// The install record decides when one exists. A workspace import the manifest
/// declares also claims the name before any local record for it exists, which
/// is the state a fresh clone of such a repository is in.
pub(crate) fn classify_bundled_request(
    context: &AppContext,
    store: &LocalStateStore,
    skill: &str,
    scope: Option<Scope>,
) -> Result<BundledRequest, AppError> {
    if !builtin::is_bundled_request(skill, scope) {
        return Ok(BundledRequest::Managed);
    }

    let import_claims_name = if scope.is_none() {
        let manifest = load_manifest_or_default(&context.working_directory)?;
        find_manifest_import(&manifest, skill, Some(Scope::Workspace))?.is_some()
    } else {
        false
    };

    match find_installed_skill(store, skill, scope)? {
        Some(managed_skill) => {
            let is_bundled_install = store
                .install_record(&managed_skill)?
                .is_some_and(|install| builtin::is_bundled_install(&install));
            match (is_bundled_install, import_claims_name) {
                (true, true) => Ok(BundledRequest::Ambiguous),
                (true, false) => Ok(BundledRequest::Bundled),
                (false, _) => Ok(BundledRequest::Managed),
            }
        }
        None => Ok(if import_claims_name {
            BundledRequest::Ambiguous
        } else {
            BundledRequest::Bundled
        }),
    }
}

/// Find a declared import in the committed manifest, or `None` when absent.
///
/// A fresh checkout carries `.agents/skillctl.yaml` and
/// `.agents/skillctl.lock` but no install record, because the state store is
/// local to each machine. Repair commands still work there: they resolve the
/// import from that portable state instead of the local record.
fn find_manifest_import(
    manifest: &WorkspaceManifest,
    skill: &str,
    scope: Option<Scope>,
) -> Result<Option<ManagedSkillRef>, AppError> {
    let scopes: Vec<ManagedScope> = manifest
        .imports
        .iter()
        .filter(|import| import.id == skill)
        .filter(|import| match scope {
            Some(Scope::Workspace) => import.scope == ManifestScope::Workspace,
            Some(Scope::User) => import.scope == ManifestScope::User,
            None => true,
        })
        .map(|import| ManagedScope::from(import.scope))
        .collect();

    match scopes.as_slice() {
        [scope] => Ok(Some(ManagedSkillRef::new(*scope, skill))),
        [] => Ok(None),
        _ => Err(multiple_scopes_error(skill)),
    }
}

fn source_type_label(install: &crate::state::InstallRecord) -> &'static str {
    if builtin::is_bundled_install(install) {
        "built-in"
    } else {
        install.source_kind.label()
    }
}

fn planned_root_views(
    context: &AppContext,
    manifest: &WorkspaceManifest,
    scope: ManagedScope,
    skill: &str,
) -> Result<Vec<serde_json::Value>, AppError> {
    if manifest.targets.is_empty() {
        return Ok(Vec::new());
    }

    let plan = planner::plan_target_roots(
        &AdapterRegistry::new(),
        TargetScope::from(scope),
        manifest.projection.policy,
        &manifest.targets,
        &manifest.adapters,
    )?;

    plan.assignments
        .into_iter()
        .map(|assignment| {
            let root = planner::resolve_runtime_root_path(context, &assignment.root)?;
            let path = root.join(skill);
            Ok(json!({
                "target": assignment.target,
                "root": planner::display_path(context, &root),
                "path": planner::display_path(context, &path),
                "source": assignment.source,
            }))
        })
        .collect()
}

/// The report a mutation returns when it could not rebuild projections.
///
/// Resolution reads every enabled import, so a checkout that still holds one
/// it never reconstructed cannot run a rebuild at all. A mutation that stays
/// correct without one reports the skip instead of refusing. The named `sync`
/// is qualified for `scope`, the scope the rebuild would have run in.
fn skipped_projection_report(unrestored: &[String], scope: ManagedScope) -> serde_json::Value {
    json!({
        "skipped": true,
        "unrestored_imports": unrestored,
        "detail": format!(
            "the projection rebuild was skipped because enabled imports have no stored \
             source in this checkout; run 'skillctl restore --frozen' or remove them, \
             then run '{}'",
            crate::apply::scoped_sync_command(scope)
        ),
    })
}

/// The warning that names the imports whose missing stored source stopped a
/// projection rebuild.
fn skipped_projection_warning(unrestored: &[String], scope: ManagedScope) -> String {
    format!(
        "skipped the projection rebuild because enabled import{} {} ha{} no stored \
         source in this checkout; run 'skillctl restore --frozen' or remove {}, then \
         run '{}'",
        if unrestored.len() == 1 { "" } else { "s" },
        unrestored
            .iter()
            .map(|name| format!("'{name}'"))
            .collect::<Vec<_>>()
            .join(", "),
        if unrestored.len() == 1 { "s" } else { "ve" },
        if unrestored.len() == 1 {
            "it".to_string()
        } else {
            "them".to_string()
        },
        crate::apply::scoped_sync_command(scope),
    )
}

fn toggle_managed_import(
    context: &AppContext,
    skill: &str,
    enabled: bool,
    command: &'static str,
) -> Result<AppResponse, AppError> {
    let manifest = load_manifest_or_default(&context.working_directory)?;
    let lockfile = load_lockfile_or_default(&context.working_directory)?;
    let store = LocalStateStore::open_default_for(&context.working_directory)?;
    let target = resolve_managed_import(
        context,
        &manifest,
        &lockfile,
        &store,
        skill,
        ImportResolution::Toggle,
    )?;
    let managed_skill = target.skill.clone();
    // Enable and disable change local bookkeeping only, so the plan requires
    // the recorded bundle identity to survive the mutation unchanged.
    let expected = target
        .toggle_expected_import(context, &lockfile)?
        .ok_or_else(|| AppError::ResolutionValidation {
            message: format!("skill '{skill}' is not a managed import"),
        })?;

    let scoped_context = history::context_for_scope(context, managed_skill.scope);
    let plan = crate::apply::MutationPlanBuilder::new(
        crate::apply::MutationOperation::Enable.or_disable(enabled),
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
            "{} '{}' in {} scope without touching its bundle identity",
            if enabled { "enable" } else { "disable" },
            managed_skill.skill_id,
            managed_skill.scope.as_str()
        ),
    })
    .target(manifest.path.clone(), crate::apply::TargetAction::Update)
    .targets(
        materialize::sync_mutation_root_paths(
            &scoped_context,
            &manifest,
            TargetScope::from(managed_skill.scope),
        )?,
        crate::apply::TargetAction::Replace,
    )
    .seal();

    crate::apply::run_guarded(&plan, context, |transaction| {
        transaction.track_state_database()?;
        let mut manifest = load_manifest_or_default(&context.working_directory)?;
        let lockfile = load_lockfile_or_default(&context.working_directory)?;
        let mut store = LocalStateStore::open_default_for(&context.working_directory)?;
        let target = resolve_managed_import(
            context,
            &manifest,
            &lockfile,
            &store,
            skill,
            ImportResolution::Toggle,
        )?;
        let managed_skill = target.skill.clone();
        let scoped_context = history::context_for_scope(context, managed_skill.scope);
        let import_index = manifest_import_index(&manifest, &managed_skill).ok_or_else(|| {
            AppError::ResolutionValidation {
                message: format!(
                    "skill '{skill}' is not a managed import in the workspace manifest"
                ),
            }
        })?;
        let changed = manifest.imports[import_index].enabled != enabled;
        manifest.imports[import_index].enabled = enabled;
        manifest.write_to_path()?;

        let timestamp = current_timestamp();
        // Projection rebuild reads every enabled import, so a checkout that
        // still holds an import it never reconstructed cannot run one. The
        // enablement change is still correct without it, and refusing it
        // would leave a fresh clone unable to stop projecting an import it
        // cannot restore.
        let unrestored = materialize::unrestored_imports(context, &manifest)?;
        let sync_report = if unrestored.is_empty() {
            Some(materialize::sync_workspace(&scoped_context)?)
        } else {
            None
        };
        let current_projections = match &sync_report {
            Some(report) => materialize::rebuild_projection_records_for_scope(
                &mut store,
                managed_skill.scope,
                report,
                &timestamp,
            )?,
            None => Vec::new(),
        };

        {
            let mut ledger = HistoryLedger::new(&mut store);
            if let Some(report) = &sync_report {
                materialize::record_pruned_projection_history(
                    &mut ledger,
                    context,
                    managed_skill.scope,
                    report,
                    &timestamp,
                )?;
            }
            for record in current_projections
                .iter()
                .filter(|record| record.skill == managed_skill)
            {
                ledger.record_projection(record)?;
            }
        }

        let projection = match &sync_report {
            Some(report) => {
                serde_json::to_value(report).map_err(|source| AppError::JsonRender { source })?
            }
            None => skipped_projection_report(&unrestored, managed_skill.scope),
        };

        let mut response = AppResponse::success(command)
            .with_summary(format!(
                "{} {} in {} scope.",
                if enabled { "Enabled" } else { "Disabled" },
                skill,
                managed_skill.scope.as_str()
            ))
            .with_data(json!({
                "skill": skill,
                "scope": managed_skill.scope.as_str(),
                "enabled": enabled,
                "changed": changed,
                "resolved_from": target.resolved_from(),
                "bundle_digest": plan.expected.first().and_then(|expected| expected.bundle_digest.clone()),
                "projection": projection,
            }));
        if target.origin == ImportOrigin::ManifestAndLockfile {
            response = response.with_warning(format!(
                "no install record for '{skill}' existed in this checkout, so {command} acted \
                 on the import declared by .agents/skillctl.yaml and .agents/skillctl.lock"
            ));
        }
        if !unrestored.is_empty() {
            response =
                response.with_warning(skipped_projection_warning(&unrestored, managed_skill.scope));
        }
        if let Some(report) = &sync_report {
            for warning in &report.warnings {
                response = response.with_warning(warning.clone());
            }
        }

        Ok(response)
    })
}

fn managed_import<'a>(
    manifest: &'a WorkspaceManifest,
    skill: &ManagedSkillRef,
) -> Option<&'a ImportDefinition> {
    manifest
        .imports
        .iter()
        .find(|import| import.id == skill.skill_id && import.scope == manifest_scope(skill.scope))
}

fn manifest_import_index(manifest: &WorkspaceManifest, skill: &ManagedSkillRef) -> Option<usize> {
    manifest.imports.iter().position(|import| {
        import.id == skill.skill_id && import.scope == manifest_scope(skill.scope)
    })
}

fn manifest_scope(scope: ManagedScope) -> ManifestScope {
    match scope {
        ManagedScope::Workspace => ManifestScope::Workspace,
        ManagedScope::User => ManifestScope::User,
    }
}

fn ensure_directory(path: &Path) -> Result<(), AppError> {
    let metadata = fs::metadata(path).map_err(|source| AppError::FilesystemOperation {
        action: "inspect skill directory",
        path: path.to_path_buf(),
        source,
    })?;

    if !metadata.is_dir() {
        return Err(AppError::PathConflict {
            path: path.to_path_buf(),
            expected: "directory",
        });
    }

    Ok(())
}

fn read_skill_manifest(path: &Path) -> Result<String, AppError> {
    match fs::read_to_string(path) {
        Ok(contents) => Ok(contents),
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            Err(skill_validation(path, "SKILL.md does not exist"))
        }
        Err(source) => Err(AppError::FilesystemOperation {
            action: "read skill manifest",
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// Return the frontmatter block of a `SKILL.md` document.
///
/// The markdown body is validated only for its closing delimiter and is not
/// retained: overlays and projections copy the raw file bytes, so no consumer
/// needs the parsed body.
fn frontmatter_source(source: &str, skill_path: &Path) -> Result<String, AppError> {
    let source = source.strip_prefix('\u{feff}').unwrap_or(source);
    let mut lines = source.lines();

    if lines.next() != Some("---") {
        return Err(skill_validation(
            skill_path,
            "SKILL.md must start with a '---' frontmatter delimiter",
        ));
    }

    let mut frontmatter_lines = Vec::new();
    let mut closed = false;

    for line in lines {
        if !closed && line == "---" {
            closed = true;
            continue;
        }

        if !closed {
            frontmatter_lines.push(line);
        }
    }

    if !closed {
        return Err(skill_validation(
            skill_path,
            "SKILL.md frontmatter must end with a closing '---' delimiter",
        ));
    }

    Ok(frontmatter_lines.join("\n"))
}

fn parse_frontmatter(
    frontmatter_source: &str,
    skill_path: &Path,
) -> Result<BTreeMap<String, Value>, AppError> {
    let parsed = serde_yaml::from_str::<Value>(frontmatter_source).map_err(|source| {
        AppError::SkillParse {
            path: skill_path.to_path_buf(),
            source,
        }
    })?;

    let mapping = parsed.as_mapping().ok_or_else(|| {
        skill_validation(skill_path, "SKILL.md frontmatter must be a YAML mapping")
    })?;

    let mut fields = BTreeMap::new();
    for (key, value) in mapping {
        let key = key.as_str().ok_or_else(|| {
            skill_validation(skill_path, "SKILL.md frontmatter keys must be strings")
        })?;
        fields.insert(key.to_string(), value.clone());
    }

    Ok(fields)
}

fn require_string_field<'a>(
    fields: &'a BTreeMap<String, Value>,
    field: &str,
    skill_path: &Path,
) -> Result<&'a str, AppError> {
    let value = fields.get(field).ok_or_else(|| {
        skill_validation(
            skill_path,
            format!("SKILL.md frontmatter must define '{field}'"),
        )
    })?;

    value.as_str().ok_or_else(|| {
        skill_validation(
            skill_path,
            format!("SKILL.md field '{field}' must be a string"),
        )
    })
}

fn validate_skill_name(value: &str, skill_path: &Path) -> Result<(), AppError> {
    if let Some(message) = skill_name_format_error(value) {
        return Err(skill_validation(
            skill_path,
            format!("SKILL.md field 'name' {message}"),
        ));
    }

    Ok(())
}

/// Return the reason a skill name violates the open Agent Skills naming rule.
///
/// The same rule governs `SKILL.md` frontmatter names, skill directory names,
/// and the `name` field of remote discovery index entries, so every caller
/// shares one definition of a valid skill name.
pub fn skill_name_format_error(value: &str) -> Option<String> {
    if value.is_empty() {
        return Some("must not be empty".to_string());
    }
    if value.len() > 64 {
        return Some(format!(
            "must be at most 64 characters, found {}",
            value.len()
        ));
    }
    if value.starts_with('-') || value.ends_with('-') || value.contains("--") {
        return Some(format!(
            "must use lowercase letters, digits, and single hyphens: '{value}'"
        ));
    }
    if !value
        .bytes()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Some(format!(
            "must use lowercase letters, digits, and hyphens only: '{value}'"
        ));
    }

    None
}

fn validate_description(value: &str, skill_path: &Path) -> Result<(), AppError> {
    if value.trim().is_empty() {
        return Err(skill_validation(
            skill_path,
            "SKILL.md field 'description' must not be empty",
        ));
    }
    let characters = value.chars().count();
    if characters > 1_024 {
        return Err(skill_validation(
            skill_path,
            format!(
                "SKILL.md field 'description' must be at most 1024 characters, found \
                 {characters}"
            ),
        ));
    }

    Ok(())
}

fn validate_directory_name(root: &Path, name: &str, skill_path: &Path) -> Result<(), AppError> {
    let directory_name = root
        .file_name()
        .and_then(|segment| segment.to_str())
        .ok_or_else(|| {
            skill_validation(
                skill_path,
                format!(
                    "skill directory '{}' must end in a valid UTF-8 directory name",
                    crate::planner::portable_path(root)
                ),
            )
        })?;

    if directory_name != name {
        return Err(skill_validation(
            skill_path,
            format!(
                "SKILL.md field 'name' must match the parent directory '{}', found '{}'",
                directory_name, name
            ),
        ));
    }

    Ok(())
}

fn validate_optional_standard_fields(
    fields: &BTreeMap<String, Value>,
    skill_path: &Path,
) -> Result<(), AppError> {
    for field in ["license", "compatibility", "allowed-tools"] {
        if let Some(value) = fields.get(field)
            && !value.is_string()
        {
            return Err(skill_validation(
                skill_path,
                format!("SKILL.md field '{field}' must be a string when present"),
            ));
        }
    }

    if let Some(value) = fields.get("metadata") {
        let mapping = value.as_mapping().ok_or_else(|| {
            skill_validation(
                skill_path,
                "SKILL.md field 'metadata' must be a YAML mapping when present",
            )
        })?;

        for key in mapping.keys() {
            if !key.is_string() {
                return Err(skill_validation(
                    skill_path,
                    "SKILL.md field 'metadata' must use string keys",
                ));
            }
        }
    }

    if let Some(value) = fields.get("capabilities") {
        let sequence = value.as_sequence().ok_or_else(|| {
            skill_validation(
                skill_path,
                "SKILL.md field 'capabilities' must be a YAML sequence of strings when present",
            )
        })?;

        for capability in sequence {
            let Some(capability) = capability.as_str() else {
                return Err(skill_validation(
                    skill_path,
                    "SKILL.md field 'capabilities' must contain only strings",
                ));
            };

            if capability.trim().is_empty() {
                return Err(skill_validation(
                    skill_path,
                    "SKILL.md field 'capabilities' must not contain empty strings",
                ));
            }
        }
    }

    if let Some(value) = fields.get("credentials") {
        let sequence = value.as_sequence().ok_or_else(|| {
            skill_validation(
                skill_path,
                "SKILL.md field 'credentials' must be a YAML sequence of mappings when present",
            )
        })?;

        for credential in sequence {
            let Some(mapping) = credential.as_mapping() else {
                return Err(skill_validation(
                    skill_path,
                    "SKILL.md field 'credentials' must contain only mappings",
                ));
            };

            let mut name_present = false;
            for (key, value) in mapping {
                let Some(key) = key.as_str() else {
                    return Err(skill_validation(
                        skill_path,
                        "SKILL.md field 'credentials' must use string keys",
                    ));
                };

                match key {
                    "name" => {
                        let Some(name) = value.as_str() else {
                            return Err(skill_validation(
                                skill_path,
                                "SKILL.md credential field 'name' must be a string",
                            ));
                        };
                        if name.trim().is_empty() {
                            return Err(skill_validation(
                                skill_path,
                                "SKILL.md credential field 'name' must not be empty",
                            ));
                        }
                        name_present = true;
                    }
                    "optional" => {
                        if !value.is_bool() {
                            return Err(skill_validation(
                                skill_path,
                                "SKILL.md credential field 'optional' must be a boolean when present",
                            ));
                        }
                    }
                    "purpose" => {
                        let Some(purpose) = value.as_str() else {
                            return Err(skill_validation(
                                skill_path,
                                "SKILL.md credential field 'purpose' must be a string when present",
                            ));
                        };
                        if purpose.trim().is_empty() {
                            return Err(skill_validation(
                                skill_path,
                                "SKILL.md credential field 'purpose' must not be empty when present",
                            ));
                        }
                    }
                    _ => {}
                }
            }

            if !name_present {
                return Err(skill_validation(
                    skill_path,
                    "SKILL.md credential entries must define a 'name'",
                ));
            }
        }
    }

    if let Some(value) = fields.get("dependencies") {
        let mapping = value.as_mapping().ok_or_else(|| {
            skill_validation(
                skill_path,
                "SKILL.md field 'dependencies' must be a YAML mapping when present",
            )
        })?;

        for (key, value) in mapping {
            let Some(key) = key.as_str() else {
                return Err(skill_validation(
                    skill_path,
                    "SKILL.md field 'dependencies' must use string keys",
                ));
            };

            if key != "tools" {
                continue;
            }

            let sequence = value.as_sequence().ok_or_else(|| {
                skill_validation(
                    skill_path,
                    "SKILL.md field 'dependencies.tools' must be a YAML sequence when present",
                )
            })?;

            for tool in sequence {
                match tool {
                    Value::String(name) => {
                        if name.trim().is_empty() {
                            return Err(skill_validation(
                                skill_path,
                                "SKILL.md dependency tool entries must not be empty strings",
                            ));
                        }
                    }
                    Value::Mapping(tool_mapping) => {
                        let mut name_present = false;
                        for (tool_key, tool_value) in tool_mapping {
                            let Some(tool_key) = tool_key.as_str() else {
                                return Err(skill_validation(
                                    skill_path,
                                    "SKILL.md dependency tool entries must use string keys",
                                ));
                            };

                            match tool_key {
                                "name" => {
                                    let Some(name) = tool_value.as_str() else {
                                        return Err(skill_validation(
                                            skill_path,
                                            "SKILL.md dependency tool field 'name' must be a string",
                                        ));
                                    };
                                    if name.trim().is_empty() {
                                        return Err(skill_validation(
                                            skill_path,
                                            "SKILL.md dependency tool field 'name' must not be empty",
                                        ));
                                    }
                                    name_present = true;
                                }
                                "optional" => {
                                    if !tool_value.is_bool() {
                                        return Err(skill_validation(
                                            skill_path,
                                            "SKILL.md dependency tool field 'optional' must be a boolean when present",
                                        ));
                                    }
                                }
                                "purpose" => {
                                    let Some(purpose) = tool_value.as_str() else {
                                        return Err(skill_validation(
                                            skill_path,
                                            "SKILL.md dependency tool field 'purpose' must be a string when present",
                                        ));
                                    };
                                    if purpose.trim().is_empty() {
                                        return Err(skill_validation(
                                            skill_path,
                                            "SKILL.md dependency tool field 'purpose' must not be empty when present",
                                        ));
                                    }
                                }
                                _ => {}
                            }
                        }

                        if !name_present {
                            return Err(skill_validation(
                                skill_path,
                                "SKILL.md dependency tool entries must define a 'name'",
                            ));
                        }
                    }
                    _ => {
                        return Err(skill_validation(
                            skill_path,
                            "SKILL.md field 'dependencies.tools' must contain only strings or mappings",
                        ));
                    }
                }
            }
        }
    }

    Ok(())
}

fn parse_safety_summary(
    fields: &BTreeMap<String, Value>,
    _skill_path: &Path,
) -> Result<SkillSafetySummary, AppError> {
    let mut summary = SkillSafetySummary::default();
    let mut seen_capabilities = BTreeSet::new();
    let mut seen_credentials = BTreeSet::new();
    let mut seen_tools = BTreeSet::new();

    if let Some(sequence) = fields.get("capabilities").and_then(Value::as_sequence) {
        for capability in sequence {
            let capability = capability
                .as_str()
                .expect("capabilities were validated as strings")
                .trim()
                .to_string();
            if seen_capabilities.insert(capability.clone()) {
                summary.capabilities.push(capability);
            }
        }
    }

    if let Some(sequence) = fields.get("credentials").and_then(Value::as_sequence) {
        for credential in sequence {
            let mapping = credential
                .as_mapping()
                .expect("credentials were validated as mappings");
            let name = mapping
                .get(Value::String("name".to_string()))
                .and_then(Value::as_str)
                .expect("credentials were validated to include string names")
                .trim()
                .to_string();
            if !seen_credentials.insert(name.clone()) {
                continue;
            }

            let optional = mapping
                .get(Value::String("optional".to_string()))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let purpose = mapping
                .get(Value::String("purpose".to_string()))
                .and_then(Value::as_str)
                .map(|purpose| purpose.trim().to_string());

            summary.credentials.push(SkillCredentialRequirement {
                name,
                optional,
                purpose,
            });
        }
    }

    if let Some(mapping) = fields.get("dependencies").and_then(Value::as_mapping)
        && let Some(sequence) = mapping
            .get(Value::String("tools".to_string()))
            .and_then(Value::as_sequence)
    {
        for tool in sequence {
            let (name, optional, purpose) = match tool {
                Value::String(name) => (name.trim().to_string(), false, None),
                Value::Mapping(tool_mapping) => {
                    let name = tool_mapping
                        .get(Value::String("name".to_string()))
                        .and_then(Value::as_str)
                        .expect("dependency tools were validated to include string names")
                        .trim()
                        .to_string();
                    let optional = tool_mapping
                        .get(Value::String("optional".to_string()))
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    let purpose = tool_mapping
                        .get(Value::String("purpose".to_string()))
                        .and_then(Value::as_str)
                        .map(|purpose| purpose.trim().to_string());
                    (name, optional, purpose)
                }
                _ => unreachable!("dependency tools were validated"),
            };

            if !seen_tools.insert(name.clone()) {
                continue;
            }

            summary.dependencies.tools.push(SkillToolRequirement {
                name,
                optional,
                purpose,
            });
        }
    }

    Ok(summary)
}

fn extract_vendor_frontmatter(fields: &BTreeMap<String, Value>) -> BTreeMap<String, Value> {
    fields
        .iter()
        .filter(|(key, _)| !STANDARD_FRONTMATTER_FIELDS.contains(&key.as_str()))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

fn load_vendor_metadata(root: &Path) -> Result<SkillVendorMetadata, AppError> {
    let mut files = BTreeMap::new();
    let relative_path = PathBuf::from(OPENAI_METADATA_FILE);
    let path = root.join(&relative_path);

    match fs::metadata(&path) {
        Ok(metadata) => {
            if !metadata.is_file() {
                return Err(AppError::PathConflict {
                    path,
                    expected: "file",
                });
            }

            let contents =
                fs::read_to_string(&path).map_err(|source| AppError::FilesystemOperation {
                    action: "read vendor metadata file",
                    path: path.clone(),
                    source,
                })?;
            files.insert(relative_path, contents);
        }
        Err(source) if source.kind() == io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(AppError::FilesystemOperation {
                action: "inspect vendor metadata file",
                path,
                source,
            });
        }
    }

    Ok(SkillVendorMetadata { files })
}

fn skill_validation(path: &Path, message: impl Into<String>) -> AppError {
    AppError::SkillValidation {
        path: path.to_path_buf(),
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        env,
        time::{SystemTime, UNIX_EPOCH},
    };

    const VALID_SKILL: &str = concat!(
        "---\n",
        "name: release-notes\n",
        "description: Summarize a project's release notes.\n",
        "metadata:\n",
        "  owner: docs\n",
        "user-invocable: true\n",
        "context:\n",
        "  - repo\n",
        "hooks:\n",
        "  pre:\n",
        "    - echo prepare\n",
        "---\n",
        "\n",
        "# Release Notes\n",
        "Use this skill to summarize changes.\n",
    );

    #[test]
    fn load_from_dir_parses_skill_frontmatter_and_vendor_metadata() {
        let skill = TestSkillDir::new("release-notes");
        skill.write(SKILL_MANIFEST_FILE, VALID_SKILL);
        skill.write(
            OPENAI_METADATA_FILE,
            concat!(
                "model: gpt-5.4\n",
                "instructions: Keep summaries concise.\n"
            ),
        );

        let parsed = SkillDefinition::load_from_dir(skill.path()).expect("skill parses");

        assert_eq!(parsed.name, SkillName("release-notes".to_string()));
        assert_eq!(parsed.description, "Summarize a project's release notes.");
        assert_eq!(
            parsed.frontmatter.vendor_fields.get("user-invocable"),
            Some(&Value::Bool(true))
        );
        assert!(parsed.frontmatter.vendor_fields.contains_key("context"));
        assert!(parsed.frontmatter.vendor_fields.contains_key("hooks"));
        assert_eq!(
            parsed
                .vendor_metadata
                .files
                .get(&PathBuf::from(OPENAI_METADATA_FILE)),
            Some(&"model: gpt-5.4\ninstructions: Keep summaries concise.\n".to_string())
        );
    }

    #[test]
    fn load_from_dir_rejects_missing_skill_manifest() {
        let skill = TestSkillDir::new("release-notes");

        let error =
            SkillDefinition::load_from_dir(skill.path()).expect_err("missing SKILL.md is rejected");

        assert!(
            error.to_string().contains("SKILL.md does not exist"),
            "unexpected error: {error}"
        );
        assert_eq!(
            error.exit_status(),
            crate::error::ExitStatus::ValidationFailure
        );
    }

    #[test]
    fn load_from_dir_rejects_invalid_skill_name_format() {
        let skill = TestSkillDir::new("release-notes");
        skill.write(
            SKILL_MANIFEST_FILE,
            concat!(
                "---\n",
                "name: Release_Notes\n",
                "description: Summarize a project's release notes.\n",
                "---\n",
            ),
        );

        let error = SkillDefinition::load_from_dir(skill.path())
            .expect_err("invalid skill name is rejected");

        assert!(
            error
                .to_string()
                .contains("must use lowercase letters, digits, and hyphens only"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn load_from_dir_rejects_directory_mismatch() {
        let skill = TestSkillDir::new("release-notes");
        skill.write(
            SKILL_MANIFEST_FILE,
            concat!(
                "---\n",
                "name: bug-triage\n",
                "description: Summarize a project's release notes.\n",
                "---\n",
            ),
        );

        let error = SkillDefinition::load_from_dir(skill.path())
            .expect_err("mismatched directory name is rejected");

        assert!(
            error
                .to_string()
                .contains("must match the parent directory"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn load_from_dir_rejects_missing_description() {
        let skill = TestSkillDir::new("release-notes");
        skill.write(
            SKILL_MANIFEST_FILE,
            concat!("---\n", "name: release-notes\n", "---\n"),
        );

        let error = SkillDefinition::load_from_dir(skill.path())
            .expect_err("missing description is rejected");

        assert!(
            error.to_string().contains("must define 'description'"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn description_limit_counts_characters_not_bytes() {
        // 1024 CJK characters occupy 3072 UTF-8 bytes. The limit is 1024
        // characters, so a description at the limit must parse.
        let skill = TestSkillDir::new("release-notes");
        skill.write(
            SKILL_MANIFEST_FILE,
            &format!(
                "---\nname: release-notes\ndescription: {}\n---\n",
                "漢".repeat(1024)
            ),
        );

        let parsed = SkillDefinition::load_from_dir(skill.path())
            .expect("a description of 1024 characters parses");
        assert_eq!(parsed.description.chars().count(), 1024);

        let skill = TestSkillDir::new("release-notes");
        skill.write(
            SKILL_MANIFEST_FILE,
            &format!(
                "---\nname: release-notes\ndescription: {}\n---\n",
                "漢".repeat(1025)
            ),
        );

        let error = SkillDefinition::load_from_dir(skill.path())
            .expect_err("a description beyond 1024 characters is rejected");
        assert!(
            error
                .to_string()
                .contains("must be at most 1024 characters, found 1025"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn overlay_relative_paths_reject_path_traversal() {
        let error =
            normalize_overlay_relative_path("../SKILL.md").expect_err("path traversal should fail");

        assert!(
            error
                .to_string()
                .contains("must be relative and must not contain '.' or '..' segments"),
            "unexpected error: {error}"
        );
    }

    struct TestSkillDir {
        path: PathBuf,
        cleanup_root: PathBuf,
    }

    impl TestSkillDir {
        fn new(name: &str) -> Self {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("time moved backwards")
                .as_nanos();
            let cleanup_root = env::temp_dir().join(format!(
                "skillctl-skill-test-{}-{unique}",
                std::process::id()
            ));
            let path = cleanup_root.join(name);
            fs::create_dir_all(&path).expect("skill dir exists");
            Self { path, cleanup_root }
        }

        fn path(&self) -> &Path {
            &self.path
        }

        fn write(&self, relative: &str, contents: &str) {
            let path = self.path.join(relative);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("parent directory exists");
            }
            fs::write(path, contents).expect("fixture written");
        }
    }

    impl Drop for TestSkillDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.cleanup_root);
        }
    }
}
