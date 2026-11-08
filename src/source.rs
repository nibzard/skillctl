//! Source detection, normalization, staging, and install inspection.

use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    ffi::OsStr,
    fs::{self, File},
    io::{self, IsTerminal, Read, Write},
    path::{Component, Path, PathBuf},
    process::Command,
};

use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use time::{OffsetDateTime, macros::format_description};
use url::Url;

use crate::{
    adapter::{AdapterRegistry, TargetRuntime, TargetScope},
    app::{AppContext, InteractionMode, OutputMode},
    cli::Scope,
    discovery::DiscoveryTransport,
    error::AppError,
    history::HistoryLedger,
    lockfile::{
        LockedBundle, LockedHashes, LockedImport, LockedRevision, LockedSource, LockfilePath,
        WorkspaceLockfile,
    },
    manifest::{
        DEFAULT_MANIFEST_PATH, ImportDefinition, ImportSourceType, ManifestPath, ManifestScope,
        WorkspaceManifest,
    },
    materialize::{self, MaterializationReport},
    overlay::{self, DEFAULT_OVERLAYS_DIR},
    response::AppResponse,
    skill::{DEFAULT_SKILLS_DIR, SKILL_MANIFEST_FILE, SkillDefinition, SkillSafetySummary},
    state::{
        InstallRecord, LocalStateStore, ManagedScope, ManagedSkillRef, PinRecord,
        workspace_key_for_path,
    },
    telemetry,
    trust::SkillTrust,
};

const DETECTION_ROOTS: &[DetectionRoot] = &[
    DetectionRoot::new(
        ".agents/skills",
        &[
            TargetRuntime::Codex,
            TargetRuntime::GithubCopilot,
            TargetRuntime::GeminiCli,
            TargetRuntime::Amp,
            TargetRuntime::Opencode,
        ],
        "neutral workspace skill root",
    ),
    DetectionRoot::new(
        ".claude/skills",
        &[
            TargetRuntime::ClaudeCode,
            TargetRuntime::GithubCopilot,
            TargetRuntime::Amp,
            TargetRuntime::Opencode,
        ],
        "claude-compatible workspace skill root",
    ),
    DetectionRoot::new(
        ".opencode/skills",
        &[TargetRuntime::Opencode],
        "opencode-native workspace skill root",
    ),
    DetectionRoot::new("skills", &[], "repo-root source packaging layout"),
];

const SUPPORTED_ARCHIVE_EXTENSIONS: &[&str] = &[
    ".zip", ".tar", ".tar.gz", ".tgz", ".tar.bz2", ".tbz2", ".tar.xz", ".txz",
];
const USER_IMPORTS_NAMESPACE: &str = "user";
const WORKSPACE_IMPORTS_NAMESPACE: &str = "workspace";
const DIRECT_SKILL_PACKAGING_ROOT: &str = "skills";

/// Supported install source categories.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SourceKind {
    /// Remote Git repository.
    Git,
    /// Local directory path.
    LocalPath,
    /// Local archive file.
    Archive,
    /// Well-known discovery index published over HTTPS or HTTP.
    WellKnown,
}

/// Unnormalized install source definition from the CLI.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstallSource {
    /// Unnormalized source value from the CLI.
    pub raw: String,
}

/// Typed request for `skillctl install`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstallRequest {
    /// Requested install source.
    pub source: InstallSource,
}

impl InstallRequest {
    /// Create an install request from parsed CLI arguments.
    pub fn new(source: String) -> Self {
        Self {
            source: InstallSource { raw: source },
        }
    }
}

/// Stable normalized source identity used by the install pipeline.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct NormalizedInstallSource {
    /// User-provided source before normalization.
    pub raw: String,
    /// Normalized source kind.
    #[serde(rename = "type")]
    pub kind: SourceKind,
    /// Lockfile-ready normalized URL or file URL.
    pub url: String,
    /// Human-facing display value for the source.
    pub display: String,
}

/// Immutable revision data resolved from a staged source.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SourceRevision {
    /// Exact commit hash or content digest.
    pub resolved: String,
    /// Last observed upstream commit, when applicable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream: Option<String>,
}

/// Whether `reference` names the resolved commit itself rather than a movable ref.
///
/// A full or abbreviated git commit sha is hexadecimal and a prefix of the
/// resolved revision. Such a reference is immutable, so a source can never
/// report a newer revision for it.
fn reference_names_commit(reference: &str, resolved_revision: &str) -> bool {
    !reference.is_empty()
        && reference.len() <= resolved_revision.len()
        && resolved_revision.starts_with(reference)
        && reference.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Whether one reference pins an immutable revision no upstream ref can report.
///
/// A commit sha and a recorded effective-version digest both name one exact
/// revision, so following them observes nothing upstream.
pub fn reference_pins_immutable_revision(reference: &str, resolved_revision: &str) -> bool {
    reference_names_commit(reference, resolved_revision)
        || crate::bundle::is_canonical_digest(reference)
}

/// The upstream revision one revision move observed, if any.
///
/// A move that follows a movable reference observes the revision that reference
/// resolved to, which is therefore the last observed upstream state. A move to
/// an immutable reference — the commit or effective version a `pin` or
/// `rollback` names — observes nothing upstream, because that reference names no
/// ref the source could move on from. Recording the moved-to revision there
/// would invent an observation, so none is recorded.
pub(crate) fn observed_upstream_revision(
    manifest_reference: Option<&str>,
    resolved_revision: &str,
) -> Option<String> {
    if manifest_reference
        .is_some_and(|reference| reference_pins_immutable_revision(reference, resolved_revision))
    {
        return None;
    }

    Some(resolved_revision.to_string())
}

/// The upstream revision one locked import can honestly report.
///
/// A reference that names an immutable revision leaves nothing for an upstream
/// check to observe, so a value an older build recorded while moving to that
/// reference is withheld instead of being presented as an observation.
pub(crate) fn reportable_upstream_revision<'a>(
    recorded: Option<&'a String>,
    resolved_revision: &str,
    requested_reference: Option<&str>,
) -> Option<&'a String> {
    if requested_reference
        .is_some_and(|reference| reference_pins_immutable_revision(reference, resolved_revision))
    {
        return None;
    }

    recorded
}

/// Normalized candidate detected during source inspection.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct InstallCandidate {
    /// Stable skill identifier from `SKILL.md`.
    pub name: String,
    /// Human-facing display value.
    pub display_name: String,
    /// Relative source path where the skill was detected.
    pub source_path: String,
    /// Stable selected subpath to record in the lockfile.
    pub selected_subpath: String,
    /// Workspace runtimes that document the detected root.
    pub compatible_targets: Vec<TargetRuntime>,
    /// Human-readable hints about the detected layout.
    pub compatibility_hints: Vec<String>,
    /// Parsed declarative safety metadata.
    #[serde(default, skip_serializing_if = "SkillSafetySummary::is_empty")]
    pub safety: SkillSafetySummary,
    /// Trust decision for installing this candidate.
    pub trust: SkillTrust,
}

/// Structured result of source inspection for `install`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct InstallInspection {
    /// Normalized source identity.
    pub source: NormalizedInstallSource,
    /// Immutable revision captured for the staged source.
    pub revision: SourceRevision,
    /// Install candidates discovered in the staged source.
    pub candidates: Vec<InstallCandidate>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DetectionRoot {
    relative_path: &'static str,
    compatible_targets: &'static [TargetRuntime],
    note: &'static str,
}

impl DetectionRoot {
    const fn new(
        relative_path: &'static str,
        compatible_targets: &'static [TargetRuntime],
        note: &'static str,
    ) -> Self {
        Self {
            relative_path,
            compatible_targets,
            note,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ResolvedInstallSource {
    normalized: NormalizedInstallSource,
    local_path: Option<PathBuf>,
}

#[derive(Debug)]
struct PreparedSource {
    source: NormalizedInstallSource,
    revision: SourceRevision,
    root: PathBuf,
    _staging_dir: Option<TempDir>,
}

#[derive(Debug)]
struct PreparedInstall {
    source: NormalizedInstallSource,
    revision: SourceRevision,
    candidates: Vec<InstallCandidate>,
    root: PathBuf,
    /// Discovery index entry a well-known source selected and staged.
    discovery: Option<crate::lockfile::LockedDiscoveryEntry>,
    /// Origin-trust warning the staged discovery origin produced, if any.
    origin_warning: Option<String>,
    /// Skill name the source itself selected, which the install must honor.
    ///
    /// A well-known source stages exactly one index entry, so the entry name is
    /// the selector the install applies to the detected candidates.
    selected_skill_name: Option<String>,
    _staging_dir: Option<TempDir>,
}

impl PreparedInstall {
    fn inspection(&self) -> InstallInspection {
        InstallInspection {
            source: self.source.clone(),
            revision: self.revision.clone(),
            candidates: self.candidates.clone(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
/// Structured record of one completed install.
pub struct InstalledSkill {
    /// Stable manifest import identifier created for this install.
    pub id: String,
    /// Installed skill name.
    pub name: String,
    /// Selected management scope.
    pub scope: String,
    /// Relative source path where the skill was detected.
    pub source_path: String,
    /// Stable selected source subpath stored in the lockfile.
    pub selected_subpath: String,
    /// Filesystem root storing the staged immutable source copy.
    pub stored_source_root: String,
    /// Exact installed revision or digest.
    pub resolved_revision: String,
    /// Last observed upstream revision, when available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream_revision: Option<String>,
    /// Hash of the selected skill contents.
    pub content_hash: String,
    /// Hash of the applied overlay set.
    pub overlay_hash: String,
    /// Effective version hash derived from the pinned inputs.
    pub effective_version_hash: String,
    /// Parsed declarative safety metadata.
    #[serde(default, skip_serializing_if = "SkillSafetySummary::is_empty")]
    pub safety: SkillSafetySummary,
    /// Trust decision for the installed effective skill.
    pub trust: SkillTrust,
}

#[derive(Clone, Debug)]
struct InstallOperation {
    installed: InstalledSkill,
    import: ImportDefinition,
    locked_import: LockedImport,
}

struct InstallBuildContext<'a> {
    working_directory: &'a Path,
    manifest: &'a WorkspaceManifest,
}

/// Inspect, normalize, and stage an install source before later lifecycle steps.
pub fn inspect_install_source(
    working_directory: &Path,
    request: &InstallRequest,
) -> Result<InstallInspection, AppError> {
    Ok(prepare_install_source_at(working_directory, request)?.inspection())
}

/// Handle `skillctl install`.
pub fn handle_install(
    context: &AppContext,
    request: InstallRequest,
) -> Result<AppResponse, AppError> {
    let prepared = prepare_install_source(context, &request)?;
    let inspection = prepared.inspection();
    let selected = select_install_candidates(
        context,
        &inspection,
        prepared.selected_skill_name.as_deref(),
    )?;
    let scope = select_install_scope(context)?;
    let expected = crate::apply::capture_expected_imports(context, Some(managed_scope(scope)))?;
    let bootstrap_manifest = {
        // `install` bootstraps the manifest itself, so the plan phase reads a
        // default layout when the workspace has no manifest yet.
        let manifest = match WorkspaceManifest::load_from_workspace(&context.working_directory) {
            Ok(manifest) => manifest,
            Err(AppError::FilesystemOperation {
                action: "read manifest",
                path,
                source,
            }) if source.kind() == io::ErrorKind::NotFound => WorkspaceManifest::default_at(path),
            Err(error) => return Err(error),
        };
        let scoped_context = install_context_for_scope(context, scope);
        crate::apply::MutationPlanBuilder::new(
            crate::apply::MutationOperation::Install,
            &context.working_directory,
        )
        .expect_imports(expected)
        .target(
            context.working_directory.join(DEFAULT_MANIFEST_PATH),
            crate::apply::TargetAction::Update,
        )
        .target(
            context
                .working_directory
                .join(crate::lockfile::DEFAULT_LOCKFILE_PATH),
            crate::apply::TargetAction::Update,
        )
        .targets(
            materialize::sync_mutation_root_paths(&scoped_context, &manifest, scope)?,
            crate::apply::TargetAction::Create,
        )
        .seal()
    };

    crate::apply::run_guarded(&bootstrap_manifest, context, |transaction| {
        transaction.track_state_database()?;

        ensure_workspace_bootstrap(&context.working_directory)?;

        let mut manifest = WorkspaceManifest::load_from_workspace(&context.working_directory)?;
        let mut lockfile = WorkspaceLockfile::load_from_workspace(&context.working_directory)?;
        let scoped_context = install_context_for_scope(context, scope);
        for root in materialize::sync_mutation_root_paths(&scoped_context, &manifest, scope)? {
            transaction.track_path(root)?;
        }

        validate_install_selection(
            &context.working_directory,
            &manifest,
            &inspection.source,
            scope,
            &selected,
        )?;

        let install_timestamp = current_timestamp();
        let mut operations = Vec::with_capacity(selected.len());

        for candidate in &selected {
            let build_context = InstallBuildContext {
                working_directory: &context.working_directory,
                manifest: &manifest,
            };
            let operation = build_install_operation(
                &prepared,
                &inspection.source,
                &inspection.revision,
                scope,
                candidate,
                &build_context,
            )?;
            transaction.track_path(PathBuf::from(&operation.installed.stored_source_root))?;
            upsert_manifest_import(&mut manifest, operation.import.clone());
            lockfile.imports.insert(
                operation.installed.id.clone(),
                operation.locked_import.clone(),
            );
            operations.push(operation);
        }

        write_manifest(&manifest)?;
        lockfile.write_to_path()?;

        for operation in &operations {
            store_selected_subtree(
                &prepared.root,
                &operation.installed.selected_subpath,
                Path::new(&operation.installed.stored_source_root),
            )?;
            // Retain an immutable, content-addressed artifact of the approved
            // skill subtree so a fresh cache can restore this exact content
            // without contacting the source again.
            let artifact_root = crate::artifact::ArtifactStore::open_default()?;
            artifact_root.store(
                &Path::new(&operation.installed.stored_source_root)
                    .join(operation.installed.selected_subpath.as_str()),
            )?;
        }

        let sync_report = materialize::sync_workspace(&scoped_context)?;
        record_install_state(
            context,
            scope,
            &operations,
            &sync_report,
            &install_timestamp,
        )?;
        transaction.checkpoint("after-state")?;

        let installed: Vec<_> = operations
            .iter()
            .map(|operation| operation.installed.clone())
            .collect();
        let telemetry = telemetry::prepare_install_report(context, &inspection.source, &installed)?;
        let mut summary = install_summary(&installed, &sync_report);
        if let Some(notice) = telemetry.notice_message() {
            summary.push('\n');
            summary.push_str(notice);
        }

        let mut response = AppResponse::success("install")
            .with_summary(summary)
            .with_data(json!({
                "source": inspection.source,
                "revision": inspection.revision,
                "candidates": inspection.candidates,
                "selected": selected,
                "installed": installed,
                "projection": sync_report,
                "telemetry": telemetry,
            }));
        for warning in &sync_report.warnings {
            response = response.with_warning(warning.clone());
        }
        if let Some(warning) = prepared.origin_warning.as_deref() {
            response = response.with_warning(warning.to_string());
        }
        let mut warnings = BTreeSet::new();
        for skill in &installed {
            for warning in skill.safety.imported_warnings(&skill.name) {
                warnings.insert(warning);
            }
            for warning in &skill.trust.warnings {
                warnings.insert(warning.clone());
            }
        }
        for warning in warnings {
            response = response.with_warning(warning);
        }

        Ok(response)
    })
}

/// Resolve and stage an install source for `skillctl install`.
///
/// A well-known source selects one index entry first, because only the selected
/// entry is downloaded.
fn prepare_install_source(
    context: &AppContext,
    request: &InstallRequest,
) -> Result<PreparedInstall, AppError> {
    prepare_install_source_with(
        context,
        request,
        &crate::discovery::HttpTransport::default(),
    )
}

/// [`prepare_install_source`] against an explicit transport, shared with the
/// tests so the whole well-known path runs without a network.
fn prepare_install_source_with(
    context: &AppContext,
    request: &InstallRequest,
    transport: &dyn DiscoveryTransport,
) -> Result<PreparedInstall, AppError> {
    let resolved = resolve_install_source(&request.source, &context.working_directory)?;
    let mut discovery = None;
    let mut origin_warning = None;
    let mut selected_skill_name = None;
    let prepared = match resolved.normalized.kind {
        SourceKind::WellKnown => {
            // The origin-trust policy gates every acquisition: an origin it
            // refuses is never read, and one it did not verify warns while the
            // declared digests still verify the fetched bytes.
            let origin_trust = crate::trust::ensure_discovery_origin_permits_acquisition(
                &resolved.normalized.url,
            )?;
            origin_warning =
                crate::trust::discovery_origin_warning(origin_trust, &resolved.normalized.url);
            let entry = select_well_known_entry(context, &resolved, transport)?;
            let staged = crate::discovery::fetch_and_stage_artifact(&entry, transport)?;
            let entry_name = entry.name.clone();
            selected_skill_name = Some(entry_name.clone());
            discovery = Some(crate::lockfile::LockedDiscoveryEntry {
                skill: entry_name,
                artifact_type: entry.artifact_type,
                url: entry.url.to_string(),
                digest: entry.digest.as_str().to_string(),
            });
            PreparedSource {
                source: resolved.normalized,
                revision: SourceRevision {
                    resolved: staged.digest.as_str().to_string(),
                    upstream: None,
                },
                root: staged.root,
                _staging_dir: Some(staged._staging_dir),
            }
        }
        SourceKind::Git | SourceKind::LocalPath | SourceKind::Archive => prepare_source(&resolved)?,
    };
    let candidates = detect_install_candidates(&prepared.root, &prepared.source.raw)?;

    Ok(PreparedInstall {
        source: prepared.source,
        revision: prepared.revision,
        candidates,
        root: prepared.root,
        discovery,
        origin_warning,
        selected_skill_name,
        _staging_dir: prepared._staging_dir,
    })
}

/// Resolve and stage an install source from a working directory alone.
///
/// Sources that need a selector to stage anything, such as a well-known index,
/// are refused here: this path stages a whole source, so it cannot choose one
/// index entry.
fn prepare_install_source_at(
    working_directory: &Path,
    request: &InstallRequest,
) -> Result<PreparedInstall, AppError> {
    let resolved = resolve_install_source(&request.source, working_directory)?;
    if resolved.normalized.kind == SourceKind::WellKnown {
        return Err(source_validation(
            &resolved.normalized.raw,
            "a well-known discovery index stages one declared entry at a time, and this path \
             cannot select one; run 'skillctl install <url> --name <skill>' instead",
        ));
    }

    let prepared = prepare_source(&resolved)?;
    let candidates = detect_install_candidates(&prepared.root, &prepared.source.raw)?;

    Ok(PreparedInstall {
        source: prepared.source,
        revision: prepared.revision,
        candidates,
        root: prepared.root,
        discovery: None,
        origin_warning: None,
        selected_skill_name: None,
        _staging_dir: prepared._staging_dir,
    })
}

fn select_install_candidates(
    context: &AppContext,
    inspection: &InstallInspection,
    selected_skill_name: Option<&str>,
) -> Result<Vec<InstallCandidate>, AppError> {
    if let Some(skill_name) = selected_skill_name.or(context.selector.skill_name.as_deref()) {
        return select_by_exact_name(inspection, skill_name);
    }

    if install_requires_non_interactive_selection(context) {
        return Err(AppError::InputRequired { command: "install" });
    }

    prompt_for_install_candidates(inspection)
}

/// Select one entry from the discovery index a well-known source points at.
///
/// The index is fetched and validated before anything is staged, because the
/// selected entry decides which artifact is downloaded. An exact skill selector
/// picks the entry directly. Without one, a non-interactive install refuses to
/// guess and an interactive install prompts over the declared entries.
fn select_well_known_entry(
    context: &AppContext,
    resolved: &ResolvedInstallSource,
    transport: &dyn DiscoveryTransport,
) -> Result<crate::discovery::DiscoveryEntry, AppError> {
    let raw = resolved.normalized.raw.as_str();
    let target = crate::discovery::resolve_target(raw)?;
    let body = transport.fetch(&target.index_url)?;
    let document = String::from_utf8(body)
        .map_err(|_| source_validation(raw, "the discovery index document is not valid UTF-8"))?;
    let index = crate::discovery::parse_index(&document, &target.index_url)?;

    if let Some(skill_name) = context.selector.skill_name.as_deref() {
        let selected = index
            .entries
            .iter()
            .find(|entry| entry.name == skill_name)
            .cloned();
        return selected.ok_or_else(|| {
            source_validation(
                raw,
                format!(
                    "the discovery index declares no skill named '{skill_name}'; it declares {}",
                    declared_entry_names(&index),
                ),
            )
        });
    }

    if index.entries.is_empty() {
        return Err(source_validation(
            raw,
            "the discovery index declares no skills, so nothing can be installed",
        ));
    }
    if install_requires_non_interactive_selection(context) {
        return Err(AppError::InputRequired { command: "install" });
    }

    prompt_for_well_known_entry(raw, &target.index_url, index)
}

/// Prompt for one entry of a discovery index.
fn prompt_for_well_known_entry(
    raw: &str,
    index_url: &Url,
    index: crate::discovery::DiscoveryIndex,
) -> Result<crate::discovery::DiscoveryEntry, AppError> {
    println!(
        "Discovered {} skill{} at {index_url}",
        index.entries.len(),
        plural_suffix(index.entries.len()),
    );
    for (position, entry) in index.entries.iter().enumerate() {
        println!(
            "  {}. {} ({}) {}",
            position + 1,
            entry.name,
            entry.artifact_type.as_str(),
            entry.description
        );
    }

    let answer = prompt_line("Select one skill by number or exact name: ")?;
    let trimmed = answer.trim();
    if let Ok(position) = trimmed.parse::<usize>() {
        if !(1..=index.entries.len()).contains(&position) {
            return Err(source_validation(
                raw,
                format!(
                    "selection '{position}' is out of range 1..{}",
                    index.entries.len()
                ),
            ));
        }
        return Ok(index
            .entries
            .into_iter()
            .nth(position - 1)
            .expect("the range was checked"));
    }

    let names = declared_entry_names(&index);
    index
        .entries
        .into_iter()
        .find(|entry| entry.name == trimmed)
        .ok_or_else(|| {
            source_validation(
                raw,
                format!(
                    "selection '{trimmed}' matched no declared skill; the index declares {names}"
                ),
            )
        })
}

/// Comma-separated declared entry names of a discovery index.
fn declared_entry_names(index: &crate::discovery::DiscoveryIndex) -> String {
    index
        .entries
        .iter()
        .map(|entry| entry.name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

fn select_install_scope(context: &AppContext) -> Result<TargetScope, AppError> {
    if let Some(scope) = context.selector.scope {
        return Ok(target_scope(scope));
    }

    if should_prompt_for_scope(context) {
        prompt_for_install_scope()
    } else {
        Ok(TargetScope::Workspace)
    }
}

fn select_by_exact_name(
    inspection: &InstallInspection,
    skill_name: &str,
) -> Result<Vec<InstallCandidate>, AppError> {
    let matches: Vec<_> = inspection
        .candidates
        .iter()
        .filter(|candidate| candidate.name == skill_name)
        .cloned()
        .collect();

    match matches.len() {
        0 => Err(source_validation(
            inspection.source.raw.clone(),
            format!("exact skill name '{skill_name}' was not found in the detected candidates"),
        )),
        1 => Ok(matches),
        _ => Err(source_validation(
            inspection.source.raw.clone(),
            format!(
                "exact skill name '{skill_name}' is ambiguous; matches {}",
                matches
                    .iter()
                    .map(|candidate| candidate.source_path.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        )),
    }
}

fn install_requires_non_interactive_selection(context: &AppContext) -> bool {
    if matches!(context.output_mode, OutputMode::Json) {
        return true;
    }

    match context.interaction_mode {
        InteractionMode::Interactive => false,
        InteractionMode::NonInteractive => true,
        InteractionMode::Auto => !(io::stdin().is_terminal() && io::stdout().is_terminal()),
    }
}

fn should_prompt_for_scope(context: &AppContext) -> bool {
    if matches!(context.output_mode, OutputMode::Json) {
        return false;
    }

    match context.interaction_mode {
        InteractionMode::Interactive => true,
        InteractionMode::NonInteractive => false,
        InteractionMode::Auto => io::stdin().is_terminal() && io::stdout().is_terminal(),
    }
}

fn prompt_for_install_candidates(
    inspection: &InstallInspection,
) -> Result<Vec<InstallCandidate>, AppError> {
    let candidate_count = inspection.candidates.len();
    println!(
        "Detected {candidate_count} install candidate{} from {} at {}",
        plural_suffix(candidate_count),
        inspection.source.display,
        inspection.revision.resolved
    );
    println!("Pinned revision: {}", inspection.revision.resolved);
    for (index, candidate) in inspection.candidates.iter().enumerate() {
        let compatible_targets = if candidate.compatible_targets.is_empty() {
            "generic".to_string()
        } else {
            candidate
                .compatible_targets
                .iter()
                .map(|target| target.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        };
        println!(
            "  {}. {} ({}) [{}]",
            index + 1,
            candidate.display_name,
            candidate.source_path,
            compatible_targets
        );
    }

    let answer =
        prompt_line("Select skills by number, exact name, comma-separated list, or '*' for all: ")?;
    parse_candidate_selection(&answer, inspection)
}

fn parse_candidate_selection(
    answer: &str,
    inspection: &InstallInspection,
) -> Result<Vec<InstallCandidate>, AppError> {
    let trimmed = answer.trim();
    if trimmed.is_empty() {
        return Err(source_validation(
            inspection.source.raw.clone(),
            "interactive install selection must not be empty",
        ));
    }

    if matches!(trimmed, "*" | "all" | "ALL") {
        return Ok(inspection.candidates.clone());
    }

    let mut selected_indexes = BTreeSet::new();
    for token in trimmed.split(',') {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }

        if let Ok(position) = token.parse::<usize>() {
            if !(1..=inspection.candidates.len()).contains(&position) {
                return Err(source_validation(
                    inspection.source.raw.clone(),
                    format!(
                        "interactive install selection index '{position}' is out of range 1..{}",
                        inspection.candidates.len()
                    ),
                ));
            }
            selected_indexes.insert(position - 1);
            continue;
        }

        let matches: Vec<_> = inspection
            .candidates
            .iter()
            .enumerate()
            .filter(|(_, candidate)| candidate.name == token)
            .map(|(index, _)| index)
            .collect();
        match matches.len() {
            0 => {
                return Err(source_validation(
                    inspection.source.raw.clone(),
                    format!("interactive install selection '{token}' did not match any candidate"),
                ));
            }
            1 => {
                selected_indexes.insert(matches[0]);
            }
            _ => {
                return Err(source_validation(
                    inspection.source.raw.clone(),
                    format!("interactive install selection '{token}' is ambiguous"),
                ));
            }
        }
    }

    if selected_indexes.is_empty() {
        return Err(source_validation(
            inspection.source.raw.clone(),
            "interactive install selection must choose at least one candidate",
        ));
    }

    Ok(selected_indexes
        .into_iter()
        .map(|index| inspection.candidates[index].clone())
        .collect())
}

fn prompt_for_install_scope() -> Result<TargetScope, AppError> {
    println!("Select scope:");
    println!("  1. workspace");
    println!("  2. user");

    let answer = prompt_line("Select scope [1]: ")?;
    match answer.trim() {
        "" | "1" | "workspace" | "w" => Ok(TargetScope::Workspace),
        "2" | "user" | "u" => Ok(TargetScope::User),
        other => Err(source_validation(
            "install",
            format!("interactive scope selection '{other}' is invalid"),
        )),
    }
}

fn prompt_line(prompt: &str) -> Result<String, AppError> {
    print!("{prompt}");
    let _ = io::stdout().flush();

    let mut buffer = String::new();
    let bytes_read = io::stdin().read_line(&mut buffer).map_err(|source| {
        source_validation(
            "install",
            format!("failed to read interactive input: {source}"),
        )
    })?;
    if bytes_read == 0 {
        return Err(AppError::InputRequired { command: "install" });
    }

    Ok(buffer)
}

fn ensure_workspace_bootstrap(working_directory: &Path) -> Result<(), AppError> {
    ensure_directory_path(&working_directory.join(DEFAULT_SKILLS_DIR))?;
    ensure_directory_path(&working_directory.join(DEFAULT_OVERLAYS_DIR))?;
    ensure_manifest_file(&working_directory.join(DEFAULT_MANIFEST_PATH))?;
    ensure_lockfile_file(&working_directory.join(crate::lockfile::DEFAULT_LOCKFILE_PATH))?;
    Ok(())
}

fn ensure_directory_path(path: &Path) -> Result<(), AppError> {
    match fs::metadata(path) {
        Ok(metadata) if metadata.is_dir() => Ok(()),
        Ok(_) => Err(AppError::PathConflict {
            path: path.to_path_buf(),
            expected: "directory",
        }),
        Err(source) if source.kind() == io::ErrorKind::NotFound => fs::create_dir_all(path)
            .map_err(|source| AppError::FilesystemOperation {
                action: "create directory",
                path: path.to_path_buf(),
                source,
            }),
        Err(source) => Err(AppError::FilesystemOperation {
            action: "inspect directory",
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn ensure_manifest_file(path: &Path) -> Result<(), AppError> {
    match fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => Ok(()),
        Ok(_) => Err(AppError::PathConflict {
            path: path.to_path_buf(),
            expected: "file",
        }),
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            write_manifest(&WorkspaceManifest::default_at(path.to_path_buf()))
        }
        Err(source) => Err(AppError::FilesystemOperation {
            action: "inspect manifest",
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn ensure_lockfile_file(path: &Path) -> Result<(), AppError> {
    match fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => Ok(()),
        Ok(_) => Err(AppError::PathConflict {
            path: path.to_path_buf(),
            expected: "file",
        }),
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            WorkspaceLockfile::default_at(path.to_path_buf()).write_to_path()
        }
        Err(source) => Err(AppError::FilesystemOperation {
            action: "inspect lockfile",
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn validate_install_selection(
    working_directory: &Path,
    manifest: &WorkspaceManifest,
    source: &NormalizedInstallSource,
    scope: TargetScope,
    selected: &[InstallCandidate],
) -> Result<(), AppError> {
    let mut seen_names = BTreeSet::new();
    for candidate in selected {
        if !seen_names.insert(candidate.name.clone()) {
            return Err(source_validation(
                source.raw.clone(),
                format!("skill '{}' was selected more than once", candidate.name),
            ));
        }

        if let Some(existing) = manifest
            .imports
            .iter()
            .find(|import| import.id == candidate.name)
        {
            let same_install = existing.kind == import_source_type(source.kind)
                && existing.url == source.url
                && existing.path.as_str() == candidate.selected_subpath
                && existing.scope == manifest_scope(scope);
            if !same_install {
                return Err(source_validation(
                    source.raw.clone(),
                    format!(
                        "skill '{}' is already managed by import '{}'; run 'skillctl remove {}' \
                         first, or install the exact source that is already pinned to re-pin it",
                        candidate.name, existing.id, candidate.name
                    ),
                ));
            }
        }

        if scope == TargetScope::Workspace {
            let existing_path = working_directory
                .join(manifest.layout.skills_dir.as_str())
                .join(&candidate.name);
            match fs::metadata(&existing_path) {
                Ok(metadata) if !metadata.is_dir() => {
                    return Err(AppError::PathConflict {
                        path: existing_path,
                        expected: "directory",
                    });
                }
                Ok(_) if !is_skillctl_generated_directory(&existing_path)? => {
                    return Err(source_validation(
                        source.raw.clone(),
                        format!(
                            "workspace skill '{}' already exists at '{}'",
                            candidate.name,
                            existing_path.display()
                        ),
                    ));
                }
                Ok(_) => {}
                Err(source) if source.kind() == io::ErrorKind::NotFound => {}
                Err(source) => {
                    return Err(AppError::FilesystemOperation {
                        action: "inspect installed workspace skill",
                        path: existing_path,
                        source,
                    });
                }
            }
        }
    }

    Ok(())
}

fn build_install_operation(
    prepared: &PreparedInstall,
    source: &NormalizedInstallSource,
    revision: &SourceRevision,
    scope: TargetScope,
    candidate: &InstallCandidate,
    context: &InstallBuildContext<'_>,
) -> Result<InstallOperation, AppError> {
    let import_id = candidate.name.clone();
    let source_subpath = Path::new(&candidate.selected_subpath);
    let skill_root = prepared.root.join(source_subpath);
    let requested_reference = install_requested_reference(source.kind, revision);
    let content_hash = hash_directory_contents(&skill_root)?;
    let bundle_manifest = crate::bundle::BundleManifest::capture(&skill_root)?;
    let overlay_hash = context
        .manifest
        .overrides
        .get(&import_id)
        .map(|overlay_path| {
            overlay::hash_overlay_root(&context.working_directory.join(overlay_path.as_str()))
        })
        .transpose()?
        .unwrap_or_else(|| overlay::NO_OVERLAY_HASH.to_string());
    let effective_version_hash =
        compute_effective_version_hash(&revision.resolved, &content_hash, &overlay_hash);
    let stored_source_root =
        stored_import_root(managed_scope(scope), context.working_directory, &import_id)?;

    Ok(InstallOperation {
        installed: InstalledSkill {
            id: import_id.clone(),
            name: candidate.name.clone(),
            scope: scope_label(scope).to_string(),
            source_path: candidate.source_path.clone(),
            selected_subpath: candidate.selected_subpath.clone(),
            stored_source_root: stored_source_root.display().to_string(),
            resolved_revision: revision.resolved.clone(),
            upstream_revision: revision.upstream.clone(),
            content_hash: content_hash.clone(),
            overlay_hash: overlay_hash.clone(),
            effective_version_hash: effective_version_hash.clone(),
            safety: candidate.safety.clone(),
            trust: candidate.trust.clone(),
        },
        import: ImportDefinition {
            id: import_id,
            kind: import_source_type(source.kind),
            url: source.url.clone(),
            ref_spec: requested_reference,
            path: ManifestPath::new(candidate.selected_subpath.clone()),
            scope: manifest_scope(scope),
            enabled: true,
        },
        locked_import: LockedImport {
            source: LockedSource {
                kind: source.kind,
                url: source.url.clone(),
                subpath: LockfilePath::new(candidate.selected_subpath.clone()),
            },
            discovery: prepared.discovery.clone(),
            revision: LockedRevision {
                resolved: revision.resolved.clone(),
                upstream: revision.upstream.clone(),
            },
            bundle: Some(LockedBundle {
                format_version: bundle_manifest.format_version,
                digest: bundle_manifest.digest(),
                files: bundle_manifest.file_count() as u64,
                bytes: bundle_manifest.total_bytes(),
            }),
            hashes: LockedHashes {
                content: content_hash,
                overlay: overlay_hash,
                effective_version: effective_version_hash,
            },
        },
    })
}

fn upsert_manifest_import(manifest: &mut WorkspaceManifest, import: ImportDefinition) {
    if let Some(existing) = manifest
        .imports
        .iter_mut()
        .find(|existing| existing.id == import.id)
    {
        *existing = import;
    } else {
        manifest.imports.push(import);
        manifest.imports.sort_by(|left, right| {
            left.id
                .cmp(&right.id)
                .then_with(|| left.path.as_str().cmp(right.path.as_str()))
        });
    }
}

fn write_manifest(manifest: &WorkspaceManifest) -> Result<(), AppError> {
    manifest.write_to_path()
}

/// Store only the selected skill subtree of a staged source.
///
/// The stored layout is `<stored root>/<subpath>`, which is what
/// `restore --frozen` writes and what every reader of the import cache joins
/// back together. Copying the subtree rather than the whole staged source keeps
/// unrelated entries out of the cache, including the `.git` directory a Git
/// checkout carries and any symlink the selected skill does not use.
pub(crate) fn store_selected_subtree(
    staged_root: &Path,
    selected_subpath: &str,
    stored_root: &Path,
) -> Result<(), AppError> {
    copy_source_tree(
        &staged_root.join(selected_subpath),
        &stored_root.join(selected_subpath),
    )
}

pub(crate) fn copy_source_tree(
    source_root: &Path,
    destination_root: &Path,
) -> Result<(), AppError> {
    match fs::metadata(destination_root) {
        Ok(metadata) if !metadata.is_dir() => {
            return Err(AppError::PathConflict {
                path: destination_root.to_path_buf(),
                expected: "directory",
            });
        }
        Ok(_) => {
            fs::remove_dir_all(destination_root).map_err(|source| {
                AppError::FilesystemOperation {
                    action: "replace stored import root",
                    path: destination_root.to_path_buf(),
                    source,
                }
            })?;
        }
        Err(source) if source.kind() == io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(AppError::FilesystemOperation {
                action: "inspect stored import root",
                path: destination_root.to_path_buf(),
                source,
            });
        }
    }

    fs::create_dir_all(destination_root).map_err(|source| AppError::FilesystemOperation {
        action: "create stored import root",
        path: destination_root.to_path_buf(),
        source,
    })?;
    copy_directory_contents(source_root, destination_root)
}

fn copy_directory_contents(source_root: &Path, destination_root: &Path) -> Result<(), AppError> {
    let mut entries = fs::read_dir(source_root)
        .map_err(|source| AppError::FilesystemOperation {
            action: "read install source directory",
            path: source_root.to_path_buf(),
            source,
        })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| AppError::FilesystemOperation {
            action: "read install source directory entry",
            path: source_root.to_path_buf(),
            source,
        })?;
    entries.sort_by_key(std::fs::DirEntry::file_name);

    for entry in entries {
        let source_path = entry.path();
        let destination_path = destination_root.join(entry.file_name());
        let metadata =
            fs::symlink_metadata(&source_path).map_err(|source| AppError::FilesystemOperation {
                action: "inspect install source entry",
                path: source_path.clone(),
                source,
            })?;

        if metadata.is_dir() {
            fs::create_dir_all(&destination_path).map_err(|source| {
                AppError::FilesystemOperation {
                    action: "create stored import directory",
                    path: destination_path.clone(),
                    source,
                }
            })?;
            copy_directory_contents(&source_path, &destination_path)?;
            continue;
        }

        if metadata.file_type().is_symlink() {
            return Err(source_validation(
                source_path.display().to_string(),
                "symlinked install source entries are not supported for stored imports",
            ));
        }

        fs::copy(&source_path, &destination_path).map_err(|source| {
            AppError::FilesystemOperation {
                action: "copy stored import file",
                path: destination_path,
                source,
            }
        })?;
    }

    Ok(())
}

fn install_context_for_scope(context: &AppContext, scope: TargetScope) -> AppContext {
    let mut install_context = context.clone();
    install_context.selector.scope = Some(match scope {
        TargetScope::Workspace => Scope::Workspace,
        TargetScope::User => Scope::User,
    });
    install_context
}

fn record_install_state(
    context: &AppContext,
    scope: TargetScope,
    operations: &[InstallOperation],
    sync_report: &MaterializationReport,
    install_timestamp: &str,
) -> Result<(), AppError> {
    let mut store = LocalStateStore::open_default_for(&context.working_directory)?;
    let managed_scope = managed_scope(scope);
    let mut installed_at_by_skill = BTreeMap::new();

    for operation in operations {
        let skill = ManagedSkillRef::new(managed_scope, operation.installed.name.clone());
        let installed_at = store.install_record(&skill)?.map_or_else(
            || install_timestamp.to_string(),
            |record| record.installed_at,
        );
        installed_at_by_skill.insert(operation.installed.name.clone(), installed_at);

        store.upsert_pin_record(&PinRecord {
            skill,
            requested_reference: operation.import.ref_spec.clone(),
            resolved_revision: operation.installed.resolved_revision.clone(),
            effective_version_hash: Some(operation.installed.effective_version_hash.clone()),
            pinned_at: install_timestamp.to_string(),
        })?;
    }

    {
        let mut ledger = HistoryLedger::new(&mut store);

        for operation in operations {
            let install_record = InstallRecord {
                skill: ManagedSkillRef::new(managed_scope, operation.installed.name.clone()),
                source_kind: operation.locked_import.source.kind,
                source_url: operation.locked_import.source.url.clone(),
                source_subpath: operation.locked_import.source.subpath.as_str().to_string(),
                resolved_revision: operation.locked_import.revision.resolved.clone(),
                upstream_revision: operation.locked_import.revision.upstream.clone(),
                content_hash: operation.locked_import.hashes.content.clone(),
                overlay_hash: operation.locked_import.hashes.overlay.clone(),
                effective_version_hash: operation.locked_import.hashes.effective_version.clone(),
                installed_at: installed_at_by_skill
                    .get(&operation.installed.name)
                    .cloned()
                    .unwrap_or_else(|| install_timestamp.to_string()),
                updated_at: install_timestamp.to_string(),
                detached: false,
                forked: false,
            };
            ledger.record_install_with_trust(&install_record, &operation.installed.trust)?;
        }
    }

    materialize::refresh_projection_state_for_scope(
        &mut store,
        context,
        managed_scope,
        sync_report,
        install_timestamp,
        |_| Ok(()),
    )?;

    Ok(())
}

fn install_requested_reference(kind: SourceKind, revision: &SourceRevision) -> String {
    match kind {
        SourceKind::Git => "HEAD".to_string(),
        SourceKind::LocalPath | SourceKind::Archive | SourceKind::WellKnown => {
            revision.resolved.clone()
        }
    }
}

fn install_summary(installed: &[InstalledSkill], sync_report: &MaterializationReport) -> String {
    let count = installed.len();
    let names = installed
        .iter()
        .map(|skill| skill.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let mut summary = format!(
        "Installed {count} skill{} ({names}) into {} scope.",
        plural_suffix(count),
        installed
            .first()
            .map_or("workspace", |skill| skill.scope.as_str())
    );

    if sync_report.materialized_skills == 0 {
        summary.push_str(" No generated projections were required.");
    } else {
        summary.push_str(&format!(
            " Materialized {} generated projection{}.",
            sync_report.materialized_skills,
            plural_suffix(sync_report.materialized_skills),
        ));
    }
    if sync_report.pruned_skills > 0 {
        summary.push_str(&format!(
            " Pruned {} stale projection{}.",
            sync_report.pruned_skills,
            plural_suffix(sync_report.pruned_skills),
        ));
    }

    summary
}

pub(crate) fn imports_store_root() -> Result<PathBuf, AppError> {
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .map(|path| path.join(".skillctl/store/imports"))
        .ok_or(AppError::HomeDirectoryUnavailable)
}

pub(crate) fn imports_scope_root(
    scope: ManagedScope,
    working_directory: &Path,
) -> Result<PathBuf, AppError> {
    let root = imports_store_root()?;
    match scope {
        ManagedScope::User => Ok(root.join(USER_IMPORTS_NAMESPACE)),
        ManagedScope::Workspace => Ok(root
            .join(WORKSPACE_IMPORTS_NAMESPACE)
            .join(workspace_key_for_path(working_directory)?)),
    }
}

pub(crate) fn stored_import_root(
    scope: ManagedScope,
    working_directory: &Path,
    import_id: &str,
) -> Result<PathBuf, AppError> {
    Ok(imports_scope_root(scope, working_directory)?.join(import_id))
}

pub(crate) fn current_timestamp() -> String {
    let format = format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]Z");

    if let Some(value) = env::var_os("SOURCE_DATE_EPOCH")
        && let Ok(timestamp) = value.to_string_lossy().parse::<i64>()
        && let Ok(timestamp) = OffsetDateTime::from_unix_timestamp(timestamp)
        && let Ok(formatted) = timestamp.format(format)
    {
        return formatted;
    }

    OffsetDateTime::now_utc()
        .format(format)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

pub(crate) fn compute_effective_version_hash(
    resolved_revision: &str,
    content_hash: &str,
    overlay_hash: &str,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(resolved_revision.as_bytes());
    hasher.update(b"\0");
    hasher.update(content_hash.as_bytes());
    hasher.update(b"\0");
    hasher.update(overlay_hash.as_bytes());
    format!("sha256:{:x}", hasher.finalize())
}

fn import_source_type(kind: SourceKind) -> ImportSourceType {
    match kind {
        SourceKind::Git => ImportSourceType::Git,
        SourceKind::LocalPath => ImportSourceType::LocalPath,
        SourceKind::Archive => ImportSourceType::Archive,
        SourceKind::WellKnown => ImportSourceType::WellKnown,
    }
}

fn target_scope(scope: Scope) -> TargetScope {
    match scope {
        Scope::Workspace => TargetScope::Workspace,
        Scope::User => TargetScope::User,
    }
}

fn manifest_scope(scope: TargetScope) -> ManifestScope {
    match scope {
        TargetScope::Workspace => ManifestScope::Workspace,
        TargetScope::User => ManifestScope::User,
    }
}

fn managed_scope(scope: TargetScope) -> ManagedScope {
    match scope {
        TargetScope::Workspace => ManagedScope::Workspace,
        TargetScope::User => ManagedScope::User,
    }
}

fn scope_label(scope: TargetScope) -> &'static str {
    match scope {
        TargetScope::Workspace => "workspace",
        TargetScope::User => "user",
    }
}

fn is_skillctl_generated_directory(path: &Path) -> Result<bool, AppError> {
    let metadata_path = path.join(materialize::PROJECTION_METADATA_FILE);
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

    Ok(parsed.get("tool").and_then(|value| value.as_str()) == Some("skillctl"))
}

fn plural_suffix(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

fn resolve_install_source(
    source: &InstallSource,
    working_directory: &Path,
) -> Result<ResolvedInstallSource, AppError> {
    let raw = source.raw.trim();
    if raw.is_empty() {
        return Err(source_validation(&source.raw, "source must not be empty"));
    }
    if raw != source.raw {
        return Err(source_validation(
            &source.raw,
            "source must not contain leading or trailing whitespace",
        ));
    }

    let candidate_path = if Path::new(raw).is_absolute() {
        PathBuf::from(raw)
    } else {
        working_directory.join(raw)
    };

    match fs::metadata(&candidate_path) {
        Ok(metadata) if metadata.is_dir() => {
            let canonical_path = fs::canonicalize(&candidate_path).map_err(|source| {
                AppError::FilesystemOperation {
                    action: "canonicalize install source path",
                    path: candidate_path.clone(),
                    source,
                }
            })?;
            let url = Url::from_directory_path(&canonical_path)
                .map_err(|()| source_validation(raw, "local directory path is not portable"))?
                .to_string();

            Ok(ResolvedInstallSource {
                normalized: NormalizedInstallSource {
                    raw: raw.to_string(),
                    kind: SourceKind::LocalPath,
                    url,
                    display: canonical_path.display().to_string(),
                },
                local_path: Some(canonical_path),
            })
        }
        Ok(metadata) if metadata.is_file() => {
            if !is_supported_archive_path(&candidate_path) {
                return Err(source_validation(
                    raw,
                    format!(
                        "local file '{}' is not a supported archive ({})",
                        candidate_path.display(),
                        SUPPORTED_ARCHIVE_EXTENSIONS.join(", ")
                    ),
                ));
            }

            let canonical_path = fs::canonicalize(&candidate_path).map_err(|source| {
                AppError::FilesystemOperation {
                    action: "canonicalize install archive path",
                    path: candidate_path.clone(),
                    source,
                }
            })?;
            let url = Url::from_file_path(&canonical_path)
                .map_err(|()| source_validation(raw, "local archive path is not portable"))?
                .to_string();

            Ok(ResolvedInstallSource {
                normalized: NormalizedInstallSource {
                    raw: raw.to_string(),
                    kind: SourceKind::Archive,
                    url,
                    display: canonical_path.display().to_string(),
                },
                local_path: Some(canonical_path),
            })
        }
        Ok(_) => Err(source_validation(
            raw,
            "source path exists but is neither a file nor a directory",
        )),
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            if let Some(target) = well_known_target(raw)? {
                return Ok(ResolvedInstallSource {
                    normalized: NormalizedInstallSource {
                        raw: raw.to_string(),
                        kind: SourceKind::WellKnown,
                        url: target.index_url.to_string(),
                        display: target.index_url.to_string(),
                    },
                    local_path: None,
                });
            }

            if looks_like_git_url(raw) {
                Ok(ResolvedInstallSource {
                    normalized: NormalizedInstallSource {
                        raw: raw.to_string(),
                        kind: SourceKind::Git,
                        url: normalize_git_url(raw)?,
                        display: raw.to_string(),
                    },
                    local_path: None,
                })
            } else {
                Err(source_validation(
                    raw,
                    "source must be a Git URL, an HTTP(S) well-known discovery origin or index \
                     URL, existing local directory, or existing local archive",
                ))
            }
        }
        Err(source) => Err(AppError::FilesystemOperation {
            action: "inspect install source",
            path: candidate_path,
            source,
        }),
    }
}

fn prepare_source(source: &ResolvedInstallSource) -> Result<PreparedSource, AppError> {
    match source.normalized.kind {
        SourceKind::LocalPath => prepare_local_path(source),
        SourceKind::Archive => prepare_archive(source),
        SourceKind::Git => prepare_git(source),
        // A well-known source stages exactly one index entry, which
        // `prepare_install_source` selects before it calls this function.
        SourceKind::WellKnown => Err(source_validation(
            &source.normalized.raw,
            "a well-known index is staged one declared entry at a time, so the install selector \
             must choose the entry before the source is prepared",
        )),
    }
}

fn prepare_local_path(source: &ResolvedInstallSource) -> Result<PreparedSource, AppError> {
    let Some(path) = source.local_path.as_ref() else {
        return Err(source_validation(
            &source.normalized.raw,
            "local path sources must carry a canonical local path",
        ));
    };

    if is_direct_skill_root(path)? {
        return stage_direct_skill_source(source, path, hash_directory_contents(path)?);
    }

    Ok(PreparedSource {
        source: source.normalized.clone(),
        revision: SourceRevision {
            resolved: hash_directory_contents(path)?,
            upstream: None,
        },
        root: path.clone(),
        _staging_dir: None,
    })
}

/// Whether `root` is itself one skill directory rather than a collection.
fn is_direct_skill_root(root: &Path) -> Result<bool, AppError> {
    let manifest_path = root.join(SKILL_MANIFEST_FILE);
    match fs::metadata(&manifest_path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(AppError::FilesystemOperation {
            action: "inspect direct skill manifest",
            path: manifest_path,
            source,
        }),
    }
}

fn stage_direct_skill_source(
    source: &ResolvedInstallSource,
    path: &Path,
    resolved_revision: String,
) -> Result<PreparedSource, AppError> {
    let staging_dir = TempDir::new().map_err(|source| AppError::FilesystemOperation {
        action: "create direct skill staging directory",
        path: std::env::temp_dir(),
        source,
    })?;
    let staged_root = staging_dir.path().join("source");
    package_direct_skill(path, &staged_root)?;

    Ok(PreparedSource {
        source: source.normalized.clone(),
        revision: SourceRevision {
            resolved: resolved_revision,
            upstream: None,
        },
        root: staged_root,
        _staging_dir: Some(staging_dir),
    })
}

/// Copy one skill directory into the direct-skill packaging layout.
///
/// The skill is repackaged under `skills/<name>` so a staged source exposes the
/// documented packaging root instead of a bare skill directory.
fn package_direct_skill(skill_root: &Path, staged_root: &Path) -> Result<(), AppError> {
    let skill = SkillDefinition::load_from_dir(skill_root)?;
    let staged_skill_root = staged_root
        .join(DIRECT_SKILL_PACKAGING_ROOT)
        .join(skill.name.as_str());
    if let Some(parent) = staged_skill_root.parent() {
        fs::create_dir_all(parent).map_err(|source| AppError::FilesystemOperation {
            action: "create direct skill packaging root",
            path: parent.to_path_buf(),
            source,
        })?;
    }
    copy_source_tree(skill_root, &staged_skill_root)
}

fn prepare_archive(source: &ResolvedInstallSource) -> Result<PreparedSource, AppError> {
    let Some(archive_path) = source.local_path.as_ref() else {
        return Err(source_validation(
            &source.normalized.raw,
            "archive sources must carry a canonical archive path",
        ));
    };
    let staging_dir = TempDir::new().map_err(|source| AppError::FilesystemOperation {
        action: "create archive staging directory",
        path: std::env::temp_dir(),
        source,
    })?;
    let extract_root = staging_dir
        .path()
        .join(archive_content_dir_name(archive_path));
    fs::create_dir_all(&extract_root).map_err(|source| AppError::FilesystemOperation {
        action: "create archive extraction directory",
        path: extract_root.clone(),
        source,
    })?;

    extract_archive(archive_path, &extract_root, &source.normalized.raw)?;
    let root = normalized_extraction_root(&extract_root)?;
    let resolved_revision = hash_file_contents(archive_path)?;
    if is_direct_skill_root(&root)? {
        return stage_direct_skill_source(source, &root, resolved_revision);
    }

    Ok(PreparedSource {
        source: source.normalized.clone(),
        revision: SourceRevision {
            resolved: resolved_revision,
            upstream: None,
        },
        root,
        _staging_dir: Some(staging_dir),
    })
}

fn prepare_git(source: &ResolvedInstallSource) -> Result<PreparedSource, AppError> {
    let staging_dir = TempDir::new().map_err(|source| AppError::FilesystemOperation {
        action: "create git staging directory",
        path: std::env::temp_dir(),
        source,
    })?;
    let checkout_path = staging_dir.path().join("checkout");

    run_external_command(
        "git",
        &[
            OsStr::new("clone"),
            OsStr::new("--quiet"),
            OsStr::new("--depth"),
            OsStr::new("1"),
            OsStr::new(source.normalized.url.as_str()),
            checkout_path.as_os_str(),
        ],
        None,
        Some(&source.normalized.raw),
        "git clone failed",
    )?;

    let resolved = run_external_command(
        "git",
        &[OsStr::new("rev-parse"), OsStr::new("HEAD")],
        Some(&checkout_path),
        Some(&source.normalized.raw),
        "failed to resolve cloned git revision",
    )?;

    Ok(PreparedSource {
        source: source.normalized.clone(),
        revision: SourceRevision {
            resolved: resolved.clone(),
            upstream: Some(resolved),
        },
        root: checkout_path,
        _staging_dir: Some(staging_dir),
    })
}

/// Extraction root below a discovery staging directory.
///
/// A leading dot keeps it apart from every safe directory name an archive URL
/// can produce, including the `source` fallback for unsafe names.
const DISCOVERY_EXTRACT_DIR_NAME: &str = ".skillctl-extract";

/// Stage one verified discovery artifact as a local install source.
///
/// A `skill-md` artifact is written into a synthetic skill directory under the
/// direct-skill packaging root. An `archive` artifact is unpacked in process
/// inside the staging root, unwrapped through [`normalized_extraction_root`],
/// and packaged the same way when the unpacked tree is one bare skill directory.
/// The verified artifact digest becomes the staged revision, which names
/// immutable content the way a commit names a Git source.
pub(crate) fn stage_discovered_artifact(
    artifact: &crate::discovery::FetchedArtifact,
) -> Result<crate::discovery::StagedArtifact, AppError> {
    let staging_dir = TempDir::new().map_err(|source| AppError::FilesystemOperation {
        action: "create discovered artifact staging directory",
        path: std::env::temp_dir(),
        source,
    })?;
    let staged_root = staging_dir.path().join("source");

    let root = match artifact.artifact_type {
        crate::discovery::ArtifactType::SkillMd => {
            stage_discovered_skill_md(artifact, &staged_root)?;
            staged_root
        }
        crate::discovery::ArtifactType::Archive => {
            stage_discovered_archive(artifact, &staging_dir, &staged_root)?
        }
    };

    Ok(crate::discovery::StagedArtifact {
        name: artifact.name.clone(),
        artifact_type: artifact.artifact_type,
        digest: artifact.digest.clone(),
        root,
        _staging_dir: staging_dir,
    })
}

/// Write one verified `skill-md` artifact into the direct-skill packaging root.
///
/// The staged `SKILL.md` has to declare the name the discovery index declared
/// for the entry, because that name is what a later install pins.
fn stage_discovered_skill_md(
    artifact: &crate::discovery::FetchedArtifact,
    staged_root: &Path,
) -> Result<(), AppError> {
    let skill_root = staged_root
        .join(DIRECT_SKILL_PACKAGING_ROOT)
        .join(artifact.name.as_str());
    fs::create_dir_all(&skill_root).map_err(|source| AppError::FilesystemOperation {
        action: "create discovered skill staging directory",
        path: skill_root.clone(),
        source,
    })?;
    let manifest_path = skill_root.join(SKILL_MANIFEST_FILE);
    fs::write(&manifest_path, &artifact.bytes).map_err(|source| AppError::FilesystemOperation {
        action: "write discovered skill manifest",
        path: manifest_path.clone(),
        source,
    })?;

    let staged = SkillDefinition::load_from_dir(&skill_root).map_err(|error| {
        artifact_rejection(
            artifact,
            format!("the fetched SKILL.md is not valid: {error}"),
        )
    })?;
    if staged.name.as_str() != artifact.name {
        return Err(artifact_rejection(
            artifact,
            format!(
                "the fetched SKILL.md declares the name '{}', which does not match the name '{}' \
                 the discovery index declares",
                staged.name.as_str(),
                artifact.name
            ),
        ));
    }

    Ok(())
}

/// Unpack one verified `archive` artifact and normalize its extraction root.
///
/// Returns the root the install pipeline detects: the normalized extraction
/// root, or the direct-skill packaging root when the unpacked tree is one bare
/// skill directory.
fn stage_discovered_archive(
    artifact: &crate::discovery::FetchedArtifact,
    staging_dir: &TempDir,
    staged_root: &Path,
) -> Result<PathBuf, AppError> {
    let extract_root = staging_dir
        .path()
        .join(DISCOVERY_EXTRACT_DIR_NAME)
        .join(remote_archive_content_dir_name(&artifact.url));
    fs::create_dir_all(&extract_root).map_err(|source| AppError::FilesystemOperation {
        action: "create discovered archive extraction directory",
        path: extract_root.clone(),
        source,
    })?;

    let unpacked_entries = unpack_archive_bytes(
        &artifact.bytes,
        &extract_root,
        &UnpackLimits::default(),
        artifact.name.as_str(),
        artifact.url.as_str(),
    )?;
    if unpacked_entries == 0 {
        return Err(artifact_rejection(
            artifact,
            "archive did not contain any files",
        ));
    }

    let root = normalized_extraction_root(&extract_root)?;
    if is_direct_skill_root(&root)? {
        package_direct_skill(&root, staged_root)?;
        return Ok(staged_root.to_path_buf());
    }

    Ok(root)
}

/// Directory name a remote archive is extracted into, derived from its URL.
fn remote_archive_content_dir_name(url: &Url) -> String {
    let file_name = url
        .path_segments()
        .and_then(Iterator::last)
        .unwrap_or_default();
    archive_content_dir_name(Path::new(file_name))
}

/// A rejection of one discovery artifact, naming the skill and its URL.
fn artifact_rejection(
    artifact: &crate::discovery::FetchedArtifact,
    message: impl Into<String>,
) -> AppError {
    AppError::DiscoveryArtifactRejected {
        skill: artifact.name.clone(),
        url: artifact.url.to_string(),
        message: message.into(),
    }
}

/// A source tree staged to reconstruct one locked import.
#[derive(Debug)]
pub(crate) struct LockedSourceStaging {
    /// Staged source tree root that the locked subpath resolves against.
    pub root: PathBuf,
    /// Revision the staging actually resolved to.
    pub resolved_revision: String,
    /// Keeps the staging directory alive until the caller drops the staging.
    pub _staging_dir: Option<TempDir>,
}

/// Stage the exact content a lockfile pins for one import.
///
/// The locked revision is acquired literally. A branch HEAD, a tag that moved,
/// or any other newer state is never substituted for a missing revision: if the
/// locked revision is unavailable the acquisition fails.
pub(crate) fn acquire_locked_source(
    locked: &crate::lockfile::LockedImport,
) -> Result<LockedSourceStaging, AppError> {
    match locked.source.kind {
        SourceKind::Git => {
            let staged = acquire_git_revision(
                &locked.source.url,
                &locked.revision.resolved,
                &locked.source.url,
            )?;
            Ok(LockedSourceStaging {
                root: staged.root,
                resolved_revision: staged.revision.resolved,
                _staging_dir: staged._staging_dir,
            })
        }
        SourceKind::LocalPath | SourceKind::Archive => {
            let staged = stage_locked_local_source(locked.source.kind, &locked.source.url)?;
            Ok(LockedSourceStaging {
                root: staged.root,
                resolved_revision: locked.revision.resolved.clone(),
                _staging_dir: staged._staging_dir,
            })
        }
        SourceKind::WellKnown => {
            let staged = acquire_locked_well_known(locked)?;
            Ok(LockedSourceStaging {
                root: staged.root,
                resolved_revision: staged.revision.resolved,
                _staging_dir: staged._staging_dir,
            })
        }
    }
}

/// Re-fetch the artifact a locked well-known import was installed from.
///
/// The locked revision is the digest the index declared, so the same fetch that
/// installed the import reconstructs it: bytes that do not hash to the locked
/// digest are refused, and no other artifact is substituted for a missing one.
fn acquire_locked_well_known(
    locked: &crate::lockfile::LockedImport,
) -> Result<PreparedSource, AppError> {
    let transport = crate::discovery::HttpTransport::default();
    acquire_locked_well_known_with_transport(locked, &transport)
}

/// One staged well-known revision an update apply moves an import to.
#[derive(Debug)]
pub(crate) struct StagedWellKnownRevision {
    /// Staged install root the verified artifact produced.
    pub(crate) root: PathBuf,
    /// Discovery entry the index now declares, which the lockfile pins.
    pub(crate) discovery: crate::lockfile::LockedDiscoveryEntry,
    /// Reference the manifest records, which pins the verified digest.
    pub(crate) manifest_reference: String,
    /// Keeps the staging directory alive until this value is dropped.
    pub(crate) _staging_dir: TempDir,
}

/// Stage the artifact a discovery index now declares for one well-known import.
///
/// The index is re-read from the recorded source URL and the entry the import
/// selects is found by name. Its declared digest must equal `expected_digest`,
/// the digest the update check observed, so an index that moved again refuses
/// the apply instead of installing content the check never saw. The artifact is
/// then fetched and verified against that digest, the same acquisition an
/// install performs.
pub(crate) fn stage_well_known_revision(
    install: &InstallRecord,
    expected_digest: &str,
) -> Result<StagedWellKnownRevision, AppError> {
    let transport = crate::discovery::HttpTransport::default();
    crate::trust::ensure_discovery_origin_permits_acquisition(&install.source_url)?;

    let entry = match crate::discovery::lookup_index_entry(
        &install.source_url,
        &install.skill.skill_id,
        &transport,
    )? {
        crate::discovery::IndexLookup::Declared(entry) => entry,
        crate::discovery::IndexLookup::NotDeclared => {
            return Err(source_validation(
                &install.source_url,
                format!(
                    "the discovery index no longer declares '{}', so no artifact can update it",
                    install.skill.skill_id
                ),
            ));
        }
    };
    if entry.digest.as_str() != expected_digest {
        return Err(source_validation(
            &install.source_url,
            format!(
                "the discovery index now declares the digest {} while this apply targets {}; \
                 re-run 'skillctl update {}' to observe the newer entry",
                entry.digest.as_str(),
                expected_digest,
                install.skill.skill_id
            ),
        ));
    }

    let staged = crate::discovery::fetch_and_stage_artifact(&entry, &transport)?;
    Ok(StagedWellKnownRevision {
        root: staged.root,
        discovery: crate::lockfile::LockedDiscoveryEntry {
            skill: entry.name.clone(),
            artifact_type: entry.artifact_type,
            url: entry.url.to_string(),
            digest: entry.digest.as_str().to_string(),
        },
        manifest_reference: staged.digest.as_str().to_string(),
        _staging_dir: staged._staging_dir,
    })
}

/// [`acquire_locked_well_known`] against an explicit transport, shared with the
/// tests so the acquisition runs without a network.
fn acquire_locked_well_known_with_transport(
    locked: &crate::lockfile::LockedImport,
    transport: &dyn DiscoveryTransport,
) -> Result<PreparedSource, AppError> {
    let entry = locked.discovery.as_ref().ok_or_else(|| {
        source_validation(
            &locked.source.url,
            "a well-known import must record the discovery index entry it was installed from",
        )
    })?;
    // A restore acquires from the recorded origin, so the same origin-trust
    // policy that gated the install gates the re-fetch.
    crate::trust::ensure_discovery_origin_permits_acquisition(&locked.source.url)?;
    let artifact_url = Url::parse(&entry.url).map_err(|source| {
        source_validation(
            &locked.source.url,
            format!(
                "the recorded artifact URL '{}' is not valid: {source}",
                entry.url
            ),
        )
    })?;
    let digest = crate::discovery::ArtifactDigest::parse(&entry.digest).map_err(|reason| {
        source_validation(
            &locked.source.url,
            format!(
                "the recorded artifact digest '{}' is invalid: {reason}",
                entry.digest
            ),
        )
    })?;
    let artifact_entry = crate::discovery::DiscoveryEntry {
        name: entry.skill.clone(),
        artifact_type: entry.artifact_type,
        description: String::new(),
        declared_url: entry.url.clone(),
        url: artifact_url,
        digest,
    };

    let staged = crate::discovery::fetch_and_stage_artifact(&artifact_entry, transport)?;
    if staged.digest.as_str() != locked.revision.resolved {
        return Err(source_validation(
            &locked.source.url,
            format!(
                "the artifact at '{}' hashes to {} but the lockfile pins {}; skillctl never \
                 substitutes one revision for another",
                entry.url,
                staged.digest.as_str(),
                locked.revision.resolved
            ),
        ));
    }

    Ok(PreparedSource {
        source: NormalizedInstallSource {
            raw: locked.source.url.clone(),
            kind: SourceKind::WellKnown,
            url: locked.source.url.clone(),
            display: locked.source.url.clone(),
        },
        revision: SourceRevision {
            resolved: staged.digest.as_str().to_string(),
            upstream: None,
        },
        root: staged.root,
        _staging_dir: Some(staged._staging_dir),
    })
}

/// Clone exactly one commit, refusing every substitute for it.
fn acquire_git_revision(url: &str, revision: &str, raw: &str) -> Result<PreparedSource, AppError> {
    let staging_dir = TempDir::new().map_err(|source| AppError::FilesystemOperation {
        action: "create locked git staging directory",
        path: std::env::temp_dir(),
        source,
    })?;
    let checkout_path = staging_dir.path().join("checkout");

    run_external_command(
        "git",
        &[
            OsStr::new("init"),
            OsStr::new("--quiet"),
            checkout_path.as_os_str(),
        ],
        None,
        Some(raw),
        "git init failed",
    )?;
    run_external_command(
        "git",
        &[
            OsStr::new("remote"),
            OsStr::new("add"),
            OsStr::new("origin"),
            OsStr::new(url),
        ],
        Some(&checkout_path),
        Some(raw),
        "failed to register the locked git source",
    )?;

    // Some servers refuse to serve an arbitrary object by name. Fall back to a
    // full fetch, then check out the locked commit from whatever arrived.
    let fetched_by_revision = run_external_command(
        "git",
        &[
            OsStr::new("fetch"),
            OsStr::new("--quiet"),
            OsStr::new("--depth"),
            OsStr::new("1"),
            OsStr::new("origin"),
            OsStr::new(revision),
        ],
        Some(&checkout_path),
        Some(raw),
        "git fetch failed",
    );
    if fetched_by_revision.is_err() {
        run_external_command(
            "git",
            &[
                OsStr::new("fetch"),
                OsStr::new("--quiet"),
                OsStr::new("origin"),
            ],
            Some(&checkout_path),
            Some(raw),
            "git fetch failed",
        )?;
    }

    // The fetches above reached the source, so a revision that is still absent
    // is an object the source does not serve, not a transport failure. Probe
    // for the object directly instead of letting `git checkout` report it as a
    // usage error.
    if !git_object_is_present(&checkout_path, revision, raw)? {
        return Err(source_validation(
            raw,
            format!(
                "locked revision {revision} is unavailable from {url}: the source was contacted \
                 but does not serve this revision. This is not a transport failure. The pinned \
                 'revision.resolved' is recorded incorrectly, or the revision no longer exists at \
                 the source. skillctl never substitutes another revision for a locked one"
            ),
        ));
    }

    run_external_command(
        "git",
        &[
            OsStr::new("checkout"),
            OsStr::new("--quiet"),
            OsStr::new("--detach"),
            OsStr::new(revision),
        ],
        Some(&checkout_path),
        Some(raw),
        &format!("failed to check out locked revision {revision} from {url}"),
    )?;

    let resolved = run_external_command(
        "git",
        &[OsStr::new("rev-parse"), OsStr::new("HEAD")],
        Some(&checkout_path),
        Some(raw),
        "failed to resolve the checked out git revision",
    )?;
    if resolved != revision {
        return Err(source_validation(
            raw,
            format!(
                "locked revision {revision} resolved to {resolved}; skillctl never substitutes a \
                 different revision for a locked one"
            ),
        ));
    }

    Ok(PreparedSource {
        source: NormalizedInstallSource {
            raw: raw.to_string(),
            kind: SourceKind::Git,
            url: url.to_string(),
            display: url.to_string(),
        },
        revision: SourceRevision {
            resolved: resolved.clone(),
            upstream: Some(resolved),
        },
        root: checkout_path,
        _staging_dir: Some(staging_dir),
    })
}

/// Whether the staged repository holds the object `revision` names.
///
/// `git cat-file -e` answers through its exit status: 0 means the object is
/// present, 1 means it is absent, and 128 means the name is not a valid object
/// name at all. Both non-zero cases describe a revision the source cannot
/// serve, which stays distinct from the transport failures `git fetch` reports.
fn git_object_is_present(
    checkout_path: &Path,
    revision: &str,
    raw: &str,
) -> Result<bool, AppError> {
    let output = Command::new("git")
        .args(["cat-file", "-e", revision])
        .current_dir(checkout_path)
        .output()
        .map_err(|source| AppError::ExternalCommand {
            command: "git".to_string(),
            source,
        })?;

    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) | Some(128) => Ok(false),
        status => Err(source_validation(
            raw,
            format!(
                "git cat-file exited with status {status:?} while checking for revision \
                 {revision}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        )),
    }
}

/// Stage the local path or archive a lockfile points at.
///
/// Local sources are only portable while the path they name exists. A missing
/// path is reported as an actionable, nonportable-source failure instead of a
/// silent fallback to some other content.
fn stage_locked_local_source(kind: SourceKind, url: &str) -> Result<PreparedSource, AppError> {
    let parsed = Url::parse(url).map_err(|source| {
        source_validation(url, format!("locked source URL is not valid: {source}"))
    })?;
    let local_path = parsed
        .to_file_path()
        .map_err(|()| source_validation(url, "locked local source URL does not name a file"))?;

    let metadata = fs::metadata(&local_path).map_err(|source| {
        source_validation(
            url,
            format!(
                "locked local source '{}' is not available on this machine ({source}); the \
                 import is only reproducible from a retained artifact or from the same local \
                 path. Run 'skillctl remove <skill>' and install the skill again from a \
                 portable source such as a Git URL, restore the artifact cache under \
                 ~/.skillctl/store/artifacts, or fetch the source path from the machine that \
                 produced the lockfile",
                local_path.display()
            ),
        )
    })?;

    let expected = match kind {
        SourceKind::Archive => "a file",
        _ => "a directory",
    };
    let is_dir = metadata.is_dir();
    if !matches!(
        (kind, is_dir),
        (SourceKind::Archive, false) | (SourceKind::LocalPath, true)
    ) {
        return Err(source_validation(
            url,
            format!(
                "locked source '{}' is {expected} but the recorded source kind no longer matches",
                local_path.display()
            ),
        ));
    }

    let resolved = ResolvedInstallSource {
        normalized: NormalizedInstallSource {
            raw: url.to_string(),
            kind,
            url: url.to_string(),
            display: local_path.display().to_string(),
        },
        local_path: Some(local_path),
    };
    match kind {
        SourceKind::Archive => prepare_archive(&resolved),
        SourceKind::LocalPath => prepare_local_path(&resolved),
        SourceKind::Git | SourceKind::WellKnown => Err(source_validation(
            url,
            "the recorded source kind no longer matches a local source",
        )),
    }
}

fn detect_install_candidates(
    root: &Path,
    raw_source: &str,
) -> Result<Vec<InstallCandidate>, AppError> {
    let _ = compatibility_registry();
    let mut candidates = Vec::new();

    for detection_root in DETECTION_ROOTS {
        let container = root.join(detection_root.relative_path);
        let metadata = match fs::metadata(&container) {
            Ok(metadata) => metadata,
            Err(source) if source.kind() == io::ErrorKind::NotFound => continue,
            Err(source) => {
                return Err(AppError::FilesystemOperation {
                    action: "inspect candidate source root",
                    path: container,
                    source,
                });
            }
        };

        if !metadata.is_dir() {
            return Err(AppError::PathConflict {
                path: container,
                expected: "directory",
            });
        }

        let mut skill_directories = Vec::new();
        for entry in fs::read_dir(&container).map_err(|source| AppError::FilesystemOperation {
            action: "read candidate source root",
            path: container.clone(),
            source,
        })? {
            let entry = entry.map_err(|source| AppError::FilesystemOperation {
                action: "read candidate source root entry",
                path: container.clone(),
                source,
            })?;
            let path = entry.path();
            let metadata = entry
                .metadata()
                .map_err(|source| AppError::FilesystemOperation {
                    action: "inspect detected skill path",
                    path: path.clone(),
                    source,
                })?;
            if metadata.is_dir() {
                skill_directories.push(path);
            }
        }

        skill_directories.sort();
        for skill_root in skill_directories {
            let skill = SkillDefinition::load_from_dir(&skill_root)?;
            let selected_subpath = relative_path_string(root, &skill.root, raw_source)?;
            let contains_scripts = crate::trust::directory_contains_scripts(&skill.root)?;
            candidates.push(InstallCandidate {
                display_name: skill.name.as_str().to_string(),
                name: skill.name.as_str().to_string(),
                source_path: selected_subpath.clone(),
                selected_subpath,
                compatible_targets: detection_root.compatible_targets.to_vec(),
                compatibility_hints: build_compatibility_hints(detection_root),
                safety: skill.safety.clone(),
                trust: crate::trust::SkillTrust::imported_unreviewed(
                    skill.name.as_str(),
                    contains_scripts,
                ),
            });
        }
    }

    if candidates.is_empty() {
        return Err(source_validation(
            raw_source,
            format!(
                "no supported skills were found under {}",
                DETECTION_ROOTS
                    .iter()
                    .map(|root| root.relative_path)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ));
    }

    Ok(candidates)
}

fn build_compatibility_hints(root: &DetectionRoot) -> Vec<String> {
    if root.compatible_targets.is_empty() {
        vec![root.note.to_string()]
    } else {
        vec![
            root.note.to_string(),
            format!(
                "documented for {}",
                root.compatible_targets
                    .iter()
                    .map(|target| target.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ]
    }
}

fn compatibility_registry() -> AdapterRegistry {
    let registry = AdapterRegistry::new();
    for detection_root in DETECTION_ROOTS {
        let discovered_targets: Vec<_> = TargetRuntime::all()
            .iter()
            .copied()
            .filter(|target| {
                registry
                    .get(*target)
                    .roots_for_scope(TargetScope::Workspace)
                    .iter()
                    .any(|root| root.path == detection_root.relative_path)
            })
            .collect();

        debug_assert_eq!(
            discovered_targets, detection_root.compatible_targets,
            "detection root compatibility drifted from adapter metadata"
        );
    }
    registry
}

fn looks_like_git_url(raw: &str) -> bool {
    raw.starts_with("git@")
        || raw.starts_with("ssh://")
        || raw.starts_with("git://")
        || raw.starts_with("http://")
        || raw.starts_with("https://")
        || raw.starts_with("file://")
}

/// Resolve one install source as a well-known discovery target.
///
/// Only an explicit HTTP or HTTPS URL can name a discovery source, so a bare
/// host or a mistyped relative path keeps its existing Git-or-local diagnosis.
/// Within those schemes, a URL whose path is the origin root names a discovery
/// origin, because no Git repository is served at that path, and a URL whose
/// final path segment ends in `.json` names an index document. Every other URL
/// keeps the Git interpretation, including repository URLs without an extension.
fn well_known_target(raw: &str) -> Result<Option<crate::discovery::DiscoveryTarget>, AppError> {
    let Ok(parsed) = Url::parse(raw) else {
        return Ok(None);
    };
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none_or(str::is_empty) {
        return Ok(None);
    }

    let names_an_index = parsed.path() == "/"
        || parsed
            .path_segments()
            .and_then(Iterator::last)
            .is_some_and(|segment| segment.ends_with(".json"));
    if !names_an_index {
        return Ok(None);
    }

    crate::discovery::resolve_target(raw).map(Some)
}

fn normalize_git_url(raw: &str) -> Result<String, AppError> {
    if raw.starts_with("git@") {
        return Ok(raw.trim_end_matches('/').to_string());
    }

    let mut url = Url::parse(raw).map_err(|_| {
        source_validation(
            raw,
            "Git source must be a valid URL or SCP-style git remote",
        )
    })?;
    if !matches!(url.scheme(), "http" | "https" | "ssh" | "git" | "file") {
        return Err(source_validation(
            raw,
            format!("unsupported Git URL scheme '{}'", url.scheme()),
        ));
    }

    url.set_query(None);
    url.set_fragment(None);
    if url.path() != "/" {
        let trimmed = url.path().trim_end_matches('/').to_string();
        url.set_path(&trimmed);
    }

    Ok(url.to_string())
}

fn is_supported_archive_path(path: &Path) -> bool {
    let lower = path.to_string_lossy().to_ascii_lowercase();
    SUPPORTED_ARCHIVE_EXTENSIONS
        .iter()
        .any(|extension| lower.ends_with(extension))
}

fn extract_archive(path: &Path, destination: &Path, raw_source: &str) -> Result<(), AppError> {
    if path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("zip"))
    {
        run_external_command(
            "unzip",
            &[
                OsStr::new("-qq"),
                path.as_os_str(),
                OsStr::new("-d"),
                destination.as_os_str(),
            ],
            None,
            Some(raw_source),
            "archive extraction failed",
        )?;
    } else {
        run_external_command(
            "tar",
            &[
                OsStr::new("-xf"),
                path.as_os_str(),
                OsStr::new("-C"),
                destination.as_os_str(),
            ],
            None,
            Some(raw_source),
            "archive extraction failed",
        )?;
    }

    Ok(())
}

/// Entry count one remote archive may declare before it is rejected.
const MAX_ARCHIVE_ENTRIES: usize = 4_096;
/// Unpacked bytes one remote archive entry may hold before it is rejected.
const MAX_ARCHIVE_ENTRY_BYTES: u64 = 64 * 1024 * 1024;
/// Unpacked bytes one remote archive may hold in total before it is rejected.
const MAX_ARCHIVE_TOTAL_BYTES: u64 = 256 * 1024 * 1024;

/// Budget one unpacked remote archive has to stay inside.
///
/// The limits are absolute instead of a compression ratio, because a ratio says
/// nothing about how much data a small archive can expand into.
#[derive(Clone, Copy, Debug)]
pub(crate) struct UnpackLimits {
    /// Maximum number of entries one archive may declare.
    pub max_entries: usize,
    /// Maximum unpacked bytes a single entry may hold.
    pub max_entry_bytes: u64,
    /// Maximum unpacked bytes the whole archive may hold.
    pub max_total_bytes: u64,
}

impl Default for UnpackLimits {
    fn default() -> Self {
        Self {
            max_entries: MAX_ARCHIVE_ENTRIES,
            max_entry_bytes: MAX_ARCHIVE_ENTRY_BYTES,
            max_total_bytes: MAX_ARCHIVE_TOTAL_BYTES,
        }
    }
}

/// The skill and URL a rejected remote artifact is reported with.
struct UnpackContext<'a> {
    /// Skill the artifact belongs to.
    skill: &'a str,
    /// URL the artifact was fetched from.
    url: &'a str,
}

impl UnpackContext<'_> {
    /// Reject the artifact with `message`.
    fn reject(&self, message: impl Into<String>) -> AppError {
        AppError::DiscoveryArtifactRejected {
            skill: self.skill.to_string(),
            url: self.url.to_string(),
            message: message.into(),
        }
    }
}

/// Container formats this build unpacks in process.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RemoteArchiveFormat {
    /// A ZIP archive.
    Zip,
    /// An uncompressed tar archive.
    Tar,
    /// A gzip-compressed tar archive.
    TarGzip,
}

/// Detect the container format of a remote archive from its leading bytes.
///
/// Detection is content based because a discovery artifact URL does not have to
/// carry a file extension, and only formats skillctl unpacks itself are
/// recognized.
fn detect_remote_archive_format(bytes: &[u8]) -> Option<RemoteArchiveFormat> {
    if bytes.starts_with(b"PK\x03\x04") || bytes.starts_with(b"PK\x05\x06") {
        return Some(RemoteArchiveFormat::Zip);
    }
    if bytes.starts_with(&[0x1f, 0x8b]) {
        return Some(RemoteArchiveFormat::TarGzip);
    }
    // Modern tar writers record the `ustar` magic in every header block.
    if bytes.len() >= 512 && bytes[257..262] == *b"ustar" {
        return Some(RemoteArchiveFormat::Tar);
    }
    None
}

/// Unpack a remote archive inside `destination` without running a shell tool.
///
/// Every entry is validated before anything is written. Entries that name a
/// path outside `destination`, entries that are links, and entries that declare
/// or produce more than the allowed budget are rejected by name. Archive
/// permission bits are dropped, so unpacked content arrives non-executable and
/// the trust gate stays in charge of executable content.
///
/// `destination` must be a dedicated, empty directory. A rejected archive
/// leaves it empty again, so no partial unpack ever reaches a caller.
///
/// Returns how many entries were written, which is zero for an archive that
/// carries no content of its own.
pub(crate) fn unpack_archive_bytes(
    bytes: &[u8],
    destination: &Path,
    limits: &UnpackLimits,
    skill: &str,
    url: &str,
) -> Result<usize, AppError> {
    let context = UnpackContext { skill, url };
    let unpacked = unpack_detected_archive(bytes, destination, limits, &context);
    if unpacked.is_err() {
        // A rejection means the archive is not used, so the bytes it already
        // wrote must not survive it.
        let _ = fs::remove_dir_all(destination);
        let _ = fs::create_dir_all(destination);
    }

    unpacked
}

/// Unpack an archive whose container format was already detected.
fn unpack_detected_archive(
    bytes: &[u8],
    destination: &Path,
    limits: &UnpackLimits,
    context: &UnpackContext<'_>,
) -> Result<usize, AppError> {
    let format = detect_remote_archive_format(bytes).ok_or_else(|| {
        context.reject(format!(
            "archive is not a format skillctl unpacks in process; supported formats are zip and \
             tar, optionally gzip compressed, while the artifact starts with the bytes {:?}",
            &bytes[..bytes.len().min(8)]
        ))
    })?;

    match format {
        RemoteArchiveFormat::Zip => unpack_zip(bytes, destination, limits, context),
        RemoteArchiveFormat::Tar => unpack_tar(bytes, destination, limits, context),
        RemoteArchiveFormat::TarGzip => {
            let decoder = flate2::read::MultiGzDecoder::new(bytes);
            unpack_tar(decoder, destination, limits, context)
        }
    }
}

/// Unpack a ZIP archive that was already verified against its digest.
fn unpack_zip(
    bytes: &[u8],
    destination: &Path,
    limits: &UnpackLimits,
    context: &UnpackContext<'_>,
) -> Result<usize, AppError> {
    let mut archive = zip::ZipArchive::new(io::Cursor::new(bytes))
        .map_err(|source| context.reject(format!("archive cannot be read as zip: {source}")))?;
    if archive.len() > limits.max_entries {
        return Err(context.reject(format!(
            "archive declares {} entries, which exceeds the {} entry limit",
            archive.len(),
            limits.max_entries
        )));
    }

    let mut written = 0_usize;
    let mut total_bytes = 0_u64;
    for index in 0..archive.len() {
        let mut file = archive.by_index(index).map_err(|source| {
            context.reject(format!("zip entry {index} cannot be read: {source}"))
        })?;
        let name = file.name().to_string();
        if file.is_symlink() {
            return Err(context.reject(format!(
                "entry '{name}' is a symbolic or hard link; linked archive entries are never \
                 unpacked"
            )));
        }
        let entry_path = PathBuf::from(&name);
        if is_macos_metadata(&entry_path) {
            continue;
        }
        let destination_path = safe_entry_path(destination, &entry_path)
            .map_err(|reason| context.reject(format!("entry '{name}' {reason}")))?;

        if file.is_dir() {
            create_unpack_directory(&destination_path).map_err(|reason| {
                context.reject(format!("entry '{name}' cannot be created: {reason}"))
            })?;
            written += 1;
            continue;
        }

        let declared_bytes = file.size();
        write_unpacked_entry(
            &mut file,
            &destination_path,
            declared_bytes,
            &entry_path,
            &mut total_bytes,
            limits,
            context,
        )?;
        written += 1;
    }

    Ok(written)
}

/// Unpack a tar archive, optionally gzip compressed, that was already verified
/// against its digest.
fn unpack_tar<R: Read>(
    reader: R,
    destination: &Path,
    limits: &UnpackLimits,
    context: &UnpackContext<'_>,
) -> Result<usize, AppError> {
    let mut archive = tar::Archive::new(reader);
    archive.set_preserve_permissions(false);
    archive.set_unpack_xattrs(false);

    let mut written = 0_usize;
    let mut total_bytes = 0_u64;
    let mut entries = 0_usize;
    let entry_iterator = archive
        .entries()
        .map_err(|source| context.reject(format!("archive cannot be read as tar: {source}")))?;

    for entry in entry_iterator {
        let mut entry = entry
            .map_err(|source| context.reject(format!("tar entry cannot be read: {source}")))?;
        entries += 1;
        if entries > limits.max_entries {
            return Err(context.reject(format!(
                "archive declares more than the {} entry limit",
                limits.max_entries
            )));
        }

        let entry_type = entry.header().entry_type();
        let entry_path = entry
            .path()
            .map_err(|source| {
                context.reject(format!("tar entry has an unreadable name: {source}"))
            })?
            .into_owned();
        let linked_name = entry.link_name().map_err(|source| {
            context.reject(format!(
                "tar entry '{}' has an unreadable link name: {source}",
                entry_path.display()
            ))
        })?;
        if linked_name.is_some() || entry_type.is_symlink() || entry_type.is_hard_link() {
            return Err(context.reject(format!(
                "entry '{}' is a symbolic or hard link; linked archive entries are never unpacked",
                entry_path.display()
            )));
        }
        if is_macos_metadata(&entry_path) {
            continue;
        }
        let destination_path = safe_entry_path(destination, &entry_path).map_err(|reason| {
            context.reject(format!("entry '{}' {reason}", entry_path.display()))
        })?;

        if entry_type.is_dir() {
            create_unpack_directory(&destination_path).map_err(|reason| {
                context.reject(format!(
                    "entry '{}' cannot be created: {reason}",
                    entry_path.display()
                ))
            })?;
            written += 1;
            continue;
        }
        if !entry_type.is_file() {
            return Err(context.reject(format!(
                "entry '{}' is not a file or a directory",
                entry_path.display()
            )));
        }

        let declared_bytes = entry.header().size().map_err(|source| {
            context.reject(format!(
                "tar entry '{}' has an unreadable size: {source}",
                entry_path.display()
            ))
        })?;
        write_unpacked_entry(
            &mut entry,
            &destination_path,
            declared_bytes,
            &entry_path,
            &mut total_bytes,
            limits,
            context,
        )?;
        written += 1;
    }

    Ok(written)
}

/// Create one unpacked directory.
fn create_unpack_directory(path: &Path) -> Result<(), String> {
    fs::create_dir_all(path)
        .map_err(|source| format!("directory '{}' cannot be created: {source}", path.display()))
}

/// Write one unpacked file, enforcing the size budget while the bytes flow.
///
/// The declared size is checked before the file is created, and the copy itself
/// stops one byte past the per-entry limit, so an archive whose header lies
/// about a size is still rejected.
fn write_unpacked_entry(
    reader: &mut dyn Read,
    destination_path: &Path,
    declared_bytes: u64,
    entry_path: &Path,
    total_bytes: &mut u64,
    limits: &UnpackLimits,
    context: &UnpackContext<'_>,
) -> Result<(), AppError> {
    let describe =
        |message: String| context.reject(format!("entry '{}' {message}", entry_path.display()));

    if declared_bytes > limits.max_entry_bytes {
        return Err(describe(format!(
            "declares {declared_bytes} bytes, which exceeds the {} byte per-entry limit",
            limits.max_entry_bytes
        )));
    }
    if total_bytes.saturating_add(declared_bytes) > limits.max_total_bytes {
        return Err(describe(format!(
            "would unpack past the {} byte total limit",
            limits.max_total_bytes
        )));
    }

    if let Some(parent) = destination_path.parent() {
        create_unpack_directory(parent)
            .map_err(|reason| describe(format!("cannot be created: {reason}")))?;
    }
    let mut file = fs::File::create(destination_path)
        .map_err(|source| describe(format!("cannot be created: {source}")))?;

    let mut limited = reader.take(limits.max_entry_bytes.saturating_add(1));
    let written = io::copy(&mut limited, &mut file)
        .map_err(|source| describe(format!("cannot be unpacked: {source}")))?;
    if written > limits.max_entry_bytes {
        return Err(describe(format!(
            "unpacks past the {} byte per-entry limit",
            limits.max_entry_bytes
        )));
    }

    *total_bytes += written;
    if *total_bytes > limits.max_total_bytes {
        return Err(describe(format!(
            "unpacks past the {} byte total limit",
            limits.max_total_bytes
        )));
    }

    Ok(())
}

/// Resolve one archive entry name to a path inside `destination`.
///
/// Absolute names, parent-directory components, backslashes (a path separator
/// on Windows), reserved Windows device names, and non-UTF-8 or empty names are
/// rejected, so no entry can name anything outside `destination`.
fn safe_entry_path(destination: &Path, entry_path: &Path) -> Result<PathBuf, String> {
    let raw = entry_path
        .to_str()
        .ok_or_else(|| "has a name that is not valid UTF-8".to_string())?;
    if raw.contains('\0') {
        return Err("contains a NUL byte".to_string());
    }
    if raw.contains('\\') {
        return Err("contains a backslash, which is a path separator on Windows".to_string());
    }
    if raw.contains(':') {
        return Err("contains ':', which names a drive or a data stream on Windows".to_string());
    }
    if entry_path.is_absolute() {
        return Err("is an absolute path".to_string());
    }

    let mut safe = PathBuf::new();
    for component in entry_path.components() {
        match component {
            Component::Normal(segment) => {
                let name = segment
                    .to_str()
                    .ok_or_else(|| "has a name that is not valid UTF-8".to_string())?;
                if name.is_empty() {
                    return Err("has an empty path segment".to_string());
                }
                if is_windows_device_name(name) {
                    return Err(format!("uses the Windows device name '{name}'"));
                }
                safe.push(segment);
            }
            Component::CurDir => {}
            Component::ParentDir => {
                return Err("escapes the extraction directory through '..'".to_string());
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err("is an absolute path".to_string());
            }
        }
    }

    if safe.as_os_str().is_empty() {
        return Err("is empty".to_string());
    }

    Ok(destination.join(safe))
}

/// Whether `name` is reserved as a device name on Windows.
///
/// Windows resolves these names, with or without an extension, to devices
/// instead of files, so they cannot name an unpacked entry.
fn is_windows_device_name(name: &str) -> bool {
    let base = name.split('.').next().unwrap_or_default();
    WINDOWS_RESERVED_DIR_NAMES.contains(&base.to_ascii_lowercase().as_str())
}

/// Whether one archive entry is macOS packaging metadata.
///
/// `__MACOSX` directories carry resource forks and symlink records that
/// describe how an archive was packed, not skill content.
fn is_macos_metadata(path: &Path) -> bool {
    path.components()
        .next()
        .is_some_and(|component| component.as_os_str() == OsStr::new("__MACOSX"))
}

/// Directory name an archive is extracted into.
///
/// The extraction directory is also the parent directory of a skill whose
/// files sit directly at the archive top level, so it carries the archive's
/// own name without its archive extension: `my-skill.zip` containing
/// `SKILL.md` validates the same way as an unpacked `my-skill/` directory.
/// Names that are not safe directory names fall back to `source`.
fn archive_content_dir_name(archive_path: &Path) -> String {
    let file_name = archive_path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let lowered = file_name.to_ascii_lowercase();
    let extension = SUPPORTED_ARCHIVE_EXTENSIONS
        .iter()
        .filter(|extension| lowered.ends_with(*extension))
        .max_by_key(|extension| extension.len());
    let stem = match extension {
        Some(extension) => &file_name[..file_name.len() - extension.len()],
        None => file_name.as_str(),
    };

    if is_safe_directory_name(stem) {
        stem.to_string()
    } else {
        ARCHIVE_FALLBACK_DIR_NAME.to_string()
    }
}

/// Whether `name` is safe to create as a directory on every supported
/// platform: portable characters, no leading or trailing dot, and no Windows
/// device name.
fn is_safe_directory_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && !name.ends_with('.')
        && name.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        })
        && !is_windows_device_name(name)
}

/// Fallback extraction directory for archives whose file name is not a safe
/// directory name.
const ARCHIVE_FALLBACK_DIR_NAME: &str = "source";

/// Directory names Windows reserves for devices, which cannot be used as a
/// directory name on that platform.
const WINDOWS_RESERVED_DIR_NAMES: &[&str] = &[
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
    "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

/// Choose the root an extracted archive is inspected from.
///
/// Archives usually wrap their content in one release directory, for example
/// `packed-1.0/skills/<skill>/SKILL.md`. That wrapper is unwrapped so
/// detection reaches the documented roots. A top level that already is a
/// documented layout keeps the extraction root, because unwrapping it would
/// make detection probe `skills/skills` and reject an archive whose unpacked
/// tree installs as a directory source.
fn normalized_extraction_root(extract_root: &Path) -> Result<PathBuf, AppError> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(extract_root).map_err(|source| AppError::FilesystemOperation {
        action: "read extracted archive root",
        path: extract_root.to_path_buf(),
        source,
    })? {
        let entry = entry.map_err(|source| AppError::FilesystemOperation {
            action: "read extracted archive entry",
            path: extract_root.to_path_buf(),
            source,
        })?;
        if entry.file_name() == OsStr::new("__MACOSX") {
            continue;
        }
        entries.push(entry.path());
    }

    entries.sort();
    if entries.is_empty() {
        return Err(source_validation(
            extract_root.display().to_string(),
            "archive did not contain any files",
        ));
    }

    let unwraps_single_wrapper =
        entries.len() == 1 && fs::metadata(&entries[0]).is_ok_and(|metadata| metadata.is_dir());
    if !unwraps_single_wrapper || root_carries_documented_layout(extract_root)? {
        return Ok(extract_root.to_path_buf());
    }

    Ok(entries.remove(0))
}

/// Whether `root` exposes a layout install documents: a bare skill directory,
/// or any of the documented detection roots.
fn root_carries_documented_layout(root: &Path) -> Result<bool, AppError> {
    if is_direct_skill_root(root)? {
        return Ok(true);
    }

    for detection_root in DETECTION_ROOTS {
        let container = root.join(detection_root.relative_path);
        match fs::metadata(&container) {
            Ok(metadata) if metadata.is_dir() => return Ok(true),
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(AppError::FilesystemOperation {
                    action: "inspect extracted detection root",
                    path: container,
                    source,
                });
            }
        }
    }

    Ok(false)
}

pub(crate) fn hash_directory_contents(root: &Path) -> Result<String, AppError> {
    let mut entries = Vec::new();
    collect_relative_paths(root, root, &mut entries)?;
    entries.sort();

    let mut hasher = Sha256::new();
    for relative in entries {
        let full_path = root.join(&relative);
        let metadata =
            fs::symlink_metadata(&full_path).map_err(|source| AppError::FilesystemOperation {
                action: "inspect source content for hashing",
                path: full_path.clone(),
                source,
            })?;
        let relative_string = relative_path_for_hash(&relative)
            .map_err(|message| source_validation(root.display().to_string(), message))?;

        if metadata.is_dir() {
            hasher.update(b"dir\0");
            hasher.update(relative_string.as_bytes());
            hasher.update(b"\0");
            continue;
        }
        if metadata.file_type().is_symlink() {
            let target =
                fs::read_link(&full_path).map_err(|source| AppError::FilesystemOperation {
                    action: "read symlink target for hashing",
                    path: full_path.clone(),
                    source,
                })?;
            hasher.update(b"symlink\0");
            hasher.update(relative_string.as_bytes());
            hasher.update(b"\0");
            hasher.update(
                relative_path_for_hash(&target)
                    .map_err(|message| source_validation(root.display().to_string(), message))?
                    .as_bytes(),
            );
            hasher.update(b"\0");
            continue;
        }

        hasher.update(b"file\0");
        hasher.update(relative_string.as_bytes());
        hasher.update(b"\0");
        hash_file_into(&full_path, &mut hasher)?;
        hasher.update(b"\0");
    }

    Ok(format!("sha256:{:x}", hasher.finalize()))
}

fn hash_file_contents(path: &Path) -> Result<String, AppError> {
    let mut hasher = Sha256::new();
    hash_file_into(path, &mut hasher)?;
    Ok(format!("sha256:{:x}", hasher.finalize()))
}

fn hash_file_into(path: &Path, hasher: &mut Sha256) -> Result<(), AppError> {
    let mut file = File::open(path).map_err(|source| AppError::FilesystemOperation {
        action: "open file for hashing",
        path: path.to_path_buf(),
        source,
    })?;
    let mut buffer = [0_u8; 8192];

    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|source| AppError::FilesystemOperation {
                action: "read file for hashing",
                path: path.to_path_buf(),
                source,
            })?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }

    Ok(())
}

fn collect_relative_paths(
    root: &Path,
    current: &Path,
    entries: &mut Vec<PathBuf>,
) -> Result<(), AppError> {
    for entry in fs::read_dir(current).map_err(|source| AppError::FilesystemOperation {
        action: "read source directory for hashing",
        path: current.to_path_buf(),
        source,
    })? {
        let entry = entry.map_err(|source| AppError::FilesystemOperation {
            action: "read source directory entry for hashing",
            path: current.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        let relative = path
            .strip_prefix(root)
            .map_err(|_| AppError::SourceValidation {
                input: root.display().to_string(),
                message: format!(
                    "path '{}' escaped the hashed source root '{}'",
                    path.display(),
                    root.display()
                ),
            })?
            .to_path_buf();
        entries.push(relative.clone());

        let metadata =
            fs::symlink_metadata(&path).map_err(|source| AppError::FilesystemOperation {
                action: "inspect source entry for hashing",
                path: path.clone(),
                source,
            })?;
        if metadata.is_dir() {
            collect_relative_paths(root, &path, entries)?;
        }
    }

    Ok(())
}

fn relative_path_string(root: &Path, path: &Path, raw_source: &str) -> Result<String, AppError> {
    let relative = path.strip_prefix(root).map_err(|_| {
        source_validation(
            raw_source,
            format!(
                "detected skill path '{}' escaped the staged source root '{}'",
                path.display(),
                root.display()
            ),
        )
    })?;

    relative_path_for_hash(relative).map_err(|message| source_validation(raw_source, message))
}

fn relative_path_for_hash(path: &Path) -> Result<String, String> {
    let mut normalized = Vec::new();
    for component in path.components() {
        match component {
            std::path::Component::Normal(segment) => {
                let segment = segment
                    .to_str()
                    .ok_or_else(|| format!("path '{}' is not valid UTF-8", path.display()))?;
                normalized.push(segment);
            }
            _ => {
                return Err(format!(
                    "path '{}' must remain relative and portable",
                    path.display()
                ));
            }
        }
    }

    if normalized.is_empty() {
        return Ok(String::new());
    }

    Ok(normalized.join("/"))
}

fn run_external_command(
    program: &str,
    args: &[&OsStr],
    current_dir: Option<&Path>,
    raw_source: Option<&str>,
    failure_prefix: &str,
) -> Result<String, AppError> {
    let mut command = Command::new(program);
    command.args(args);
    if let Some(current_dir) = current_dir {
        command.current_dir(current_dir);
    }

    let output = command
        .output()
        .map_err(|source| AppError::ExternalCommand {
            command: program.to_string(),
            source,
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let message = if stderr.is_empty() {
            format!("{failure_prefix} with status {}", output.status)
        } else {
            format!("{failure_prefix}: {stderr}")
        };

        return Err(match raw_source {
            Some(raw_source) => source_validation(raw_source, message),
            None => source_validation(program, message),
        });
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn source_validation(source: impl Into<String>, message: impl Into<String>) -> AppError {
    AppError::SourceValidation {
        input: source.into(),
        message: message.into(),
    }
}

/// Test-only archive builders, including hostile archives the archive libraries
/// refuse to write themselves.
///
/// Path traversal, absolute names, and link entries are exactly what the safety
/// tests need, so the tar builder writes raw header blocks by hand instead of
/// going through `tar::Builder`, which validates paths. Zip archives are built
/// with `zip::ZipWriter` and then edited in place when a hostile name is needed.
#[cfg(test)]
pub(crate) mod test_archives {
    use std::io::Write;

    use super::*;

    /// One hand-built tar entry.
    #[derive(Clone)]
    pub(crate) struct RawTarEntry {
        /// Entry name exactly as stored.
        pub name: String,
        /// Type flag: `b'0'` file, `b'5'` directory, `b'1'` hard link,
        /// `b'2'` symbolic link.
        pub kind: u8,
        /// Permission bits as stored.
        pub mode: u32,
        /// Link target, for link entries.
        pub link_name: Option<String>,
        /// Entry content.
        pub data: Vec<u8>,
    }

    impl RawTarEntry {
        /// A regular file entry with ordinary permissions.
        pub fn file(name: &str, data: &[u8]) -> Self {
            Self {
                name: name.to_string(),
                kind: b'0',
                mode: 0o644,
                link_name: None,
                data: data.to_vec(),
            }
        }

        /// A regular file entry that carries executable permission bits.
        pub fn executable_file(name: &str, data: &[u8]) -> Self {
            Self {
                mode: 0o755,
                ..Self::file(name, data)
            }
        }

        /// A directory entry.
        pub fn directory(name: &str) -> Self {
            Self {
                name: name.to_string(),
                kind: b'5',
                mode: 0o755,
                link_name: None,
                data: Vec::new(),
            }
        }

        /// A symbolic link entry pointing at `target`.
        pub fn symlink(name: &str, target: &str) -> Self {
            Self {
                name: name.to_string(),
                kind: b'2',
                mode: 0o777,
                link_name: Some(target.to_string()),
                data: Vec::new(),
            }
        }

        /// A hard link entry pointing at `target`.
        pub fn hard_link(name: &str, target: &str) -> Self {
            Self {
                name: name.to_string(),
                kind: b'1',
                mode: 0o644,
                link_name: Some(target.to_string()),
                data: Vec::new(),
            }
        }
    }

    /// Build uncompressed tar bytes for `entries`.
    pub fn tar_bytes(entries: &[RawTarEntry]) -> Vec<u8> {
        let mut archive = Vec::new();
        for entry in entries {
            archive.extend_from_slice(&tar_header(entry));
            archive.extend_from_slice(&entry.data);
            let padding = (512 - entry.data.len() % 512) % 512;
            archive.extend(std::iter::repeat_n(0, padding));
        }
        // Two zero blocks end every tar archive.
        archive.extend(std::iter::repeat_n(0, 1024));
        archive
    }

    /// Build gzip-compressed tar bytes for `entries`.
    pub fn tar_gzip_bytes(entries: &[RawTarEntry]) -> Vec<u8> {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder
            .write_all(&tar_bytes(entries))
            .expect("test tar archives compress");
        encoder.finish().expect("test gzip stream finishes")
    }

    /// One 512-byte tar header block for `entry`.
    fn tar_header(entry: &RawTarEntry) -> [u8; 512] {
        let mut header = [0_u8; 512];
        copy_name(&mut header[..100], &entry.name);
        copy_octal(&mut header[100..108], entry.mode as u64);
        copy_octal(&mut header[108..116], 0);
        copy_octal(&mut header[116..124], 0);
        copy_octal(&mut header[124..136], entry.data.len() as u64);
        copy_octal(&mut header[136..148], 0);
        header[156] = entry.kind;
        if let Some(link_name) = &entry.link_name {
            copy_name(&mut header[157..257], link_name);
        }
        header[257..262].copy_from_slice(b"ustar");
        header[263..265].copy_from_slice(b"00");

        for byte in &mut header[148..156] {
            *byte = b' ';
        }
        let checksum: u32 = header.iter().map(|byte| u32::from(*byte)).sum();
        let checksum = format!("{checksum:06o}\0 ");
        header[148..156].copy_from_slice(checksum.as_bytes());
        header
    }

    /// Write `name` into a fixed-width tar field.
    fn copy_name(field: &mut [u8], name: &str) {
        let bytes = name.as_bytes();
        assert!(
            bytes.len() < field.len(),
            "test entry name '{name}' does not fit its tar field",
        );
        field[..bytes.len()].copy_from_slice(bytes);
    }

    /// Write `value` as a NUL-terminated octal number into a tar field.
    fn copy_octal(field: &mut [u8], value: u64) {
        let last = field.len() - 1;
        let digits = format!("{value:0width$o}", width = last);
        field[..last].copy_from_slice(digits.as_bytes());
        field[last] = 0;
    }

    /// Build deflate-compressed zip bytes for `entries`.
    pub fn zip_bytes(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(io::Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for (name, contents) in entries {
            writer
                .start_file(*name, options)
                .unwrap_or_else(|error| panic!("test zip entry '{name}' starts: {error}"));
            writer
                .write_all(contents)
                .unwrap_or_else(|error| panic!("test zip entry '{name}' writes: {error}"));
        }
        writer
            .finish()
            .expect("test zip archive finishes")
            .into_inner()
    }

    /// Build zip bytes holding one symbolic link entry named `name`.
    pub fn zip_symlink_bytes(name: &str, target: &str) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(io::Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        writer
            .add_symlink(name, target, options)
            .expect("test zip symlink entry is added");
        writer
            .finish()
            .expect("test zip archive finishes")
            .into_inner()
    }

    /// Rewrite every occurrence of the stored name `from` with `to`.
    ///
    /// `zip::ZipWriter` normalizes hostile names away, so a traversal entry is
    /// built by rewriting an equally long benign name after the archive is
    /// written. Both names must have the same length so the archive stays valid.
    pub fn rename_zip_entry(bytes: &mut [u8], from: &[u8], to: &[u8]) {
        assert_eq!(
            from.len(),
            to.len(),
            "a rewritten zip name must keep the archive's field widths",
        );
        let mut searched_from = 0;
        while let Some(offset) = bytes[searched_from..]
            .windows(from.len())
            .position(|window| window == from)
        {
            let position = searched_from + offset;
            bytes[position..position + from.len()].copy_from_slice(to);
            searched_from = position + from.len();
        }
    }

    /// SHA-256 over `bytes` as 64 lowercase hexadecimal characters.
    pub fn sha256_hex(bytes: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        format!("{:x}", hasher.finalize())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command as ProcessCommand;
    use test_archives::RawTarEntry;

    const RELEASE_NOTES_SKILL: &str = concat!(
        "---\n",
        "name: release-notes\n",
        "description: Summarize release notes.\n",
        "---\n",
        "\n",
        "# Release Notes\n",
    );
    const BUG_TRIAGE_SKILL: &str = concat!(
        "---\n",
        "name: bug-triage\n",
        "description: Triage incoming bugs.\n",
        "---\n",
        "\n",
        "# Bug Triage\n",
    );
    const AI_SDK_SKILL: &str = concat!(
        "---\n",
        "name: ai-sdk\n",
        "description: Work with the AI SDK.\n",
        "---\n",
        "\n",
        "# AI SDK\n",
    );
    const ONLY_SKILL: &str = concat!(
        "---\n",
        "name: only-skill\n",
        "description: Do one thing.\n",
        "---\n",
        "\n",
        "# Only Skill\n",
    );

    #[test]
    fn local_directory_sources_detect_candidates_in_supported_roots() {
        let fixture = TestSourceFixture::new();
        fixture.write_skill(".agents/skills/release-notes", RELEASE_NOTES_SKILL);
        fixture.write_skill(".claude/skills/bug-triage", BUG_TRIAGE_SKILL);
        fixture.write_skill("skills/ai-sdk", AI_SDK_SKILL);

        let inspection =
            inspect_install_source(fixture.path(), &InstallRequest::new(".".to_string()))
                .expect("local path source inspects successfully");

        assert_eq!(inspection.source.kind, SourceKind::LocalPath);
        assert!(inspection.source.url.starts_with("file://"));
        assert!(inspection.revision.resolved.starts_with("sha256:"));
        assert_eq!(
            inspection
                .candidates
                .iter()
                .map(|candidate| (
                    candidate.name.clone(),
                    candidate.source_path.clone(),
                    candidate.compatible_targets.clone()
                ))
                .collect::<Vec<_>>(),
            vec![
                (
                    "release-notes".to_string(),
                    ".agents/skills/release-notes".to_string(),
                    vec![
                        TargetRuntime::Codex,
                        TargetRuntime::GithubCopilot,
                        TargetRuntime::GeminiCli,
                        TargetRuntime::Amp,
                        TargetRuntime::Opencode,
                    ],
                ),
                (
                    "bug-triage".to_string(),
                    ".claude/skills/bug-triage".to_string(),
                    vec![
                        TargetRuntime::ClaudeCode,
                        TargetRuntime::GithubCopilot,
                        TargetRuntime::Amp,
                        TargetRuntime::Opencode,
                    ],
                ),
                (
                    "ai-sdk".to_string(),
                    "skills/ai-sdk".to_string(),
                    Vec::new(),
                ),
            ]
        );
    }

    #[test]
    fn local_directory_sources_capture_declared_capabilities() {
        let fixture = TestSourceFixture::new();
        fixture.write_skill(
            ".agents/skills/release-notes",
            concat!(
                "---\n",
                "name: release-notes\n",
                "description: Summarize release notes.\n",
                "capabilities:\n",
                "  - network\n",
                "  - shell\n",
                "---\n",
                "\n",
                "# Release Notes\n",
            ),
        );

        let inspection =
            inspect_install_source(fixture.path(), &InstallRequest::new(".".to_string()))
                .expect("local path source inspects successfully");

        assert_eq!(
            inspection.candidates[0].safety.capabilities,
            vec!["network".to_string(), "shell".to_string()]
        );
    }

    #[test]
    fn local_directory_sources_capture_declared_credentials() {
        let fixture = TestSourceFixture::new();
        fixture.write_skill(
            ".agents/skills/release-notes",
            concat!(
                "---\n",
                "name: release-notes\n",
                "description: Summarize release notes.\n",
                "credentials:\n",
                "  - name: OPENAI_API_KEY\n",
                "    purpose: Access the OpenAI API\n",
                "  - name: GITHUB_TOKEN\n",
                "    optional: true\n",
                "---\n",
                "\n",
                "# Release Notes\n",
            ),
        );

        let inspection =
            inspect_install_source(fixture.path(), &InstallRequest::new(".".to_string()))
                .expect("local path source inspects successfully");

        assert_eq!(
            inspection.candidates[0].safety.credentials,
            vec![
                crate::skill::SkillCredentialRequirement {
                    name: "OPENAI_API_KEY".to_string(),
                    optional: false,
                    purpose: Some("Access the OpenAI API".to_string()),
                },
                crate::skill::SkillCredentialRequirement {
                    name: "GITHUB_TOKEN".to_string(),
                    optional: true,
                    purpose: None,
                },
            ]
        );
    }

    #[test]
    fn local_directory_sources_capture_declared_tool_dependencies() {
        let fixture = TestSourceFixture::new();
        fixture.write_skill(
            ".agents/skills/release-notes",
            concat!(
                "---\n",
                "name: release-notes\n",
                "description: Summarize release notes.\n",
                "dependencies:\n",
                "  tools:\n",
                "    - ffmpeg\n",
                "    - name: gh\n",
                "      optional: true\n",
                "---\n",
                "\n",
                "# Release Notes\n",
            ),
        );

        let inspection =
            inspect_install_source(fixture.path(), &InstallRequest::new(".".to_string()))
                .expect("local path source inspects successfully");

        assert_eq!(
            inspection.candidates[0].safety.dependencies.tools,
            vec![
                crate::skill::SkillToolRequirement {
                    name: "ffmpeg".to_string(),
                    optional: false,
                    purpose: None,
                },
                crate::skill::SkillToolRequirement {
                    name: "gh".to_string(),
                    optional: true,
                    purpose: None,
                },
            ]
        );
    }

    #[test]
    fn git_sources_clone_and_capture_the_exact_commit() {
        let fixture = TestSourceFixture::new();
        let (source, expected_commit) = commit_git_source(&fixture, "repo");

        let inspection = inspect_install_source(fixture.path(), &InstallRequest::new(source))
            .expect("git source inspects successfully");

        assert_eq!(inspection.source.kind, SourceKind::Git);
        assert_eq!(inspection.revision.resolved, expected_commit);
        assert_eq!(inspection.revision.upstream, Some(expected_commit));
        assert_eq!(
            inspection.candidates,
            vec![InstallCandidate {
                name: "ai-sdk".to_string(),
                display_name: "ai-sdk".to_string(),
                source_path: ".opencode/skills/ai-sdk".to_string(),
                selected_subpath: ".opencode/skills/ai-sdk".to_string(),
                compatible_targets: vec![TargetRuntime::Opencode],
                compatibility_hints: vec![
                    "opencode-native workspace skill root".to_string(),
                    "documented for opencode".to_string(),
                ],
                safety: crate::skill::SkillSafetySummary::default(),
                trust: SkillTrust::imported_unreviewed("ai-sdk", false),
            }]
        );
    }

    #[test]
    fn locked_git_revisions_served_by_the_source_are_acquired_exactly() {
        let fixture = TestSourceFixture::new();
        let (url, head) = commit_git_source(&fixture, "repo");

        let staged = acquire_locked_source(&locked_git_import(&url, &head))
            .expect("a served revision is acquired");

        assert_eq!(staged.resolved_revision, head);
        assert!(
            staged
                .root
                .join(".opencode/skills/ai-sdk/SKILL.md")
                .is_file(),
            "the staged tree should carry the locked skill content"
        );
    }

    #[test]
    fn locked_git_revisions_the_source_does_not_serve_report_a_dedicated_error() {
        let fixture = TestSourceFixture::new();
        let (url, head) = commit_git_source(&fixture, "repo");

        // One fabricated revision that is a valid object name but is not
        // served, and one hand-edited revision that is not an object name.
        let fabricated = "0123456789abcdef0123456789abcdef01234567".to_string();
        let mistyped = format!("{}g", &head[..head.len() - 1]);

        for unavailable in [fabricated, mistyped] {
            let error = acquire_locked_source(&locked_git_import(&url, &unavailable))
                .expect_err("an unserved revision must fail acquisition");
            let message = error.to_string();
            assert!(
                message.contains(&format!(
                    "locked revision {unavailable} is unavailable from {url}"
                )),
                "expected the unavailable-revision failure, found: {message}"
            );
            assert!(
                message.contains("the source was contacted but does not serve this revision"),
                "expected the source-versus-transport distinction, found: {message}"
            );
            assert!(
                !message.contains("git checkout") && !message.contains("--detach"),
                "git's checkout usage error must not surface: {message}"
            );
        }
    }

    #[test]
    fn unreachable_git_sources_stay_transport_failures() {
        let fixture = TestSourceFixture::new();
        let missing_url = Url::from_file_path(fixture.path().join("no-such-repo"))
            .expect("repo path converts to file url")
            .to_string();

        let error = acquire_locked_source(&locked_git_import(
            &missing_url,
            "0123456789abcdef0123456789abcdef01234567",
        ))
        .expect_err("an unreachable source must fail acquisition");

        let message = error.to_string();
        assert!(
            message.contains("git fetch failed"),
            "expected a transport failure, found: {message}"
        );
        assert!(
            !message.contains("does not serve this revision"),
            "an unreachable source must not be reported as an unserved revision: {message}"
        );
    }

    #[test]
    fn archive_sources_extract_and_detect_skills() {
        let fixture = TestSourceFixture::new();
        let repo = fixture.path().join("archive-src");
        fs::create_dir_all(&repo).expect("archive source exists");
        fixture.write_skill_at(&repo, ".agents/skills/release-notes", RELEASE_NOTES_SKILL);

        let archive_path = fixture.path().join("skills.tar.gz");
        run_test_command(
            ProcessCommand::new("tar")
                .arg("-czf")
                .arg(&archive_path)
                .arg("-C")
                .arg(fixture.path())
                .arg("archive-src"),
        );

        let inspection = inspect_install_source(
            fixture.path(),
            &InstallRequest::new(archive_path.display().to_string()),
        )
        .expect("archive source inspects successfully");

        assert_eq!(inspection.source.kind, SourceKind::Archive);
        assert!(inspection.revision.resolved.starts_with("sha256:"));
        assert_eq!(
            inspection
                .candidates
                .iter()
                .map(|candidate| candidate.source_path.clone())
                .collect::<Vec<_>>(),
            vec![".agents/skills/release-notes".to_string()]
        );
    }

    #[test]
    fn archive_sources_detect_skills_in_every_documented_root() {
        for detection_root in DETECTION_ROOTS {
            let fixture = TestSourceFixture::new();
            let tree = fixture.path().join("tree");
            fs::create_dir_all(&tree).expect("source tree exists");
            fixture.write_skill_at(
                &tree,
                &format!("{}/release-notes", detection_root.relative_path),
                RELEASE_NOTES_SKILL,
            );

            let archive_path = write_test_archive(&fixture, &tree, detection_root.relative_path);

            let archive_inspection = inspect_install_source(
                fixture.path(),
                &InstallRequest::new(archive_path.display().to_string()),
            )
            .unwrap_or_else(|error| {
                panic!(
                    "archive with a top-level '{}' layout should inspect: {error}",
                    detection_root.relative_path
                )
            });
            let directory_inspection = inspect_install_source(
                fixture.path(),
                &InstallRequest::new(tree.display().to_string()),
            )
            .expect("directory source inspects successfully");

            assert_eq!(
                archive_inspection
                    .candidates
                    .iter()
                    .map(|candidate| candidate.source_path.clone())
                    .collect::<Vec<_>>(),
                directory_inspection
                    .candidates
                    .iter()
                    .map(|candidate| candidate.source_path.clone())
                    .collect::<Vec<_>>(),
                "archive and directory sources must detect the same candidates under '{}'",
                detection_root.relative_path,
            );
        }
    }

    #[test]
    fn archive_sources_detect_a_bare_top_level_skill_directory() {
        let fixture = TestSourceFixture::new();
        let tree = fixture.path().join("tree");
        fs::create_dir_all(&tree).expect("source tree exists");
        fixture.write_skill_at(&tree, "only-skill", ONLY_SKILL);

        let archive_path = write_test_archive(&fixture, &tree, "only-skill");

        let archive_inspection = inspect_install_source(
            fixture.path(),
            &InstallRequest::new(archive_path.display().to_string()),
        )
        .expect("bare skill archive inspects successfully");
        let directory_inspection = inspect_install_source(
            fixture.path(),
            &InstallRequest::new(tree.join("only-skill").display().to_string()),
        )
        .expect("direct skill directory inspects successfully");

        assert_eq!(
            inspection_candidates(&archive_inspection),
            vec![("only-skill".to_string(), "skills/only-skill".to_string())]
        );
        assert_eq!(
            inspection_candidates(&archive_inspection),
            inspection_candidates(&directory_inspection),
            "a bare skill archive must install like the unpacked skill directory",
        );
    }

    #[test]
    fn archive_sources_detect_a_bare_top_level_skill_manifest() {
        let fixture = TestSourceFixture::new();
        fixture.write_skill("only-skill", ONLY_SKILL);
        let skill_root = fixture.path().join("only-skill");

        let archive_path = fixture.path().join("only-skill.tgz");
        run_test_command(
            ProcessCommand::new("tar")
                .arg("-czf")
                .arg(&archive_path)
                .arg("-C")
                .arg(&skill_root)
                .arg(SKILL_MANIFEST_FILE),
        );

        let archive_inspection = inspect_install_source(
            fixture.path(),
            &InstallRequest::new(archive_path.display().to_string()),
        )
        .expect("bare manifest archive inspects successfully");
        let directory_inspection = inspect_install_source(
            fixture.path(),
            &InstallRequest::new(skill_root.display().to_string()),
        )
        .expect("direct skill directory inspects successfully");

        assert_eq!(
            inspection_candidates(&archive_inspection),
            vec![("only-skill".to_string(), "skills/only-skill".to_string())]
        );
        assert_eq!(
            inspection_candidates(&archive_inspection),
            inspection_candidates(&directory_inspection),
            "an archive named after one skill must install like the unpacked skill directory",
        );
    }

    #[test]
    fn archive_extraction_directories_carry_the_archive_name() {
        for (archive_name, expected) in [
            ("packed.tar.gz", "packed"),
            ("packed.tgz", "packed"),
            ("packed.tar.bz2", "packed"),
            ("PACKED.ZIP", "PACKED"),
            ("skills.zip", "skills"),
            ("hidden.zip", "hidden"),
            (".hidden.zip", ARCHIVE_FALLBACK_DIR_NAME),
            ("con.tar.gz", ARCHIVE_FALLBACK_DIR_NAME),
            ("trailing..tar.gz", ARCHIVE_FALLBACK_DIR_NAME),
            ("name with space.zip", ARCHIVE_FALLBACK_DIR_NAME),
        ] {
            assert_eq!(
                archive_content_dir_name(Path::new(archive_name)),
                expected,
                "unexpected extraction directory for '{archive_name}'",
            );
        }
    }

    #[test]
    fn archive_sources_unwrap_a_release_wrapper_around_documented_roots() {
        let fixture = TestSourceFixture::new();
        let tree = fixture.path().join("tree");
        fs::create_dir_all(&tree).expect("source tree exists");
        fixture.write_skill_at(&tree, "release-wrapper/skills/ai-sdk", AI_SDK_SKILL);

        let archive_path = write_test_archive(&fixture, &tree, "release-wrapper");

        let archive_inspection = inspect_install_source(
            fixture.path(),
            &InstallRequest::new(archive_path.display().to_string()),
        )
        .expect("wrapped archive inspects successfully");

        assert_eq!(
            inspection_candidates(&archive_inspection),
            vec![("ai-sdk".to_string(), "skills/ai-sdk".to_string())]
        );
    }

    #[test]
    fn archive_sources_without_a_supported_layout_name_the_documented_roots() {
        let fixture = TestSourceFixture::new();
        let tree = fixture.path().join("tree");
        fs::create_dir_all(tree.join("docs")).expect("archive content exists");

        let archive_path = write_test_archive(&fixture, &tree, "docs");

        let error = inspect_install_source(
            fixture.path(),
            &InstallRequest::new(archive_path.display().to_string()),
        )
        .expect_err("archive without skills should fail");

        assert!(
            error
                .to_string()
                .contains("no supported skills were found under .agents/skills, .claude/skills, .opencode/skills, skills"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn direct_skill_directory_sources_are_repackaged_as_single_skill_sources() {
        let fixture = TestSourceFixture::new();
        fixture.write_skill("release-notes", RELEASE_NOTES_SKILL);

        let inspection = inspect_install_source(
            fixture.path(),
            &InstallRequest::new(fixture.path().join("release-notes").display().to_string()),
        )
        .expect("direct skill directory inspects successfully");

        assert_eq!(inspection.source.kind, SourceKind::LocalPath);
        assert!(inspection.source.url.starts_with("file://"));
        assert!(inspection.revision.resolved.starts_with("sha256:"));
        assert_eq!(
            inspection.candidates,
            vec![InstallCandidate {
                name: "release-notes".to_string(),
                display_name: "release-notes".to_string(),
                source_path: "skills/release-notes".to_string(),
                selected_subpath: "skills/release-notes".to_string(),
                compatible_targets: Vec::new(),
                compatibility_hints: vec!["repo-root source packaging layout".to_string()],
                safety: SkillSafetySummary::default(),
                trust: SkillTrust::imported_unreviewed("release-notes", false),
            }]
        );
    }

    /// Origin an index fixture is served at.
    const DISCOVERY_ORIGIN: &str = "https://skills.example.com";
    /// Path of the index document the fixtures serve.
    const DISCOVERY_INDEX_PATH: &str = "/.well-known/agent-skills/index.json";
    /// Schema URI every fixture index declares.
    const DISCOVERY_SCHEMA: &str = "https://schemas.agentskills.io/discovery/0.2.0/schema.json";

    /// Transport that serves a discovery index and its artifacts by URL path,
    /// and records every path it was asked for.
    struct DiscoveryFixture {
        responses: Vec<(String, Result<Vec<u8>, String>)>,
        requested: std::cell::RefCell<Vec<String>>,
    }

    impl DiscoveryFixture {
        /// Serve `artifacts` plus an index that declares them in order.
        ///
        /// Each artifact is declared as a `skill-md` entry named after its
        /// `SKILL.md` front matter, with the digest of the bytes served.
        fn index_and_artifacts(artifacts: &[(&str, &str, &[u8])]) -> Self {
            let entries = artifacts
                .iter()
                .map(|(name, path, bytes)| {
                    format!(
                        "{{\"name\":\"{name}\",\"type\":\"skill-md\",\
                         \"description\":\"Does one thing well.\",\"url\":\"{path}\",\
                         \"digest\":\"sha256:{}\"}}",
                        test_archives::sha256_hex(bytes)
                    )
                })
                .collect::<Vec<_>>()
                .join(",");
            let index = format!("{{\"$schema\":\"{DISCOVERY_SCHEMA}\",\"skills\":[{entries}]}}");

            let mut responses = vec![(DISCOVERY_INDEX_PATH.to_string(), Ok(index.into_bytes()))];
            for (_, path, bytes) in artifacts {
                responses.push(((*path).to_string(), Ok(bytes.to_vec())));
            }

            Self {
                responses,
                requested: std::cell::RefCell::new(Vec::new()),
            }
        }

        fn lookup(&self, url: &Url) -> Result<Vec<u8>, AppError> {
            self.requested.borrow_mut().push(url.path().to_string());
            for (path, body) in &self.responses {
                if url.path() == *path {
                    return match body {
                        Ok(bytes) => Ok(bytes.clone()),
                        Err(message) => Err(AppError::DiscoveryFetch {
                            url: url.to_string(),
                            message: message.clone(),
                        }),
                    };
                }
            }

            Err(AppError::DiscoveryFetch {
                url: url.to_string(),
                message: "origin responded with HTTP 404".to_string(),
            })
        }

        /// Paths this transport was asked for, in request order.
        fn requested_paths(&self) -> Vec<String> {
            self.requested.borrow().clone()
        }
    }

    impl DiscoveryTransport for DiscoveryFixture {
        fn fetch(&self, url: &Url) -> Result<Vec<u8>, AppError> {
            self.lookup(url)
        }

        fn fetch_artifact(&self, url: &Url) -> Result<Vec<u8>, AppError> {
            self.lookup(url)
        }
    }

    /// One non-interactive JSON install context, optionally selecting a skill.
    fn install_context(skill_name: Option<&str>) -> AppContext {
        AppContext {
            working_directory: std::env::temp_dir(),
            output_mode: OutputMode::Json,
            verbosity: crate::app::Verbosity::Normal,
            interaction_mode: InteractionMode::NonInteractive,
            selector: crate::app::CommandSelector {
                skill_name: skill_name.map(str::to_string),
                scope: None,
                targets: Vec::new(),
            },
        }
    }

    #[test]
    fn only_discovery_shaped_http_urls_resolve_as_well_known_sources() {
        let fixture = TestSourceFixture::new();

        let origin = resolve_install_source(
            &InstallSource {
                raw: "https://skills.example.com".to_string(),
            },
            fixture.path(),
        )
        .expect("an origin resolves");
        assert_eq!(origin.normalized.kind, SourceKind::WellKnown);
        assert_eq!(
            origin.normalized.url,
            "https://skills.example.com/.well-known/agent-skills/index.json"
        );

        let index = resolve_install_source(
            &InstallSource {
                raw: "https://cdn.example.com/skills/index.json".to_string(),
            },
            fixture.path(),
        )
        .expect("an index URL resolves");
        assert_eq!(index.normalized.kind, SourceKind::WellKnown);
        assert_eq!(
            index.normalized.url,
            "https://cdn.example.com/skills/index.json"
        );

        for git_source in [
            "https://github.com/acme/skills.git",
            "https://github.com/acme/skills",
            "git@github.com:acme/skills.git",
        ] {
            let resolved = resolve_install_source(
                &InstallSource {
                    raw: git_source.to_string(),
                },
                fixture.path(),
            )
            .expect("a repository URL resolves");
            assert_eq!(
                resolved.normalized.kind,
                SourceKind::Git,
                "{git_source} should stay a git source"
            );
        }

        // A bare host and a relative path keep their existing diagnosis.
        for rejected in ["skills.example.com", "./missing-source"] {
            let error = resolve_install_source(
                &InstallSource {
                    raw: rejected.to_string(),
                },
                fixture.path(),
            )
            .expect_err("the source is refused");
            assert!(
                error.to_string().contains("must be a Git URL"),
                "unexpected error for {rejected}: {error}"
            );
        }
    }

    #[test]
    fn a_well_known_install_selects_one_entry_by_exact_name_and_stages_it() {
        let transport = DiscoveryFixture::index_and_artifacts(&[
            (
                "release-notes",
                "/.well-known/agent-skills/release-notes/SKILL.md",
                RELEASE_NOTES_SKILL.as_bytes(),
            ),
            (
                "bug-triage",
                "/.well-known/agent-skills/bug-triage/SKILL.md",
                BUG_TRIAGE_SKILL.as_bytes(),
            ),
        ]);

        let prepared = prepare_install_source_with(
            &install_context(Some("bug-triage")),
            &InstallRequest::new(DISCOVERY_ORIGIN.to_string()),
            &transport,
        )
        .expect("the selected entry stages");

        assert_eq!(prepared.source.kind, SourceKind::WellKnown);
        assert_eq!(
            prepared.source.url,
            format!("{DISCOVERY_ORIGIN}{DISCOVERY_INDEX_PATH}")
        );
        assert_eq!(prepared.selected_skill_name.as_deref(), Some("bug-triage"));
        assert_eq!(prepared.revision.upstream, None);
        assert_eq!(
            prepared.revision.resolved,
            format!(
                "sha256:{}",
                test_archives::sha256_hex(BUG_TRIAGE_SKILL.as_bytes())
            ),
            "the verified artifact digest is the pinned revision"
        );

        let discovery = prepared.discovery.as_ref().expect("the entry is recorded");
        assert_eq!(discovery.skill, "bug-triage");
        assert_eq!(
            discovery.artifact_type,
            crate::discovery::ArtifactType::SkillMd
        );
        assert_eq!(
            discovery.url,
            "https://skills.example.com/.well-known/agent-skills/bug-triage/SKILL.md"
        );
        assert_eq!(discovery.digest, prepared.revision.resolved);

        assert_eq!(
            prepared
                .candidates
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>(),
            vec!["bug-triage"],
            "only the selected entry is downloaded and detected"
        );
        assert_eq!(
            transport.requested_paths(),
            vec![
                DISCOVERY_INDEX_PATH.to_string(),
                "/.well-known/agent-skills/bug-triage/SKILL.md".to_string(),
            ],
            "the unselected entry must not be fetched"
        );
    }

    #[test]
    fn a_well_known_install_without_a_selector_needs_input() {
        let transport = DiscoveryFixture::index_and_artifacts(&[(
            "release-notes",
            "/.well-known/agent-skills/release-notes/SKILL.md",
            RELEASE_NOTES_SKILL.as_bytes(),
        )]);

        let error = prepare_install_source_with(
            &install_context(None),
            &InstallRequest::new(DISCOVERY_ORIGIN.to_string()),
            &transport,
        )
        .expect_err("a non-interactive install never guesses");

        assert!(matches!(error, AppError::InputRequired { .. }), "{error:?}");
    }

    #[test]
    fn a_well_known_install_names_the_declared_entries_when_nothing_matches() {
        let transport = DiscoveryFixture::index_and_artifacts(&[
            (
                "release-notes",
                "/.well-known/agent-skills/release-notes/SKILL.md",
                RELEASE_NOTES_SKILL.as_bytes(),
            ),
            (
                "bug-triage",
                "/.well-known/agent-skills/bug-triage/SKILL.md",
                BUG_TRIAGE_SKILL.as_bytes(),
            ),
        ]);

        let error = prepare_install_source_with(
            &install_context(Some("ai-sdk")),
            &InstallRequest::new(DISCOVERY_ORIGIN.to_string()),
            &transport,
        )
        .expect_err("an unknown name is refused");

        let message = error.to_string();
        assert!(
            message.contains("declares no skill named 'ai-sdk'"),
            "unexpected error: {message}"
        );
        assert!(
            message.contains("release-notes, bug-triage"),
            "the declared names should be listed: {message}"
        );
        assert_eq!(
            transport.requested_paths(),
            vec![DISCOVERY_INDEX_PATH.to_string()],
            "no artifact is fetched for a name the index does not declare"
        );
    }

    #[test]
    fn locked_well_known_imports_are_reacquired_by_their_recorded_digest() {
        let transport = DiscoveryFixture::index_and_artifacts(&[(
            "release-notes",
            "/.well-known/agent-skills/release-notes/SKILL.md",
            RELEASE_NOTES_SKILL.as_bytes(),
        )]);
        let digest = format!(
            "sha256:{}",
            test_archives::sha256_hex(RELEASE_NOTES_SKILL.as_bytes())
        );
        let locked = crate::lockfile::LockedImport {
            source: crate::lockfile::LockedSource {
                kind: SourceKind::WellKnown,
                url: format!("{DISCOVERY_ORIGIN}{DISCOVERY_INDEX_PATH}"),
                subpath: crate::lockfile::LockfilePath::new("skills/release-notes"),
            },
            discovery: Some(crate::lockfile::LockedDiscoveryEntry {
                skill: "release-notes".to_string(),
                artifact_type: crate::discovery::ArtifactType::SkillMd,
                url: "https://skills.example.com/.well-known/agent-skills/release-notes/SKILL.md"
                    .to_string(),
                digest: digest.clone(),
            }),
            revision: crate::lockfile::LockedRevision {
                resolved: digest.clone(),
                upstream: None,
            },
            bundle: None,
            hashes: crate::lockfile::LockedHashes {
                content: "sha256:content".to_string(),
                overlay: "sha256:overlay".to_string(),
                effective_version: "sha256:effective".to_string(),
            },
        };

        let staged = acquire_locked_well_known_with_transport(&locked, &transport)
            .expect("the recorded artifact is reacquired");

        assert_eq!(staged.revision.resolved, digest);
        assert!(
            staged.root.join("skills/release-notes/SKILL.md").is_file(),
            "the staged tree should carry the locked skill content"
        );
    }

    #[test]
    fn a_locked_well_known_import_without_its_entry_is_refused() {
        let transport = DiscoveryFixture::index_and_artifacts(&[]);
        let mut locked = locked_git_import("https://example.com/repo.git", "aaaaaaaaaaaaaaaa");
        locked.source.kind = SourceKind::WellKnown;
        locked.source.url = format!("{DISCOVERY_ORIGIN}{DISCOVERY_INDEX_PATH}");

        let error = acquire_locked_well_known_with_transport(&locked, &transport)
            .expect_err("an entry-less import cannot be reacquired");

        assert!(
            error.to_string().contains("discovery index entry"),
            "unexpected error: {error}"
        );
        assert!(transport.requested_paths().is_empty());
    }

    #[test]
    fn missing_sources_fail_validation() {
        let fixture = TestSourceFixture::new();

        let error = inspect_install_source(
            fixture.path(),
            &InstallRequest::new("./missing-source".to_string()),
        )
        .expect_err("missing source should fail");

        assert!(
            error.to_string().contains(
                "source must be a Git URL, an HTTP(S) well-known discovery origin or index URL, \
                 existing local directory, or existing local archive"
            ),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn a_recorded_upstream_revision_is_withheld_for_an_immutable_reference() {
        let revision = "0123456789abcdef0123456789abcdef01234567";
        let recorded_value = "fedcba9876543210fedcba9876543210fedcba98".to_string();
        let recorded = Some(&recorded_value);

        assert_eq!(
            reportable_upstream_revision(recorded, revision, Some("main")),
            recorded,
            "a movable reference still reports what an operation observed",
        );
        assert_eq!(
            reportable_upstream_revision(recorded, revision, None),
            recorded,
            "a read without a reference cannot prove the value was fabricated",
        );
        for reference in [revision, &revision[..7]] {
            assert_eq!(
                reportable_upstream_revision(recorded, revision, Some(reference)),
                None,
                "a lockfile written while moving to '{reference}' reports no observation",
            );
        }
    }

    #[test]
    fn stored_import_roots_are_namespaced_by_scope_and_workspace() {
        let fixture = TestSourceFixture::new();
        let workspace_a = fixture.path().join("workspace-a");
        let workspace_b = fixture.path().join("workspace-b");
        fs::create_dir_all(&workspace_a).expect("workspace a exists");
        fs::create_dir_all(&workspace_b).expect("workspace b exists");

        let workspace_a_root =
            stored_import_root(ManagedScope::Workspace, &workspace_a, "release-notes")
                .expect("workspace a root builds");
        let workspace_b_root =
            stored_import_root(ManagedScope::Workspace, &workspace_b, "release-notes")
                .expect("workspace b root builds");
        let user_a_root = stored_import_root(ManagedScope::User, &workspace_a, "release-notes")
            .expect("user root builds for workspace a");
        let user_b_root = stored_import_root(ManagedScope::User, &workspace_b, "release-notes")
            .expect("user root builds for workspace b");

        assert_ne!(workspace_a_root, workspace_b_root);
        assert_eq!(user_a_root, user_b_root);
        assert!(
            workspace_a_root
                .to_string_lossy()
                .contains(WORKSPACE_IMPORTS_NAMESPACE),
            "workspace root should stay in the workspace namespace: {}",
            workspace_a_root.display()
        );
        assert!(
            user_a_root
                .to_string_lossy()
                .contains(USER_IMPORTS_NAMESPACE),
            "user root should stay in the user namespace: {}",
            user_a_root.display()
        );
    }

    /// Compress one entry of `source_dir` into a fixture archive so tests model
    /// the top-level layout an archive actually carries.
    fn write_test_archive(
        fixture: &TestSourceFixture,
        source_dir: &Path,
        top_level_entry: &str,
    ) -> PathBuf {
        let archive_path = fixture
            .path()
            .join(format!("{}.tar.gz", top_level_entry.replace('/', "_")));
        run_test_command(
            ProcessCommand::new("tar")
                .arg("-czf")
                .arg(&archive_path)
                .arg("-C")
                .arg(source_dir)
                .arg(top_level_entry),
        );
        archive_path
    }

    /// Commit one skill into a fresh Git repository below the fixture and
    /// return its file URL and head commit.
    fn commit_git_source(fixture: &TestSourceFixture, relative: &str) -> (String, String) {
        let repo = fixture.path().join(relative);
        fs::create_dir_all(&repo).expect("repo dir exists");
        fixture.write_skill_at(&repo, ".opencode/skills/ai-sdk", AI_SDK_SKILL);
        run_test_command(ProcessCommand::new("git").arg("init").arg(&repo));
        run_test_command(ProcessCommand::new("git").arg("-C").arg(&repo).args([
            "config",
            "user.name",
            "Test User",
        ]));
        run_test_command(ProcessCommand::new("git").arg("-C").arg(&repo).args([
            "config",
            "user.email",
            "test@example.com",
        ]));
        run_test_command(
            ProcessCommand::new("git")
                .arg("-C")
                .arg(&repo)
                .args(["add", "."]),
        );
        run_test_command(ProcessCommand::new("git").arg("-C").arg(&repo).args([
            "commit",
            "-m",
            "initial import",
        ]));

        let head = run_test_command(
            ProcessCommand::new("git")
                .arg("-C")
                .arg(&repo)
                .args(["rev-parse", "HEAD"]),
        );
        let url = Url::from_file_path(&repo)
            .expect("repo path converts to file url")
            .to_string();

        (url, head)
    }

    /// One git import pinned at `revision`, for acquisition tests.
    fn locked_git_import(url: &str, revision: &str) -> crate::lockfile::LockedImport {
        crate::lockfile::LockedImport {
            source: crate::lockfile::LockedSource {
                kind: SourceKind::Git,
                url: url.to_string(),
                subpath: crate::lockfile::LockfilePath::new(".opencode/skills/ai-sdk"),
            },
            discovery: None,
            revision: crate::lockfile::LockedRevision {
                resolved: revision.to_string(),
                upstream: None,
            },
            bundle: None,
            hashes: crate::lockfile::LockedHashes {
                content: "sha256:content".to_string(),
                overlay: "sha256:overlay".to_string(),
                effective_version: "sha256:effective".to_string(),
            },
        }
    }

    fn inspection_candidates(inspection: &InstallInspection) -> Vec<(String, String)> {
        inspection
            .candidates
            .iter()
            .map(|candidate| (candidate.name.clone(), candidate.source_path.clone()))
            .collect()
    }

    struct TestSourceFixture {
        root: TempDir,
    }

    impl TestSourceFixture {
        fn new() -> Self {
            Self {
                root: TempDir::new().expect("temp dir exists"),
            }
        }

        fn path(&self) -> &Path {
            self.root.path()
        }

        fn write_skill(&self, relative_path: &str, contents: &str) {
            self.write_skill_at(self.path(), relative_path, contents);
        }

        fn write_skill_at(&self, base: &Path, relative_path: &str, contents: &str) {
            let skill_root = base.join(relative_path);
            fs::create_dir_all(&skill_root).expect("skill root exists");
            fs::write(skill_root.join("SKILL.md"), contents).expect("skill manifest exists");
        }
    }

    fn run_test_command(command: &mut ProcessCommand) -> String {
        let output = command.output().expect("command launches");
        assert!(
            output.status.success(),
            "command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    /// A digest-verified artifact for one discovery entry, built from bytes the
    /// test already holds.
    fn fetched_artifact(
        name: &str,
        artifact_type: crate::discovery::ArtifactType,
        url: &str,
        bytes: &[u8],
    ) -> crate::discovery::FetchedArtifact {
        crate::discovery::FetchedArtifact {
            name: name.to_string(),
            artifact_type,
            url: Url::parse(url).expect("artifact url parses"),
            digest: crate::discovery::ArtifactDigest::parse(&format!(
                "sha256:{}",
                test_archives::sha256_hex(bytes)
            ))
            .expect("digest parses"),
            bytes: bytes.to_vec(),
        }
    }

    /// Stage `bytes` as a remote artifact and return the staged root's
    /// candidates, proving the staging matches the install pipeline's detection.
    fn staged_candidates(name: &str, url: &str, bytes: &[u8]) -> Vec<(String, String)> {
        let staged = stage_discovered_artifact(&fetched_artifact(
            name,
            crate::discovery::ArtifactType::Archive,
            url,
            bytes,
        ))
        .expect("archive stages");
        detect_install_candidates(&staged.root, url)
            .expect("staged archive detects candidates")
            .into_iter()
            .map(|candidate| (candidate.name, candidate.source_path))
            .collect()
    }

    #[test]
    fn verified_skill_md_artifacts_stage_a_synthetic_skill_directory() {
        let artifact = fetched_artifact(
            "code-review",
            crate::discovery::ArtifactType::SkillMd,
            "https://example.com/.well-known/agent-skills/code-review/SKILL.md",
            concat!(
                "---\n",
                "name: code-review\n",
                "description: Review code for defects.\n",
                "---\n",
                "\n",
                "# Code Review\n",
            )
            .as_bytes(),
        );

        let staged = stage_discovered_artifact(&artifact).expect("skill-md artifact stages");

        assert_eq!(staged.name, "code-review");
        assert_eq!(
            staged.artifact_type,
            crate::discovery::ArtifactType::SkillMd
        );
        assert_eq!(staged.digest.as_str(), artifact.digest.as_str());
        assert_eq!(
            fs::read_to_string(staged.root.join("skills/code-review/SKILL.md"))
                .expect("staged manifest exists"),
            String::from_utf8_lossy(&artifact.bytes),
        );
        assert_eq!(
            detect_install_candidates(&staged.root, "discovery")
                .expect("staged skill detects candidates")
                .iter()
                .map(|candidate| candidate.source_path.as_str())
                .collect::<Vec<_>>(),
            vec!["skills/code-review"],
            "a staged skill-md artifact must install through the packaging root",
        );
    }

    #[test]
    fn skill_md_staging_refuses_a_manifest_that_names_another_skill() {
        let artifact = fetched_artifact(
            "code-review",
            crate::discovery::ArtifactType::SkillMd,
            "https://example.com/.well-known/agent-skills/code-review/SKILL.md",
            concat!(
                "---\n",
                "name: other-skill\n",
                "description: Names another skill.\n",
                "---\n",
                "\n",
                "# Other Skill\n",
            )
            .as_bytes(),
        );

        let error =
            stage_discovered_artifact(&artifact).expect_err("name mismatch refuses staging");
        let message = error.to_string();
        assert!(
            message.contains("'other-skill'") && message.contains("'code-review'"),
            "unexpected error: {message}"
        );
        assert!(
            matches!(error, AppError::DiscoveryArtifactRejected { .. }),
            "unexpected error: {error:?}"
        );
    }

    #[test]
    fn skill_md_staging_refuses_bytes_that_are_not_a_skill_manifest() {
        let artifact = fetched_artifact(
            "code-review",
            crate::discovery::ArtifactType::SkillMd,
            "https://example.com/.well-known/agent-skills/code-review/SKILL.md",
            b"# just some markdown\n",
        );

        let error = stage_discovered_artifact(&artifact).expect_err("invalid manifest refuses");
        assert!(
            matches!(error, AppError::DiscoveryArtifactRejected { .. }),
            "unexpected error: {error:?}"
        );
    }

    #[test]
    fn remote_tar_archives_stage_into_a_root_detection_reads() {
        let candidates = staged_candidates(
            "release-notes",
            "https://example.com/download/release-notes.tar.gz",
            &test_archives::tar_gzip_bytes(&[
                RawTarEntry::file(
                    ".agents/skills/release-notes/SKILL.md",
                    RELEASE_NOTES_SKILL.as_bytes(),
                ),
                RawTarEntry::file(
                    ".agents/skills/release-notes/references/style.md",
                    b"# Style\n",
                ),
            ]),
        );

        assert_eq!(
            candidates,
            vec![(
                "release-notes".to_string(),
                ".agents/skills/release-notes".to_string()
            )]
        );
    }

    #[test]
    fn remote_tar_archives_are_detected_without_compression_too() {
        let candidates = staged_candidates(
            "release-notes",
            "https://example.com/download/release-notes.tar",
            &test_archives::tar_bytes(&[RawTarEntry::file(
                ".agents/skills/release-notes/SKILL.md",
                RELEASE_NOTES_SKILL.as_bytes(),
            )]),
        );

        assert_eq!(
            candidates,
            vec![(
                "release-notes".to_string(),
                ".agents/skills/release-notes".to_string()
            )]
        );
    }

    #[test]
    fn remote_zip_archives_stage_into_a_root_detection_reads() {
        let candidates = staged_candidates(
            "release-notes",
            "https://example.com/download/release-notes.zip",
            &test_archives::zip_bytes(&[
                (
                    ".agents/skills/release-notes/SKILL.md",
                    RELEASE_NOTES_SKILL.as_bytes(),
                ),
                (
                    ".agents/skills/release-notes/references/style.md",
                    b"# Style\n",
                ),
            ]),
        );

        assert_eq!(
            candidates,
            vec![(
                "release-notes".to_string(),
                ".agents/skills/release-notes".to_string()
            )]
        );
    }

    #[test]
    fn a_bare_remote_skill_archive_uses_the_direct_skill_packaging_path() {
        let staged = stage_discovered_artifact(&fetched_artifact(
            "only-skill",
            crate::discovery::ArtifactType::Archive,
            "https://example.com/download/only-skill.tar.gz",
            &test_archives::tar_gzip_bytes(&[RawTarEntry::file("SKILL.md", ONLY_SKILL.as_bytes())]),
        ))
        .expect("bare skill archive stages");

        assert_eq!(
            detect_install_candidates(&staged.root, "discovery")
                .expect("staged archive detects candidates")
                .iter()
                .map(|candidate| candidate.source_path.as_str())
                .collect::<Vec<_>>(),
            vec!["skills/only-skill"],
            "a bare skill archive must repackage through the direct-skill path",
        );
    }

    #[test]
    fn a_wrapped_remote_archive_is_unwrapped_by_the_normalized_root() {
        let candidates = staged_candidates(
            "ai-sdk",
            "https://example.com/download/packed-1.0.tar.gz",
            &test_archives::tar_gzip_bytes(&[RawTarEntry::file(
                "release-wrapper/skills/ai-sdk/SKILL.md",
                AI_SDK_SKILL.as_bytes(),
            )]),
        );

        assert_eq!(
            candidates,
            vec![("ai-sdk".to_string(), "skills/ai-sdk".to_string())]
        );
    }

    #[test]
    fn a_wrapped_single_skill_archive_installs_like_its_unpacked_directory() {
        // The layout most publishers ship: one directory named after the skill.
        let candidates = staged_candidates(
            "release-notes",
            "https://example.com/download/release-notes.tar.gz",
            &test_archives::tar_gzip_bytes(&[RawTarEntry::file(
                "release-notes/SKILL.md",
                RELEASE_NOTES_SKILL.as_bytes(),
            )]),
        );

        assert_eq!(
            candidates,
            vec![(
                "release-notes".to_string(),
                "skills/release-notes".to_string()
            )],
            "a wrapped single skill archive must repackage through the direct-skill path",
        );
    }

    #[test]
    fn remote_archives_with_traversal_entries_are_rejected_before_anything_is_written() {
        let fixture = TestSourceFixture::new();
        let destination = fixture.path().join("unpack");
        fs::create_dir_all(&destination).expect("destination exists");
        let limits = UnpackLimits::default();

        let traversal_names = [
            "../escaped.txt",
            "skills/../../escaped.txt",
            "./../escaped.txt",
        ];
        for name in traversal_names {
            let bytes = test_archives::tar_gzip_bytes(&[RawTarEntry::file(name, b"payload")]);
            let error = unpack_archive_bytes(
                &bytes,
                &destination,
                &limits,
                "demo",
                "https://example.com/a.tar.gz",
            )
            .expect_err("traversal entry refuses unpacking");
            let message = error.to_string();
            assert!(
                message.contains("escapes the extraction directory"),
                "entry '{name}' produced: {message}"
            );
            assert!(
                matches!(error, AppError::DiscoveryArtifactRejected { .. }),
                "unexpected error: {error:?}"
            );
        }

        let mut zip = test_archives::zip_bytes(&[("skills/escape.txt", b"payload")]);
        test_archives::rename_zip_entry(&mut zip, b"skills/escape.txt", b"../../../evil.txt");
        let error = unpack_archive_bytes(
            &zip,
            &destination,
            &limits,
            "demo",
            "https://example.com/a.zip",
        )
        .expect_err("zip traversal refuses unpacking");
        assert!(
            error
                .to_string()
                .contains("escapes the extraction directory"),
            "unexpected error: {error}"
        );

        assert!(
            !fixture.path().join("escaped.txt").exists()
                && !fixture.path().join("evil.txt").exists(),
            "a rejected archive must not leave files outside its destination",
        );
        assert!(
            fs::read_dir(&destination).unwrap().count() == 0,
            "a rejected archive must not leave files inside its destination",
        );
    }

    #[test]
    fn remote_archives_with_absolute_or_windows_hostile_names_are_rejected() {
        let fixture = TestSourceFixture::new();
        let destination = fixture.path().join("unpack");
        fs::create_dir_all(&destination).expect("destination exists");
        let limits = UnpackLimits::default();

        let hostile = [
            "/etc/skillctl-evil",
            "..\\escaped.txt",
            "con.txt",
            "skills/aux.md",
        ];
        for name in hostile {
            let bytes = test_archives::tar_bytes(&[RawTarEntry::file(name, b"payload")]);
            let error = unpack_archive_bytes(
                &bytes,
                &destination,
                &limits,
                "demo",
                "https://example.com/a.tar",
            )
            .expect_err("hostile entry name refuses unpacking");
            let message = error.to_string();
            assert!(
                message.contains("rejected"),
                "entry '{name}' produced: {message}"
            );
        }
        assert!(
            fs::read_dir(&destination).unwrap().count() == 0,
            "rejected entries must not be written",
        );
    }

    #[test]
    fn remote_archives_with_link_entries_are_rejected() {
        let fixture = TestSourceFixture::new();
        let destination = fixture.path().join("unpack");
        fs::create_dir_all(&destination).expect("destination exists");
        let limits = UnpackLimits::default();
        let skill = RawTarEntry::file(".agents/skills/demo/SKILL.md", ONLY_SKILL.as_bytes());

        let linked = [
            test_archives::tar_bytes(&[RawTarEntry::symlink("skills/link", "/etc/passwd"), skill]),
            test_archives::tar_bytes(&[RawTarEntry::hard_link(
                "skills/link",
                ".agents/skills/demo/SKILL.md",
            )]),
            test_archives::zip_symlink_bytes("skills/link", "/etc/passwd"),
        ];
        for bytes in linked {
            let error = unpack_archive_bytes(
                &bytes,
                &destination,
                &limits,
                "demo",
                "https://example.com/a",
            )
            .expect_err("link entry refuses unpacking");
            let message = error.to_string();
            assert!(
                message.contains("symbolic or hard link"),
                "unexpected error: {message}"
            );
        }

        assert!(
            !destination.join("skills/link").exists(),
            "a link entry must never reach the filesystem",
        );
    }

    #[test]
    fn remote_archives_outside_their_size_budget_are_rejected() {
        let fixture = TestSourceFixture::new();
        let destination = fixture.path().join("unpack");
        fs::create_dir_all(&destination).expect("destination exists");
        let url = "https://example.com/a.tar.gz";

        let oversized_entry =
            test_archives::tar_bytes(&[RawTarEntry::file("skills/big.bin", &[7_u8; 64])]);
        let error = unpack_archive_bytes(
            &oversized_entry,
            &destination,
            &UnpackLimits {
                max_entry_bytes: 32,
                ..UnpackLimits::default()
            },
            "demo",
            url,
        )
        .expect_err("an oversized entry refuses unpacking");
        assert!(
            error.to_string().contains("per-entry limit"),
            "unexpected error: {error}"
        );

        let two_entries = test_archives::tar_bytes(&[
            RawTarEntry::file("skills/one.bin", &[1_u8; 16]),
            RawTarEntry::file("skills/two.bin", &[2_u8; 16]),
        ]);
        let error = unpack_archive_bytes(
            &two_entries,
            &destination,
            &UnpackLimits {
                max_total_bytes: 24,
                ..UnpackLimits::default()
            },
            "demo",
            url,
        )
        .expect_err("a total budget overrun refuses unpacking");
        assert!(
            error.to_string().contains("total limit"),
            "unexpected error: {error}"
        );

        let error = unpack_archive_bytes(
            &two_entries,
            &destination,
            &UnpackLimits {
                max_entries: 1,
                ..UnpackLimits::default()
            },
            "demo",
            url,
        )
        .expect_err("an entry count overrun refuses unpacking");
        assert!(
            error.to_string().contains("entry limit"),
            "unexpected error: {error}"
        );

        assert!(
            fs::read_dir(&destination).unwrap().count() == 0,
            "a rejected archive must not write partial content",
        );
    }

    #[test]
    fn remote_archives_without_content_are_rejected() {
        let empty = test_archives::tar_gzip_bytes(&[]);
        let error = stage_discovered_artifact(&fetched_artifact(
            "demo",
            crate::discovery::ArtifactType::Archive,
            "https://example.com/download/empty.tar.gz",
            &empty,
        ))
        .expect_err("an empty archive refuses staging");
        let message = error.to_string();
        assert!(
            message.contains("did not contain any files"),
            "unexpected error: {message}"
        );

        let metadata_only = test_archives::zip_bytes(&[("__MACOSX/demo/._SKILL.md", b"junk")]);
        let error = stage_discovered_artifact(&fetched_artifact(
            "demo",
            crate::discovery::ArtifactType::Archive,
            "https://example.com/download/demo.zip",
            &metadata_only,
        ))
        .expect_err("a metadata-only archive refuses staging");
        assert!(
            error.to_string().contains("did not contain any files"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn remote_archives_in_an_unsupported_format_are_rejected() {
        let fixture = TestSourceFixture::new();
        let destination = fixture.path().join("unpack");
        fs::create_dir_all(&destination).expect("destination exists");

        for bytes in [
            b"not an archive at all".as_slice(),
            b"BZh9YetAnotherArchiveFormat".as_slice(),
            &[0_u8; 1024],
        ] {
            let error = unpack_archive_bytes(
                bytes,
                &destination,
                &UnpackLimits::default(),
                "demo",
                "https://example.com/download/blob",
            )
            .expect_err("an unknown container refuses unpacking");
            let message = error.to_string();
            assert!(
                message.contains("not a format skillctl unpacks"),
                "unexpected error: {message}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn unpacked_files_do_not_carry_archive_permission_bits() {
        use std::os::unix::fs::PermissionsExt as _;

        let staged = stage_discovered_artifact(&fetched_artifact(
            "demo",
            crate::discovery::ArtifactType::Archive,
            "https://example.com/download/demo.tar.gz",
            &test_archives::tar_gzip_bytes(&[
                RawTarEntry::file(".agents/skills/demo/SKILL.md", ONLY_SKILL.as_bytes()),
                RawTarEntry::executable_file(".agents/skills/demo/scripts/run.sh", b"#!/bin/sh\n"),
            ]),
        ))
        .expect("archive with an executable entry stages");

        let script = staged.root.join(".agents/skills/demo/scripts/run.sh");
        let mode = fs::metadata(&script)
            .expect("staged script exists")
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o111,
            0,
            "unpacked content must not arrive executable, found mode {mode:o}"
        );
    }

    #[test]
    fn safe_entry_paths_reject_every_hostile_shape() {
        let destination = Path::new("/tmp/skillctl-unpack");
        for name in [
            "../escaped.txt",
            "skills/../../escaped.txt",
            "/etc/passwd",
            "C:/evil.txt",
            "..\\escaped.txt",
            "con",
            "con.txt",
            "skills/aux.md",
            "",
            ".",
        ] {
            assert!(
                safe_entry_path(destination, Path::new(name)).is_err(),
                "entry name '{name}' must be rejected",
            );
        }

        for name in ["skills", "skills/demo/SKILL.md", "./skills/demo/SKILL.md"] {
            let resolved = safe_entry_path(destination, Path::new(name))
                .unwrap_or_else(|error| panic!("entry name '{name}' must resolve: {error}"));
            assert!(
                resolved.starts_with(destination),
                "entry name '{name}' resolved outside the destination: {}",
                resolved.display()
            );
        }
    }
}
