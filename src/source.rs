//! Source detection, normalization, staging, and install inspection.

use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    ffi::OsStr,
    fs::{self, File},
    io::{self, IsTerminal, Read, Write},
    path::{Path, PathBuf},
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
    error::AppError,
    history::HistoryLedger,
    lifecycle,
    lockfile::{
        LockedHashes, LockedImport, LockedRevision, LockedSource, LockedTimestamps, LockfilePath,
        LockfileTimestamp, WorkspaceLockfile,
    },
    manifest::{
        DEFAULT_MANIFEST_PATH, ImportDefinition, ImportSourceType, ManifestPath, ManifestScope,
        WorkspaceManifest,
    },
    materialize::{self, MaterializationReport},
    overlay::{self, DEFAULT_OVERLAYS_DIR},
    response::AppResponse,
    skill::{DEFAULT_SKILLS_DIR, SKILL_MANIFEST_FILE, SkillDefinition},
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

struct PreparedInstall {
    source: NormalizedInstallSource,
    revision: SourceRevision,
    candidates: Vec<InstallCandidate>,
    root: PathBuf,
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
    lockfile: &'a WorkspaceLockfile,
    install_timestamp: &'a str,
}

/// Inspect, normalize, and stage an install source before later lifecycle steps.
pub fn inspect_install_source(
    working_directory: &Path,
    request: &InstallRequest,
) -> Result<InstallInspection, AppError> {
    Ok(prepare_install_source(working_directory, request)?.inspection())
}

/// Handle `skillctl install`.
pub fn handle_install(
    context: &AppContext,
    request: InstallRequest,
) -> Result<AppResponse, AppError> {
    let prepared = prepare_install_source(&context.working_directory, &request)?;
    let inspection = prepared.inspection();
    let selected = select_install_candidates(context, &inspection)?;
    let scope = select_install_scope(context)?;
    lifecycle::run_transaction("install", |transaction| {
        transaction.track_state_database()?;
        transaction.track_path(context.working_directory.join(DEFAULT_MANIFEST_PATH))?;
        transaction.track_path(
            context
                .working_directory
                .join(crate::lockfile::DEFAULT_LOCKFILE_PATH),
        )?;

        ensure_workspace_bootstrap(&context.working_directory)?;

        let mut manifest = WorkspaceManifest::load_from_workspace(&context.working_directory)?;
        let mut lockfile = WorkspaceLockfile::load_from_workspace(&context.working_directory)?;
        let scoped_context = install_context_for_scope(context, scope);
        for root in materialize::planned_physical_root_paths(&scoped_context, &manifest, scope)? {
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
                lockfile: &lockfile,
                install_timestamp: &install_timestamp,
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
            copy_source_tree(
                &prepared.root,
                Path::new(&operation.installed.stored_source_root),
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
        let mut warnings = BTreeSet::new();
        for skill in &installed {
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

fn prepare_install_source(
    working_directory: &Path,
    request: &InstallRequest,
) -> Result<PreparedInstall, AppError> {
    let resolved = resolve_install_source(&request.source, working_directory)?;
    let prepared = prepare_source(&resolved)?;
    let candidates = detect_install_candidates(&prepared.root, &prepared.source.raw)?;

    Ok(PreparedInstall {
        source: prepared.source,
        revision: prepared.revision,
        candidates,
        root: prepared.root,
        _staging_dir: prepared._staging_dir,
    })
}

fn select_install_candidates(
    context: &AppContext,
    inspection: &InstallInspection,
) -> Result<Vec<InstallCandidate>, AppError> {
    if let Some(skill_name) = &context.selector.skill_name {
        return select_by_exact_name(inspection, skill_name);
    }

    if install_requires_non_interactive_selection(context) {
        return Err(AppError::InputRequired { command: "install" });
    }

    prompt_for_install_candidates(inspection)
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
                        "skill '{}' is already managed by import '{}'",
                        candidate.name, existing.id
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
    let first_installed_at = context.lockfile.imports.get(&import_id).map_or_else(
        || LockfileTimestamp::new(context.install_timestamp.to_string()),
        |entry| entry.timestamps.first_installed_at.clone(),
    );

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
            revision: LockedRevision {
                resolved: revision.resolved.clone(),
                upstream: revision.upstream.clone(),
            },
            timestamps: LockedTimestamps {
                fetched_at: LockfileTimestamp::new(context.install_timestamp.to_string()),
                first_installed_at,
                last_updated_at: LockfileTimestamp::new(context.install_timestamp.to_string()),
            },
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
        SourceKind::LocalPath | SourceKind::Archive => revision.resolved.clone(),
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
                    "source must be a Git URL, existing local directory, or existing local archive",
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
    }
}

fn prepare_local_path(source: &ResolvedInstallSource) -> Result<PreparedSource, AppError> {
    let Some(path) = source.local_path.as_ref() else {
        return Err(source_validation(
            &source.normalized.raw,
            "local path sources must carry a canonical local path",
        ));
    };

    let direct_skill_manifest_path = path.join(SKILL_MANIFEST_FILE);
    match fs::metadata(&direct_skill_manifest_path) {
        Ok(_) => return stage_direct_skill_source(source, path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(AppError::FilesystemOperation {
                action: "inspect direct skill manifest",
                path: direct_skill_manifest_path,
                source,
            });
        }
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

fn stage_direct_skill_source(
    source: &ResolvedInstallSource,
    path: &Path,
) -> Result<PreparedSource, AppError> {
    let skill = SkillDefinition::load_from_dir(path)?;
    let staging_dir = TempDir::new().map_err(|source| AppError::FilesystemOperation {
        action: "create direct skill staging directory",
        path: std::env::temp_dir(),
        source,
    })?;
    let staged_root = staging_dir.path().join("source");
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
    copy_source_tree(path, &staged_skill_root)?;

    Ok(PreparedSource {
        source: source.normalized.clone(),
        revision: SourceRevision {
            resolved: hash_directory_contents(path)?,
            upstream: None,
        },
        root: staged_root,
        _staging_dir: Some(staging_dir),
    })
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
    let extract_root = staging_dir.path().join("source");
    fs::create_dir_all(&extract_root).map_err(|source| AppError::FilesystemOperation {
        action: "create archive extraction directory",
        path: extract_root.clone(),
        source,
    })?;

    extract_archive(archive_path, &extract_root, &source.normalized.raw)?;
    let root = normalized_extraction_root(&extract_root)?;

    Ok(PreparedSource {
        source: source.normalized.clone(),
        revision: SourceRevision {
            resolved: hash_file_contents(archive_path)?,
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
    if entries.len() == 1
        && fs::metadata(&entries[0])
            .map(|metadata| metadata.is_dir())
            .unwrap_or(false)
    {
        Ok(entries.remove(0))
    } else {
        Ok(extract_root.to_path_buf())
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command as ProcessCommand;

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
    fn git_sources_clone_and_capture_the_exact_commit() {
        let fixture = TestSourceFixture::new();
        let repo = fixture.path().join("repo");
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

        let expected_commit = run_test_command(
            ProcessCommand::new("git")
                .arg("-C")
                .arg(&repo)
                .args(["rev-parse", "HEAD"]),
        );
        let source = Url::from_file_path(&repo)
            .expect("repo path converts to file url")
            .to_string();

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
                trust: SkillTrust::imported_unreviewed("ai-sdk", false),
            }]
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
                trust: SkillTrust::imported_unreviewed("release-notes", false),
            }]
        );
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
                "source must be a Git URL, existing local directory, or existing local archive"
            ),
            "unexpected error: {error}"
        );
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
}
