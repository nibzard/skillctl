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
    lockfile::{DEFAULT_LOCKFILE_PATH, WorkspaceLockfile, portable},
    overlay::DEFAULT_OVERLAYS_DIR,
    response::AppResponse,
    skill::DEFAULT_SKILLS_DIR,
    state::{CURRENT_MANIFEST_VERSION, MANIFEST_SCHEMA_POLICY, ManagedScope, VersionDisposition},
};

/// Default relative path to the workspace manifest.
pub const DEFAULT_MANIFEST_PATH: &str = ".agents/skillctl.yaml";
/// Current supported manifest schema version.
pub const DEFAULT_MANIFEST_VERSION: u32 = CURRENT_MANIFEST_VERSION;

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

    /// Load the workspace manifest, or the documented defaults when it is absent.
    ///
    /// The manifest is optional for the simplest local-only flow (spec section
    /// 11.1), so a missing file yields the same defaults `skillctl init` writes
    /// instead of an error. The returned flag reports whether the defaults were
    /// used, because diagnostics report that state as an issue while mutating
    /// commands only warn. Every other read failure, such as an unreadable or
    /// invalid manifest, still fails.
    pub fn load_from_workspace_or_default(
        working_directory: &Path,
    ) -> Result<(Self, bool), AppError> {
        match Self::load_from_workspace(working_directory) {
            Ok(manifest) => Ok((manifest, false)),
            Err(AppError::FilesystemOperation {
                action: "read manifest",
                path,
                source,
            }) if source.kind() == io::ErrorKind::NotFound => Ok((Self::default_at(path), true)),
            Err(error) => Err(error),
        }
    }

    /// Load the workspace manifest for a command that answers from declared
    /// state.
    ///
    /// `restore`, `verify`, and `update` report against the imports and
    /// overrides the manifest declares, and `pin`, `rollback`, `override`, and
    /// `fork` rewrite them. Defaults would invent an empty declaration, so a
    /// checkout that lost its manifest would hear that every pinned import is
    /// stale instead of the truth: no declaration exists. The absence is
    /// therefore a validation failure that names the repair, never the raw
    /// filesystem error. Every other read failure passes through unchanged.
    pub fn load_required_from_workspace(
        context: &AppContext,
        command: &'static str,
    ) -> Result<Self, AppError> {
        match Self::load_from_workspace(&context.working_directory) {
            Ok(manifest) => Ok(manifest),
            Err(AppError::FilesystemOperation {
                action: "read manifest",
                path,
                source,
            }) if source.kind() == io::ErrorKind::NotFound => Err(AppError::ManifestMissing {
                path: PathBuf::from(crate::planner::display_path(context, &path)),
                command,
            }),
            Err(error) => Err(error),
        }
    }

    /// Resolve the managed overlay path for one imported skill.
    pub fn overlay_path_for(&self, skill_id: &str) -> ManifestPath {
        self.overrides.get(skill_id).cloned().unwrap_or_else(|| {
            let overlays_root = self.layout.overlays_dir.as_str().trim_end_matches('/');
            ManifestPath::new(format!("{overlays_root}/{skill_id}"))
        })
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
    ///
    /// Only this module's tests read the explicit form: every workspace write
    /// goes through [`WorkspaceManifest::write_to_path`], which serializes the
    /// minimal form that omits fully default sections.
    #[cfg(test)]
    pub(crate) fn to_yaml_string(&self) -> Result<String, AppError> {
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

        // Staged replacement keeps readers from observing a partial file.
        crate::state::write_file_atomically(&self.path, contents.as_bytes())
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
    /// Well-known discovery index source.
    WellKnown,
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

impl From<ManifestScope> for ManagedScope {
    fn from(scope: ManifestScope) -> Self {
        match scope {
            ManifestScope::Workspace => Self::Workspace,
            ManifestScope::User => Self::User,
        }
    }
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
        validate_token("imports[].url", &self.url, manifest_path)?;
        validate_non_empty_trimmed("imports[].ref", &self.ref_spec, manifest_path)?;
        validate_relative_manifest_path("imports[].path", self.path.as_str(), manifest_path)?;

        Ok(())
    }
}

/// Telemetry emission policy.
///
/// The policy drives local classification only. No emission backend exists, so
/// classified events are never transmitted.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum TelemetryMode {
    /// Classify only public-source events as eligible for emission.
    PublicOnly,
    /// Disable telemetry emission classification.
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

    // The manifest exists now, so init honors the Git exclusion mode it
    // declares (spec section 11.3) instead of assuming the default.
    let manifest = WorkspaceManifest::load_from_workspace(&context.working_directory)?;
    let git_exclude =
        ensure_git_excludes(&context.working_directory, manifest.projection.git_exclude)?;
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

impl GitExcludeOutcome {
    /// Outcome for a workspace whose manifest opts out of Git exclusions.
    fn no_exclusions() -> Self {
        Self {
            path: None,
            created: Vec::new(),
            skipped: Vec::new(),
        }
    }

    /// Outcome for a workspace that has no Git repository metadata.
    fn without_git_metadata() -> Self {
        Self {
            path: None,
            created: Vec::new(),
            skipped: vec!["no Git repository metadata found".to_string()],
        }
    }
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
            WorkspaceManifest::default_at(path.to_path_buf()).write_to_path()?;
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

/// Add the generated runtime root excludes the way the manifest declares.
///
/// `local` (the default) appends to the repository-local exclude file Git
/// never commits, `gitignore` appends to `.gitignore`, and `none` adds no
/// exclusions so generated roots stay visible to Git.
fn ensure_git_excludes(
    working_directory: &Path,
    mode: GitExcludeMode,
) -> Result<GitExcludeOutcome, AppError> {
    match mode {
        GitExcludeMode::None => Ok(GitExcludeOutcome::no_exclusions()),
        GitExcludeMode::Gitignore => {
            append_generated_root_excludes(&working_directory.join(".gitignore"), ".gitignore")
        }
        GitExcludeMode::Local => {
            let Some(actual_path) = resolve_git_exclude_path(working_directory)? else {
                return Ok(GitExcludeOutcome::without_git_metadata());
            };
            append_generated_root_excludes(
                &actual_path,
                &display_exclude_path(working_directory, &actual_path),
            )
        }
    }
}

/// Append the missing generated runtime root excludes to one exclude file.
///
/// The outcome reports the file through `display_path`, because worktree and
/// submodule metadata can resolve the local exclude file outside the
/// workspace.
fn append_generated_root_excludes(
    actual_path: &Path,
    display_path: &str,
) -> Result<GitExcludeOutcome, AppError> {
    if let Some(parent) = actual_path.parent() {
        fs::create_dir_all(parent).map_err(|source| AppError::FilesystemOperation {
            action: "create exclude file parent directory",
            path: parent.to_path_buf(),
            source,
        })?;
    }

    let existing_content = match fs::metadata(actual_path) {
        Ok(metadata) => {
            if !metadata.is_file() {
                return Err(AppError::PathConflict {
                    path: actual_path.to_path_buf(),
                    expected: "file",
                });
            }
            fs::read_to_string(actual_path).map_err(|source| AppError::FilesystemOperation {
                action: "read exclude file",
                path: actual_path.to_path_buf(),
                source,
            })?
        }
        Err(source) if source.kind() == io::ErrorKind::NotFound => String::new(),
        Err(source) => {
            return Err(AppError::FilesystemOperation {
                action: "inspect exclude file",
                path: actual_path.to_path_buf(),
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
        fs::write(actual_path, updated_content).map_err(|source| {
            AppError::FilesystemOperation {
                action: "write exclude file",
                path: actual_path.to_path_buf(),
                source,
            }
        })?;
    }

    Ok(GitExcludeOutcome {
        path: Some(display_path.to_string()),
        created,
        skipped,
    })
}

/// Exclude file path as reported to the user: relative to the workspace when
/// the file lives inside it, absolute when it resolves outside, as worktree
/// and submodule metadata can.
fn display_exclude_path(working_directory: &Path, actual_path: &Path) -> String {
    let relative_to_workspace = actual_path.strip_prefix(working_directory);
    crate::planner::portable_path(relative_to_workspace.unwrap_or(actual_path))
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
    if let Some(message) = portable::identifier_error(field, value) {
        return Err(manifest_validation(manifest_path, message));
    }
    Ok(())
}

fn validate_token(field: &str, value: &str, manifest_path: &Path) -> Result<(), AppError> {
    if let Some(message) = portable::token_error(field, value) {
        return Err(manifest_validation(manifest_path, message));
    }
    Ok(())
}

fn validate_non_empty_trimmed(
    field: &str,
    value: &str,
    manifest_path: &Path,
) -> Result<(), AppError> {
    if let Some(message) = portable::non_empty_trimmed_error(field, value) {
        return Err(manifest_validation(manifest_path, message));
    }
    Ok(())
}

fn validate_relative_manifest_path(
    field: &str,
    value: &str,
    manifest_path: &Path,
) -> Result<(), AppError> {
    if let Some(message) = portable::relative_path_error(field, value) {
        return Err(manifest_validation(manifest_path, message));
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
    Ok(portable::format_document_yaml(&raw))
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
                "Updated git excludes in {}: {}",
                outcome
                    .git_exclude
                    .path
                    .as_deref()
                    .unwrap_or("git exclude file"),
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
            "Skipped git excludes: {}",
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
    fn required_loader_names_init_when_the_manifest_is_absent() {
        let workspace = TempWorkspace::new("skillctl-manifest-required");
        let context = context_in(workspace.path());

        let error = WorkspaceManifest::load_required_from_workspace(&context, "verify")
            .expect_err("an absent manifest is refused, not defaulted");

        assert_eq!(
            error.exit_status(),
            crate::error::ExitStatus::ValidationFailure,
            "the refusal is a validation failure: {error}",
        );
        let message = error.to_string();
        assert!(
            message.contains(".agents/skillctl.yaml"),
            "the refusal names the missing manifest: {message}",
        );
        assert!(
            message.contains("'verify'"),
            "the refusal names the command that needs declared state: {message}",
        );
        assert!(
            message.contains("skillctl init"),
            "the refusal names the repair: {message}",
        );
        assert!(
            !message.contains("os error"),
            "the raw filesystem error must not surface: {message}",
        );
    }

    #[test]
    fn required_loader_returns_the_manifest_when_it_exists() {
        let workspace = TempWorkspace::new("skillctl-manifest-required-present");
        workspace.write_manifest(MINIMAL_MANIFEST);
        let context = context_in(workspace.path());

        let manifest =
            WorkspaceManifest::load_required_from_workspace(&context, "update").expect("loads");

        assert_eq!(
            manifest.targets,
            vec![
                TargetRuntime::Codex,
                TargetRuntime::GeminiCli,
                TargetRuntime::Opencode
            ]
        );
    }

    #[test]
    fn required_loader_passes_an_invalid_manifest_through() {
        let workspace = TempWorkspace::new("skillctl-manifest-required-invalid");
        workspace.write_manifest(concat!(
            "version: 1\n",
            "\n",
            "targets:\n",
            "  - not-a-runtime\n",
        ));
        let context = context_in(workspace.path());

        let error = WorkspaceManifest::load_required_from_workspace(&context, "update")
            .expect_err("an invalid manifest still fails");

        assert!(
            matches!(error, AppError::ManifestParse { .. }),
            "parse failures are not rewritten into a missing-manifest refusal: {error}",
        );
    }

    fn context_in(working_directory: &Path) -> AppContext {
        AppContext {
            working_directory: working_directory.to_path_buf(),
            output_mode: crate::app::OutputMode::Json,
            verbosity: crate::app::Verbosity::Normal,
            interaction_mode: crate::app::InteractionMode::NonInteractive,
            selector: crate::app::CommandSelector::default(),
        }
    }

    /// Temporary directory a manifest loader test reads from.
    struct TempWorkspace {
        path: PathBuf,
    }

    impl TempWorkspace {
        fn new(prefix: &str) -> Self {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("time moved backwards")
                .as_nanos();
            let path = env::temp_dir().join(format!("{prefix}-{}-{unique}.d", std::process::id()));
            fs::create_dir_all(&path).expect("workspace directory exists");
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }

        fn write_manifest(&self, contents: &str) {
            let path = self.path.join(".agents").join("skillctl.yaml");
            fs::create_dir_all(path.parent().expect("parent exists"))
                .expect("agents directory exists");
            fs::write(path, contents).expect("manifest written");
        }
    }

    impl Drop for TempWorkspace {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
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

    /// Round-trip one non-default `projection.git_exclude` mode.
    fn git_exclude_mode_roundtrips(mode: &str) {
        let manifest_text = format!(
            concat!(
                "version: 1\n",
                "\n",
                "projection:\n",
                "  policy: prefer-neutral\n",
                "  mode: copy\n",
                "  prune: true\n",
                "  git_exclude: {mode}\n",
                "\n",
                "layout:\n",
                "  skills_dir: .agents/skills\n",
                "  overlays_dir: .agents/overlays\n",
                "\n",
                "targets:\n",
                "  - codex\n",
                "\n",
                "telemetry:\n",
                "  enabled: true\n",
                "  mode: public-only\n",
            ),
            mode = mode,
        );

        let manifest = WorkspaceManifest::from_yaml_str(DEFAULT_MANIFEST_PATH, &manifest_text)
            .unwrap_or_else(|error| panic!("git_exclude: {mode} parses: {error}"));

        let expected_mode = match mode {
            "none" => GitExcludeMode::None,
            "gitignore" => GitExcludeMode::Gitignore,
            other => panic!("test does not know the mode '{other}'"),
        };
        assert_eq!(
            manifest.projection.git_exclude, expected_mode,
            "the declared mode must parse",
        );
        assert_eq!(
            manifest.to_yaml_string().expect("explicit form serializes"),
            manifest_text
        );
    }

    #[test]
    fn non_default_git_exclude_modes_roundtrip() {
        git_exclude_mode_roundtrips("none");
        git_exclude_mode_roundtrips("gitignore");
    }

    #[test]
    fn validation_rejects_windows_reserved_import_ids() {
        let manifest = concat!(
            "version: 1\n",
            "\n",
            "imports:\n",
            "  - id: con\n",
            "    type: git\n",
            "    url: https://github.com/vercel/ai.git\n",
            "    ref: main\n",
            "    path: skills/con\n",
            "    scope: workspace\n",
            "\n",
            "targets:\n",
            "  - codex\n",
        );

        let error = WorkspaceManifest::from_yaml_str(DEFAULT_MANIFEST_PATH, manifest)
            .expect_err("a reserved device name cannot name an import");

        assert!(
            error
                .to_string()
                .contains("imports[].id must not use a Windows reserved device name: 'con'"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn validation_rejects_control_characters_in_import_urls() {
        let manifest = concat!(
            "version: 1\n",
            "\n",
            "imports:\n",
            "  - id: ai-sdk\n",
            "    type: git\n",
            "    url: \"https://github.com/vercel/\\x07ai.git\"\n",
            "    ref: main\n",
            "    path: skills/ai-sdk\n",
            "    scope: workspace\n",
            "\n",
            "targets:\n",
            "  - codex\n",
        );

        let error = WorkspaceManifest::from_yaml_str(DEFAULT_MANIFEST_PATH, manifest)
            .expect_err("a control character in a URL is refused like in the lockfile");

        assert!(
            error
                .to_string()
                .contains("imports[].url must not contain control characters"),
            "unexpected error: {error}"
        );
    }

    /// Manifest that opts out of Git exclusions for one `handle_init` test.
    const NONE_EXCLUDE_MANIFEST: &str = concat!(
        "version: 1\n",
        "\n",
        "projection:\n",
        "  git_exclude: none\n",
        "\n",
        "targets:\n",
        "  - codex\n",
    );

    /// Manifest that routes Git exclusions into `.gitignore`.
    const GITIGNORE_EXCLUDE_MANIFEST: &str = concat!(
        "version: 1\n",
        "\n",
        "projection:\n",
        "  git_exclude: gitignore\n",
        "\n",
        "targets:\n",
        "  - codex\n",
    );

    #[test]
    fn init_writes_no_excludes_when_the_manifest_opts_out() {
        let workspace = TempWorkspace::new("skillctl-init-none-mode");
        workspace.write_manifest(NONE_EXCLUDE_MANIFEST);
        fs::create_dir_all(workspace.path().join(".git/info")).expect("git info directory exists");
        let exclude_path = workspace.path().join(".git/info/exclude");
        fs::write(&exclude_path, "# existing rule\n").expect("exclude file exists");
        let context = context_in(workspace.path());

        let response = handle_init(&context, InitRequest).expect("init runs");

        assert_eq!(
            response.data["git_exclude"],
            json!({"path": null, "created": [], "skipped": []}),
            "opting out reports no exclude work: {}",
            response.data
        );
        assert_eq!(
            fs::read_to_string(&exclude_path).expect("exclude readable"),
            "# existing rule\n",
            "the local exclude file must stay untouched",
        );
        assert!(
            !workspace.path().join(".gitignore").exists(),
            "opting out must not create .gitignore either",
        );
    }

    #[test]
    fn init_updates_gitignore_when_the_manifest_asks_for_it() {
        let workspace = TempWorkspace::new("skillctl-init-gitignore-mode");
        workspace.write_manifest(GITIGNORE_EXCLUDE_MANIFEST);
        fs::create_dir_all(workspace.path().join(".git/info")).expect("git info directory exists");
        let exclude_path = workspace.path().join(".git/info/exclude");
        fs::write(&exclude_path, "# local rule\n").expect("exclude file exists");
        fs::write(workspace.path().join(".gitignore"), "node_modules/\n")
            .expect("gitignore exists");
        let context = context_in(workspace.path());

        let response = handle_init(&context, InitRequest).expect("init runs");

        assert_eq!(response.data["git_exclude"]["path"], json!(".gitignore"));
        assert_eq!(
            fs::read_to_string(workspace.path().join(".gitignore")).expect("gitignore readable"),
            concat!(
                "node_modules/\n",
                "/.claude/skills/\n",
                "/.github/skills/\n",
                "/.gemini/skills/\n",
                "/.opencode/skills/\n",
            ),
        );
        assert_eq!(
            fs::read_to_string(&exclude_path).expect("exclude readable"),
            "# local rule\n",
            "gitignore mode must leave the local exclude file alone",
        );
        assert!(
            response
                .summary
                .as_deref()
                .unwrap_or_default()
                .contains("Updated git excludes in .gitignore"),
            "the summary names the file it updated: {:?}",
            response.summary,
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
