//! Workspace manifest loading, validation, serialization, and init support.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::json;

use crate::{
    adapter::TargetRuntime,
    app::AppContext,
    error::AppError,
    lockfile::{DEFAULT_LOCKFILE_PATH, WorkspaceLockfile},
    overlay::DEFAULT_OVERLAYS_DIR,
    response::AppResponse,
    skill::DEFAULT_SKILLS_DIR,
    state::{CURRENT_MANIFEST_VERSION, MANIFEST_SCHEMA_POLICY, VersionDisposition},
};

/// Default relative path to the workspace manifest.
pub const DEFAULT_MANIFEST_PATH: &str = ".agents/skillctl.yaml";
/// Current supported manifest schema version.
pub const DEFAULT_MANIFEST_VERSION: u32 = CURRENT_MANIFEST_VERSION;

const DEFAULT_GIT_EXCLUDE_PATH: &str = ".git/info/exclude";
const GENERATED_RUNTIME_ROOT_EXCLUDES: &[&str] = &[
    "/.claude/skills/",
    "/.github/skills/",
    "/.gemini/skills/",
    "/.opencode/skills/",
];
const DEFAULT_TARGETS: [TargetRuntime; 3] = [
    TargetRuntime::Codex,
    TargetRuntime::GeminiCli,
    TargetRuntime::Opencode,
];

/// Typed workspace manifest for `.agents/skillctl.yaml`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceManifest {
    /// Filesystem path to the manifest file.
    #[serde(skip, default = "default_manifest_pathbuf")]
    pub path: PathBuf,
    /// Manifest schema version.
    #[serde(default = "default_manifest_version")]
    pub version: u32,
    /// Projection configuration for generated runtime roots.
    #[serde(default)]
    pub projection: ProjectionConfig,
    /// Layout configuration for canonical local state.
    #[serde(default)]
    pub layout: LayoutConfig,
    /// External immutable imports.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub imports: Vec<ImportDefinition>,
    /// Overlay path overrides keyed by imported skill id.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub overrides: BTreeMap<String, ManifestPath>,
    /// Enabled runtime targets.
    #[serde(default)]
    pub targets: Vec<TargetRuntime>,
    /// Telemetry policy for this workspace.
    #[serde(default)]
    pub telemetry: TelemetryConfig,
    /// Per-target adapter root overrides.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub adapters: BTreeMap<TargetRuntime, AdapterOverride>,
}

#[derive(Serialize)]
struct MinimalManifestView<'a> {
    version: u32,
    #[serde(skip_serializing_if = "ProjectionConfig::is_default")]
    projection: &'a ProjectionConfig,
    #[serde(skip_serializing_if = "LayoutConfig::is_default")]
    layout: &'a LayoutConfig,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    imports: &'a Vec<ImportDefinition>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    overrides: &'a BTreeMap<String, ManifestPath>,
    targets: &'a [TargetRuntime],
    #[serde(skip_serializing_if = "TelemetryConfig::is_default")]
    telemetry: &'a TelemetryConfig,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    adapters: &'a BTreeMap<TargetRuntime, AdapterOverride>,
}

impl Default for WorkspaceManifest {
    fn default() -> Self {
        Self::default_at(DEFAULT_MANIFEST_PATH)
    }
}

impl WorkspaceManifest {
    /// Build the default workspace manifest at the given path.
    pub fn default_at(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            version: default_manifest_version(),
            projection: ProjectionConfig::default(),
            layout: LayoutConfig::default(),
            imports: Vec::new(),
            overrides: BTreeMap::new(),
            targets: DEFAULT_TARGETS.into(),
            telemetry: TelemetryConfig::default(),
            adapters: BTreeMap::new(),
        }
    }

    /// Load the manifest from the default workspace location.
    pub fn load_from_workspace(working_directory: &Path) -> Result<Self, AppError> {
        Self::load_from_path(working_directory.join(DEFAULT_MANIFEST_PATH))
    }

    /// Load, parse, and validate a manifest from disk.
    pub fn load_from_path(path: impl AsRef<Path>) -> Result<Self, AppError> {
        let path = path.as_ref().to_path_buf();
        let contents =
            fs::read_to_string(&path).map_err(|source| AppError::FilesystemOperation {
                action: "read manifest",
                path: path.clone(),
                source,
            })?;

        Self::from_yaml_str(path, &contents)
    }

    /// Parse and validate a manifest from YAML text.
    pub fn from_yaml_str(path: impl Into<PathBuf>, contents: &str) -> Result<Self, AppError> {
        let path = path.into();
        let mut manifest: Self =
            serde_yaml::from_str(contents).map_err(|source| AppError::ManifestParse {
                path: path.clone(),
                source,
            })?;
        manifest.path = path;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Serialize the manifest using an explicit, stable field order.
    pub fn to_yaml_string(&self) -> Result<String, AppError> {
        render_manifest_yaml(self, &self.path)
    }

    /// Serialize the manifest while omitting fully default sections.
    pub fn to_minimal_yaml_string(&self) -> Result<String, AppError> {
        let view = MinimalManifestView {
            version: self.version,
            projection: &self.projection,
            layout: &self.layout,
            imports: &self.imports,
            overrides: &self.overrides,
            targets: &self.targets,
            telemetry: &self.telemetry,
            adapters: &self.adapters,
        };

        render_manifest_yaml(&view, &self.path)
    }

    /// Write the manifest to disk using the minimal deterministic YAML form.
    pub fn write_to_path(&self) -> Result<(), AppError> {
        self.validate()?;
        let contents = self.to_minimal_yaml_string()?;

        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|source| AppError::FilesystemOperation {
                action: "create manifest parent directory",
                path: parent.to_path_buf(),
                source,
            })?;
        }

        fs::write(&self.path, contents).map_err(|source| AppError::FilesystemOperation {
            action: "write manifest",
            path: self.path.clone(),
            source,
        })
    }

    /// Validate manifest invariants that require more than enum parsing.
    pub fn validate(&self) -> Result<(), AppError> {
        match MANIFEST_SCHEMA_POLICY.classify(self.version) {
            VersionDisposition::Current => {}
            VersionDisposition::NeedsMigration { from, to } => {
                return Err(manifest_validation(
                    &self.path,
                    format!("version {from} requires migration to {to}"),
                ));
            }
            VersionDisposition::Unsupported {
                found,
                minimum_supported,
                current,
            } => {
                let message = if minimum_supported == current {
                    format!("version must be {current}, found {found}")
                } else {
                    format!("version supports {minimum_supported} through {current}, found {found}")
                };
                return Err(manifest_validation(&self.path, message));
            }
        }

        let targets = validate_targets(&self.targets, &self.path)?;
        self.projection.validate(&targets, &self.path)?;
        self.layout.validate(&self.path)?;
        let import_ids = validate_imports(&self.imports, &self.path)?;
        validate_overrides(&self.overrides, &self.layout, &import_ids, &self.path)?;
        self.telemetry.validate(&self.path)?;
        validate_adapters(&self.adapters, &targets, &self.path)?;

        Ok(())
    }
}

/// Projection policy for runtime root planning.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProjectionPolicy {
    /// Choose the fewest compatible roots.
    MinimizeNoise,
    /// Prefer `.agents/skills` where compatibility is equal.
    #[default]
    PreferNeutral,
    /// Prefer each runtime's vendor-native path.
    PreferNative,
}

/// Projection materialization mode.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProjectionMode {
    /// Materialize copy-based projections.
    #[default]
    Copy,
    /// Materialize symlink-based projections.
    Symlink,
}

/// Git ignore policy for generated runtime roots.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum GitExcludeMode {
    /// Use `.git/info/exclude`.
    #[default]
    Local,
    /// Update `.gitignore`.
    Gitignore,
    /// Do not add exclusions.
    None,
}

/// Projection configuration for generated runtime roots.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectionConfig {
    /// Planner policy for target root selection.
    #[serde(default)]
    pub policy: ProjectionPolicy,
    /// Materialization mode.
    #[serde(default)]
    pub mode: ProjectionMode,
    /// Explicit acknowledgement for unstable targets when symlink mode is enabled.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow_unsafe_targets: Vec<TargetRuntime>,
    /// Whether stale generated roots may be pruned.
    #[serde(default = "default_projection_prune")]
    pub prune: bool,
    /// How generated runtime roots should be excluded from Git.
    #[serde(default)]
    pub git_exclude: GitExcludeMode,
}

impl Default for ProjectionConfig {
    fn default() -> Self {
        Self {
            policy: ProjectionPolicy::PreferNeutral,
            mode: ProjectionMode::Copy,
            allow_unsafe_targets: Vec::new(),
            prune: default_projection_prune(),
            git_exclude: GitExcludeMode::Local,
        }
    }
}

impl ProjectionConfig {
    fn is_default(&self) -> bool {
        self == &Self::default()
    }

    fn validate(
        &self,
        enabled_targets: &BTreeSet<TargetRuntime>,
        manifest_path: &Path,
    ) -> Result<(), AppError> {
        let mut seen = BTreeSet::new();
        for target in &self.allow_unsafe_targets {
            if !seen.insert(*target) {
                return Err(manifest_validation(
                    manifest_path,
                    format!(
                        "projection.allow_unsafe_targets contains duplicate runtime '{}'",
                        target.as_str()
                    ),
                ));
            }

            if !enabled_targets.contains(target) {
                return Err(manifest_validation(
                    manifest_path,
                    format!(
                        "projection.allow_unsafe_targets includes '{}' but that runtime is not enabled in targets",
                        target.as_str()
                    ),
                ));
            }
        }

        Ok(())
    }
}

/// A portable manifest path stored exactly as written in YAML.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct ManifestPath(String);

impl ManifestPath {
    /// Create a manifest path from a string.
    pub fn new(path: impl Into<String>) -> Self {
        Self(path.into())
    }

    /// Borrow the raw path string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Layout configuration for canonical skills and overlays.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LayoutConfig {
    /// Canonical local skills directory.
    #[serde(default = "default_skills_manifest_path")]
    pub skills_dir: ManifestPath,
    /// Canonical overlay root directory.
    #[serde(default = "default_overlays_manifest_path")]
    pub overlays_dir: ManifestPath,
}

impl Default for LayoutConfig {
    fn default() -> Self {
        Self {
            skills_dir: default_skills_manifest_path(),
            overlays_dir: default_overlays_manifest_path(),
        }
    }
}

impl LayoutConfig {
    fn is_default(&self) -> bool {
        self == &Self::default()
    }

    fn validate(&self, manifest_path: &Path) -> Result<(), AppError> {
        validate_relative_manifest_path(
            "layout.skills_dir",
            self.skills_dir.as_str(),
            manifest_path,
        )?;
        validate_relative_manifest_path(
            "layout.overlays_dir",
            self.overlays_dir.as_str(),
            manifest_path,
        )?;
        Ok(())
    }
}

/// Supported manifest import sources.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ImportSourceType {
    /// Git repository source.
    Git,
    /// Local directory source.
    LocalPath,
    /// Local archive source.
    Archive,
}

/// Supported import scope targets.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ManifestScope {
    /// Workspace-local scope.
    Workspace,
    /// User scope.
    User,
}

/// Immutable imported source definition.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ImportDefinition {
    /// Stable import identifier used in overrides and responses.
    pub id: String,
    /// Import source kind.
    #[serde(rename = "type")]
    pub kind: ImportSourceType,
    /// Source URL.
    pub url: String,
    /// Revision selector for the source.
    #[serde(rename = "ref")]
    pub ref_spec: String,
    /// Relative subpath to the skill inside the source.
    pub path: ManifestPath,
    /// Requested scope for the imported skill.
    pub scope: ManifestScope,
    /// Whether the import is enabled.
    #[serde(default = "default_import_enabled")]
    pub enabled: bool,
}

impl ImportDefinition {
    fn validate(&self, manifest_path: &Path) -> Result<(), AppError> {
        validate_identifier("imports[].id", &self.id, manifest_path)?;
        validate_non_empty_trimmed("imports[].url", &self.url, manifest_path)?;
        if self.url.chars().any(char::is_whitespace) {
            return Err(manifest_validation(
                manifest_path,
                format!("imports[].url must not contain whitespace: '{}'", self.url),
            ));
        }
        validate_non_empty_trimmed("imports[].ref", &self.ref_spec, manifest_path)?;
        validate_relative_manifest_path("imports[].path", self.path.as_str(), manifest_path)?;

        Ok(())
    }
}

/// Telemetry emission policy.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum TelemetryMode {
    /// Emit only public-source events.
    PublicOnly,
    /// Disable remote telemetry emission.
    Off,
}

/// Telemetry configuration for the workspace manifest.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct TelemetryConfig {
    /// Whether telemetry is enabled for this workspace.
    pub enabled: bool,
    /// Telemetry emission mode.
    pub mode: TelemetryMode,
}

impl<'de> Deserialize<'de> for TelemetryConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct RawTelemetryConfig {
            #[serde(default = "default_telemetry_enabled")]
            enabled: bool,
            mode: Option<TelemetryMode>,
        }

        let raw = RawTelemetryConfig::deserialize(deserializer)?;
        let mode = raw.mode.unwrap_or({
            if raw.enabled {
                TelemetryMode::PublicOnly
            } else {
                TelemetryMode::Off
            }
        });

        Ok(Self {
            enabled: raw.enabled,
            mode,
        })
    }
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            enabled: default_telemetry_enabled(),
            mode: TelemetryMode::PublicOnly,
        }
    }
}

impl TelemetryConfig {
    fn is_default(&self) -> bool {
        self == &Self::default()
    }

    fn validate(&self, manifest_path: &Path) -> Result<(), AppError> {
        match (self.enabled, self.mode) {
            (true, TelemetryMode::Off) => Err(manifest_validation(
                manifest_path,
                "telemetry.enabled cannot be true when telemetry.mode is off",
            )),
            (false, TelemetryMode::PublicOnly) => Err(manifest_validation(
                manifest_path,
                "telemetry.mode must be off when telemetry.enabled is false",
            )),
            _ => Ok(()),
        }
    }
}

/// Per-target adapter overrides in the manifest.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AdapterOverride {
    /// Explicit workspace root override or `auto`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_root: Option<AdapterRoot>,
    /// Explicit user root override or `auto`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_root: Option<AdapterRoot>,
}

impl AdapterOverride {
    fn validate(&self, target: TargetRuntime, manifest_path: &Path) -> Result<(), AppError> {
        if self.workspace_root.is_none() && self.user_root.is_none() {
            return Err(manifest_validation(
                manifest_path,
                format!(
                    "adapters.{} must set at least one of workspace_root or user_root",
                    target.as_str()
                ),
            ));
        }

        if let Some(root) = &self.workspace_root {
            root.validate(
                &format!("adapters.{}.workspace_root", target.as_str()),
                manifest_path,
            )?;
        }
        if let Some(root) = &self.user_root {
            root.validate(
                &format!("adapters.{}.user_root", target.as_str()),
                manifest_path,
            )?;
        }

        Ok(())
    }
}

/// Adapter root override value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AdapterRoot {
    /// Let the planner choose the documented default root.
    Auto,
    /// Use an explicit runtime root path.
    Path(String),
}

impl AdapterRoot {
    fn validate(&self, field: &str, manifest_path: &Path) -> Result<(), AppError> {
        match self {
            Self::Auto => Ok(()),
            Self::Path(path) => {
                validate_non_empty_trimmed(field, path, manifest_path)?;
                if path.chars().any(char::is_control) {
                    return Err(manifest_validation(
                        manifest_path,
                        format!("{field} must not contain control characters"),
                    ));
                }
                Ok(())
            }
        }
    }
}

impl Serialize for AdapterRoot {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Auto => serializer.serialize_str("auto"),
            Self::Path(path) => serializer.serialize_str(path),
        }
    }
}

impl<'de> Deserialize<'de> for AdapterRoot {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        if value == "auto" {
            Ok(Self::Auto)
        } else {
            Ok(Self::Path(value))
        }
    }
}

/// Typed request for `skillctl init`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct InitRequest;

/// Handle `skillctl init`.
pub fn handle_init(context: &AppContext, _request: InitRequest) -> Result<AppResponse, AppError> {
    let skills_dir = context.working_directory.join(DEFAULT_SKILLS_DIR);
    let overlays_dir = context.working_directory.join(DEFAULT_OVERLAYS_DIR);
    let manifest_path = context.working_directory.join(DEFAULT_MANIFEST_PATH);
    let lockfile_path = context.working_directory.join(DEFAULT_LOCKFILE_PATH);

    let mut created = Vec::new();
    let mut skipped = Vec::new();

    record_path_result(
        ensure_directory(&skills_dir)?,
        DEFAULT_SKILLS_DIR,
        &mut created,
        &mut skipped,
    );
    record_path_result(
        ensure_directory(&overlays_dir)?,
        DEFAULT_OVERLAYS_DIR,
        &mut created,
        &mut skipped,
    );
    record_path_result(
        ensure_manifest(&manifest_path)?,
        DEFAULT_MANIFEST_PATH,
        &mut created,
        &mut skipped,
    );
    record_path_result(
        ensure_lockfile(&lockfile_path)?,
        DEFAULT_LOCKFILE_PATH,
        &mut created,
        &mut skipped,
    );

    let git_exclude = ensure_local_git_excludes(&context.working_directory)?;
    let outcome = InitOutcome {
        created,
        skipped,
        git_exclude,
    };

    let data = json!({
        "created": outcome.created,
        "skipped": outcome.skipped,
        "git_exclude": {
            "path": outcome.git_exclude.path,
            "created": outcome.git_exclude.created,
            "skipped": outcome.git_exclude.skipped,
        }
    });

    Ok(AppResponse::success("init")
        .with_summary(render_summary(&outcome))
        .with_data(data))
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct InitOutcome {
    created: Vec<String>,
    skipped: Vec<String>,
    git_exclude: GitExcludeOutcome,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct GitExcludeOutcome {
    path: Option<String>,
    created: Vec<String>,
    skipped: Vec<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PathAction {
    Created,
    Skipped,
}

fn ensure_directory(path: &Path) -> Result<PathAction, AppError> {
    match fs::metadata(path) {
        Ok(metadata) => {
            if metadata.is_dir() {
                Ok(PathAction::Skipped)
            } else {
                Err(AppError::PathConflict {
                    path: path.to_path_buf(),
                    expected: "directory",
                })
            }
        }
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(path).map_err(|source| AppError::FilesystemOperation {
                action: "create directory",
                path: path.to_path_buf(),
                source,
            })?;
            Ok(PathAction::Created)
        }
        Err(source) => Err(AppError::FilesystemOperation {
            action: "inspect directory",
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn ensure_manifest(path: &Path) -> Result<PathAction, AppError> {
    match fs::metadata(path) {
        Ok(metadata) => {
            if metadata.is_file() {
                Ok(PathAction::Skipped)
            } else {
                Err(AppError::PathConflict {
                    path: path.to_path_buf(),
                    expected: "file",
                })
            }
        }
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).map_err(|source| AppError::FilesystemOperation {
                    action: "create manifest parent directory",
                    path: parent.to_path_buf(),
                    source,
                })?;
            }
            let contents =
                WorkspaceManifest::default_at(path.to_path_buf()).to_minimal_yaml_string()?;
            fs::write(path, contents).map_err(|source| AppError::FilesystemOperation {
                action: "write manifest",
                path: path.to_path_buf(),
                source,
            })?;
            Ok(PathAction::Created)
        }
        Err(source) => Err(AppError::FilesystemOperation {
            action: "inspect manifest",
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn ensure_lockfile(path: &Path) -> Result<PathAction, AppError> {
    match fs::metadata(path) {
        Ok(metadata) => {
            if metadata.is_file() {
                Ok(PathAction::Skipped)
            } else {
                Err(AppError::PathConflict {
                    path: path.to_path_buf(),
                    expected: "file",
                })
            }
        }
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            WorkspaceLockfile::default_at(path.to_path_buf()).write_to_path()?;
            Ok(PathAction::Created)
        }
        Err(source) => Err(AppError::FilesystemOperation {
            action: "inspect lockfile",
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn ensure_local_git_excludes(working_directory: &Path) -> Result<GitExcludeOutcome, AppError> {
    let Some(actual_path) = resolve_git_exclude_path(working_directory)? else {
        return Ok(GitExcludeOutcome {
            path: None,
            created: Vec::new(),
            skipped: vec!["no Git repository metadata found".to_string()],
        });
    };

    if let Some(parent) = actual_path.parent() {
        fs::create_dir_all(parent).map_err(|source| AppError::FilesystemOperation {
            action: "create git info directory",
            path: parent.to_path_buf(),
            source,
        })?;
    }

    let existing_content = match fs::metadata(&actual_path) {
        Ok(metadata) => {
            if !metadata.is_file() {
                return Err(AppError::PathConflict {
                    path: actual_path,
                    expected: "file",
                });
            }
            fs::read_to_string(&actual_path).map_err(|source| AppError::FilesystemOperation {
                action: "read git exclude file",
                path: actual_path.clone(),
                source,
            })?
        }
        Err(source) if source.kind() == io::ErrorKind::NotFound => String::new(),
        Err(source) => {
            return Err(AppError::FilesystemOperation {
                action: "inspect git exclude file",
                path: actual_path,
                source,
            });
        }
    };

    let existing_lines = existing_content
        .lines()
        .map(|line| line.trim_end_matches('\r').to_string())
        .collect::<BTreeSet<_>>();
    let mut created = Vec::new();
    let mut skipped = Vec::new();

    for entry in GENERATED_RUNTIME_ROOT_EXCLUDES {
        if existing_lines.contains(*entry) {
            skipped.push((*entry).to_string());
        } else {
            created.push((*entry).to_string());
        }
    }

    if !created.is_empty() {
        let mut updated_content = existing_content;
        if !updated_content.is_empty() && !updated_content.ends_with('\n') {
            updated_content.push('\n');
        }
        for entry in &created {
            updated_content.push_str(entry);
            updated_content.push('\n');
        }
        fs::write(&actual_path, updated_content).map_err(|source| {
            AppError::FilesystemOperation {
                action: "write git exclude file",
                path: actual_path.clone(),
                source,
            }
        })?;
    }

    Ok(GitExcludeOutcome {
        path: Some(DEFAULT_GIT_EXCLUDE_PATH.to_string()),
        created,
        skipped,
    })
}

fn resolve_git_exclude_path(working_directory: &Path) -> Result<Option<PathBuf>, AppError> {
    let dot_git = working_directory.join(".git");

    match fs::metadata(&dot_git) {
        Ok(metadata) => {
            if metadata.is_dir() {
                return Ok(Some(dot_git.join("info/exclude")));
            }
            if metadata.is_file() {
                let git_dir_contents = fs::read_to_string(&dot_git).map_err(|source| {
                    AppError::FilesystemOperation {
                        action: "read git metadata",
                        path: dot_git.clone(),
                        source,
                    }
                })?;
                let Some(relative_or_absolute_git_dir) = parse_git_dir(&git_dir_contents) else {
                    return Err(AppError::InvalidGitDirFile { path: dot_git });
                };

                let git_dir = if relative_or_absolute_git_dir.is_absolute() {
                    relative_or_absolute_git_dir
                } else {
                    working_directory.join(relative_or_absolute_git_dir)
                };
                return Ok(Some(git_dir.join("info/exclude")));
            }

            Err(AppError::PathConflict {
                path: dot_git,
                expected: "Git metadata file or directory",
            })
        }
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(AppError::FilesystemOperation {
            action: "inspect git metadata",
            path: dot_git,
            source,
        }),
    }
}

fn parse_git_dir(contents: &str) -> Option<PathBuf> {
    contents
        .trim()
        .strip_prefix("gitdir:")
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
}

fn validate_targets(
    targets: &[TargetRuntime],
    manifest_path: &Path,
) -> Result<BTreeSet<TargetRuntime>, AppError> {
    if targets.is_empty() {
        return Err(manifest_validation(
            manifest_path,
            "targets must contain at least one runtime",
        ));
    }

    let mut seen = BTreeSet::new();
    for target in targets {
        if !seen.insert(*target) {
            return Err(manifest_validation(
                manifest_path,
                format!("targets contains duplicate runtime '{}'", target.as_str()),
            ));
        }
    }

    Ok(seen)
}

fn validate_imports(
    imports: &[ImportDefinition],
    manifest_path: &Path,
) -> Result<BTreeSet<String>, AppError> {
    let mut ids = BTreeSet::new();

    for import in imports {
        import.validate(manifest_path)?;
        if !ids.insert(import.id.clone()) {
            return Err(manifest_validation(
                manifest_path,
                format!("imports contains duplicate id '{}'", import.id),
            ));
        }
    }

    Ok(ids)
}

fn validate_overrides(
    overrides: &BTreeMap<String, ManifestPath>,
    layout: &LayoutConfig,
    import_ids: &BTreeSet<String>,
    manifest_path: &Path,
) -> Result<(), AppError> {
    let overlays_root = layout.overlays_dir.as_str();
    let overlays_prefix = format!("{overlays_root}/");

    for (id, path) in overrides {
        validate_identifier("overrides key", id, manifest_path)?;
        validate_relative_manifest_path(&format!("overrides.{id}"), path.as_str(), manifest_path)?;

        if !import_ids.contains(id) {
            return Err(manifest_validation(
                manifest_path,
                format!("overrides.{id} does not match any imports[].id"),
            ));
        }

        if !path.as_str().starts_with(&overlays_prefix) {
            return Err(manifest_validation(
                manifest_path,
                format!(
                    "overrides.{id} must live under '{}', found '{}'",
                    overlays_root,
                    path.as_str()
                ),
            ));
        }
    }

    Ok(())
}

fn validate_adapters(
    adapters: &BTreeMap<TargetRuntime, AdapterOverride>,
    enabled_targets: &BTreeSet<TargetRuntime>,
    manifest_path: &Path,
) -> Result<(), AppError> {
    for (target, adapter) in adapters {
        if !enabled_targets.contains(target) {
            return Err(manifest_validation(
                manifest_path,
                format!(
                    "adapters.{} is configured but that runtime is not enabled in targets",
                    target.as_str()
                ),
            ));
        }

        adapter.validate(*target, manifest_path)?;
    }

    Ok(())
}

fn validate_identifier(field: &str, value: &str, manifest_path: &Path) -> Result<(), AppError> {
    if value.is_empty() {
        return Err(manifest_validation(
            manifest_path,
            format!("{field} must not be empty"),
        ));
    }

    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return Err(manifest_validation(
            manifest_path,
            format!("{field} must not be empty"),
        ));
    };
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return Err(manifest_validation(
            manifest_path,
            format!("{field} must start with a lowercase ASCII letter or digit: '{value}'"),
        ));
    }

    if !chars
        .all(|char| char.is_ascii_lowercase() || char.is_ascii_digit() || matches!(char, '-' | '_'))
    {
        return Err(manifest_validation(
            manifest_path,
            format!(
                "{field} must contain only lowercase ASCII letters, digits, '-' or '_': '{value}'"
            ),
        ));
    }

    Ok(())
}

fn validate_non_empty_trimmed(
    field: &str,
    value: &str,
    manifest_path: &Path,
) -> Result<(), AppError> {
    if value.trim().is_empty() {
        return Err(manifest_validation(
            manifest_path,
            format!("{field} must not be empty"),
        ));
    }
    if value.trim() != value {
        return Err(manifest_validation(
            manifest_path,
            format!("{field} must not contain leading or trailing whitespace"),
        ));
    }

    Ok(())
}

fn validate_relative_manifest_path(
    field: &str,
    value: &str,
    manifest_path: &Path,
) -> Result<(), AppError> {
    validate_non_empty_trimmed(field, value, manifest_path)?;

    if value.starts_with('/') {
        return Err(manifest_validation(
            manifest_path,
            format!("{field} must be relative, found absolute path '{value}'"),
        ));
    }
    if value.contains('\\') {
        return Err(manifest_validation(
            manifest_path,
            format!("{field} must use '/' separators: '{value}'"),
        ));
    }
    if value.contains(':') {
        return Err(manifest_validation(
            manifest_path,
            format!("{field} must not contain ':' so it remains portable: '{value}'"),
        ));
    }
    if value.ends_with('/') {
        return Err(manifest_validation(
            manifest_path,
            format!("{field} must not end with '/': '{value}'"),
        ));
    }

    for segment in value.split('/') {
        if segment.is_empty() {
            return Err(manifest_validation(
                manifest_path,
                format!("{field} must not contain empty path segments: '{value}'"),
            ));
        }
        if matches!(segment, "." | "..") {
            return Err(manifest_validation(
                manifest_path,
                format!("{field} must not contain '.' or '..' segments: '{value}'"),
            ));
        }
    }

    Ok(())
}

fn render_manifest_yaml<T>(value: &T, path: &Path) -> Result<String, AppError>
where
    T: Serialize,
{
    let raw = serde_yaml::to_string(value).map_err(|source| AppError::ManifestSerialize {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(format_manifest_yaml(&raw))
}

fn format_manifest_yaml(raw: &str) -> String {
    let raw = raw.strip_prefix("---\n").unwrap_or(raw);
    let mut formatted = String::new();
    let mut saw_top_level_key = false;
    let mut in_top_level_sequence = false;

    for line in raw.lines() {
        let is_top_level_key = !line.is_empty()
            && !line.starts_with(' ')
            && !line.starts_with("- ")
            && line.contains(':');
        if is_top_level_key && saw_top_level_key {
            formatted.push('\n');
        }
        if is_top_level_key {
            saw_top_level_key = true;
            in_top_level_sequence = false;
            formatted.push_str(line);
            formatted.push('\n');
            continue;
        }

        if line.starts_with("- ") {
            in_top_level_sequence = true;
            formatted.push_str("  ");
            formatted.push_str(line);
            formatted.push('\n');
            continue;
        }

        if in_top_level_sequence && line.starts_with("  ") {
            formatted.push_str("  ");
        } else if !line.starts_with(' ') {
            in_top_level_sequence = false;
        }

        formatted.push_str(line);
        formatted.push('\n');
    }

    formatted
}

fn manifest_validation(path: &Path, message: impl Into<String>) -> AppError {
    AppError::ManifestValidation {
        path: path.to_path_buf(),
        message: message.into(),
    }
}

fn default_manifest_pathbuf() -> PathBuf {
    PathBuf::from(DEFAULT_MANIFEST_PATH)
}

fn default_manifest_version() -> u32 {
    DEFAULT_MANIFEST_VERSION
}

fn default_projection_prune() -> bool {
    true
}

fn default_import_enabled() -> bool {
    true
}

fn default_telemetry_enabled() -> bool {
    true
}

fn default_skills_manifest_path() -> ManifestPath {
    ManifestPath::new(DEFAULT_SKILLS_DIR)
}

fn default_overlays_manifest_path() -> ManifestPath {
    ManifestPath::new(DEFAULT_OVERLAYS_DIR)
}

fn record_path_result(
    result: PathAction,
    display_path: &str,
    created: &mut Vec<String>,
    skipped: &mut Vec<String>,
) {
    match result {
        PathAction::Created => created.push(display_path.to_string()),
        PathAction::Skipped => skipped.push(display_path.to_string()),
    }
}

fn render_summary(outcome: &InitOutcome) -> String {
    let mut lines = Vec::new();

    if outcome.created.is_empty() && outcome.git_exclude.created.is_empty() {
        lines.push(
            "No changes were required; the skillctl workspace is already initialized".to_string(),
        );
    } else {
        lines.push("Initialized skillctl workspace".to_string());
        if !outcome.created.is_empty() {
            lines.push(format!("Created {}", outcome.created.join(", ")));
        }
        if !outcome.git_exclude.created.is_empty() {
            lines.push(format!(
                "Updated local git excludes: {}",
                outcome.git_exclude.created.join(", ")
            ));
        }
    }

    if !outcome.skipped.is_empty() {
        lines.push(format!(
            "Skipped existing paths: {}",
            outcome.skipped.join(", ")
        ));
    }

    if !outcome.git_exclude.skipped.is_empty() {
        lines.push(format!(
            "Skipped local git excludes: {}",
            outcome.git_exclude.skipped.join(", ")
        ));
    }

    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        env, fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    const MINIMAL_MANIFEST: &str = concat!(
        "version: 1\n",
        "\n",
        "targets:\n",
        "  - codex\n",
        "  - gemini-cli\n",
        "  - opencode\n",
    );

    const FULL_MANIFEST: &str = concat!(
        "version: 1\n",
        "\n",
        "projection:\n",
        "  policy: prefer-neutral\n",
        "  mode: copy\n",
        "  prune: true\n",
        "  git_exclude: local\n",
        "\n",
        "layout:\n",
        "  skills_dir: .agents/skills\n",
        "  overlays_dir: .agents/overlays\n",
        "\n",
        "imports:\n",
        "  - id: ai-sdk\n",
        "    type: git\n",
        "    url: https://github.com/vercel/ai.git\n",
        "    ref: main\n",
        "    path: skills/ai-sdk\n",
        "    scope: workspace\n",
        "    enabled: true\n",
        "\n",
        "overrides:\n",
        "  ai-sdk: .agents/overlays/ai-sdk\n",
        "\n",
        "targets:\n",
        "  - codex\n",
        "  - gemini-cli\n",
        "  - amp\n",
        "  - opencode\n",
        "\n",
        "telemetry:\n",
        "  enabled: true\n",
        "  mode: public-only\n",
        "\n",
        "adapters:\n",
        "  codex:\n",
        "    workspace_root: auto\n",
        "    user_root: auto\n",
        "  opencode:\n",
        "    workspace_root: auto\n",
        "    user_root: auto\n",
    );

    const SYMLINK_OVERRIDE_MANIFEST: &str = concat!(
        "version: 1\n",
        "\n",
        "projection:\n",
        "  policy: prefer-neutral\n",
        "  mode: symlink\n",
        "  allow_unsafe_targets:\n",
        "  - claude-code\n",
        "  prune: true\n",
        "  git_exclude: local\n",
        "\n",
        "layout:\n",
        "  skills_dir: .agents/skills\n",
        "  overlays_dir: .agents/overlays\n",
        "\n",
        "targets:\n",
        "  - claude-code\n",
        "\n",
        "telemetry:\n",
        "  enabled: true\n",
        "  mode: public-only\n",
    );

    const EXPLICIT_DEFAULT_MANIFEST: &str = concat!(
        "version: 1\n",
        "\n",
        "projection:\n",
        "  policy: prefer-neutral\n",
        "  mode: copy\n",
        "  prune: true\n",
        "  git_exclude: local\n",
        "\n",
        "layout:\n",
        "  skills_dir: .agents/skills\n",
        "  overlays_dir: .agents/overlays\n",
        "\n",
        "targets:\n",
        "  - codex\n",
        "  - gemini-cli\n",
        "  - opencode\n",
        "\n",
        "telemetry:\n",
        "  enabled: true\n",
        "  mode: public-only\n",
    );

    #[test]
    fn default_manifest_serializes_to_the_minimal_init_shape() {
        let manifest = WorkspaceManifest::default();

        assert_eq!(
            manifest
                .to_minimal_yaml_string()
                .expect("minimal manifest serializes"),
            MINIMAL_MANIFEST
        );
    }

    #[test]
    fn minimal_manifest_loads_with_explicit_defaults() {
        let manifest = WorkspaceManifest::from_yaml_str(DEFAULT_MANIFEST_PATH, MINIMAL_MANIFEST)
            .expect("minimal manifest parses");

        assert_eq!(manifest.version, DEFAULT_MANIFEST_VERSION);
        assert_eq!(manifest.path, PathBuf::from(DEFAULT_MANIFEST_PATH));
        assert_eq!(manifest.projection, ProjectionConfig::default());
        assert_eq!(manifest.layout, LayoutConfig::default());
        assert_eq!(manifest.telemetry, TelemetryConfig::default());
        assert_eq!(
            manifest.targets,
            vec![
                TargetRuntime::Codex,
                TargetRuntime::GeminiCli,
                TargetRuntime::Opencode
            ]
        );
        assert_eq!(
            manifest
                .to_yaml_string()
                .expect("explicit manifest serializes"),
            EXPLICIT_DEFAULT_MANIFEST
        );
    }

    #[test]
    fn full_manifest_roundtrips_with_stable_explicit_serialization() {
        let manifest = WorkspaceManifest::from_yaml_str(DEFAULT_MANIFEST_PATH, FULL_MANIFEST)
            .expect("full manifest parses");

        assert_eq!(
            manifest.imports,
            vec![ImportDefinition {
                id: "ai-sdk".to_string(),
                kind: ImportSourceType::Git,
                url: "https://github.com/vercel/ai.git".to_string(),
                ref_spec: "main".to_string(),
                path: ManifestPath::new("skills/ai-sdk"),
                scope: ManifestScope::Workspace,
                enabled: true,
            }]
        );
        assert_eq!(
            manifest.overrides.get("ai-sdk"),
            Some(&ManifestPath::new(".agents/overlays/ai-sdk"))
        );
        assert_eq!(
            manifest.adapters.get(&TargetRuntime::Codex),
            Some(&AdapterOverride {
                workspace_root: Some(AdapterRoot::Auto),
                user_root: Some(AdapterRoot::Auto),
            })
        );
        assert_eq!(
            manifest
                .to_yaml_string()
                .expect("full manifest serializes deterministically"),
            FULL_MANIFEST
        );
    }

    #[test]
    fn load_from_path_reads_and_validates_the_manifest_file() {
        let path = unique_temp_path("skillctl-manifest-load");
        fs::write(&path, MINIMAL_MANIFEST).expect("manifest written");

        let manifest = WorkspaceManifest::load_from_path(&path).expect("manifest loads");

        assert_eq!(manifest.path, path);
    }

    #[test]
    fn validation_rejects_duplicate_targets() {
        let manifest = concat!(
            "version: 1\n",
            "\n",
            "targets:\n",
            "  - codex\n",
            "  - codex\n",
        );

        let error = WorkspaceManifest::from_yaml_str(DEFAULT_MANIFEST_PATH, manifest)
            .expect_err("duplicate target is rejected");

        assert!(
            error
                .to_string()
                .contains("targets contains duplicate runtime 'codex'"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn validation_rejects_non_portable_layout_paths() {
        let manifest = concat!(
            "version: 1\n",
            "\n",
            "layout:\n",
            "  skills_dir: ../skills\n",
            "  overlays_dir: .agents/overlays\n",
            "\n",
            "targets:\n",
            "  - codex\n",
        );

        let error = WorkspaceManifest::from_yaml_str(DEFAULT_MANIFEST_PATH, manifest)
            .expect_err("traversal path is rejected");

        assert!(
            error
                .to_string()
                .contains("layout.skills_dir must not contain '.' or '..' segments"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn validation_rejects_overrides_without_matching_imports() {
        let manifest = concat!(
            "version: 1\n",
            "\n",
            "overrides:\n",
            "  ai-sdk: .agents/overlays/ai-sdk\n",
            "\n",
            "targets:\n",
            "  - codex\n",
        );

        let error = WorkspaceManifest::from_yaml_str(DEFAULT_MANIFEST_PATH, manifest)
            .expect_err("orphaned override is rejected");

        assert!(
            error
                .to_string()
                .contains("overrides.ai-sdk does not match any imports[].id"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn validation_rejects_conflicting_telemetry_settings() {
        let manifest = concat!(
            "version: 1\n",
            "\n",
            "targets:\n",
            "  - codex\n",
            "\n",
            "telemetry:\n",
            "  enabled: true\n",
            "  mode: off\n",
        );

        let error = WorkspaceManifest::from_yaml_str(DEFAULT_MANIFEST_PATH, manifest)
            .expect_err("conflicting telemetry settings are rejected");

        assert!(
            error
                .to_string()
                .contains("telemetry.enabled cannot be true when telemetry.mode is off"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn symlink_projection_override_targets_roundtrip() {
        let manifest =
            WorkspaceManifest::from_yaml_str(DEFAULT_MANIFEST_PATH, SYMLINK_OVERRIDE_MANIFEST)
                .expect("symlink override manifest parses");

        assert_eq!(manifest.projection.mode, ProjectionMode::Symlink);
        assert_eq!(
            manifest.projection.allow_unsafe_targets,
            vec![TargetRuntime::ClaudeCode]
        );
        assert_eq!(
            manifest
                .to_yaml_string()
                .expect("symlink override manifest serializes"),
            SYMLINK_OVERRIDE_MANIFEST
        );
    }

    fn unique_temp_path(prefix: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time moved backwards")
            .as_nanos();
        env::temp_dir().join(format!("{prefix}-{}-{unique}.yaml", std::process::id()))
    }
}
