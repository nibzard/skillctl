//! Lockfile model, deterministic YAML IO, and state-version metadata.
//!
//! The lockfile is the portable dependency contract of a workspace. It records
//! dependency identity only: where a skill came from, which immutable revision
//! was selected, which canonical bundle was approved, and which digests the
//! installed content must reproduce.
//!
//! Local installation bookkeeping such as fetch and install timestamps is not
//! dependency identity and lives in the local state store instead. A lockfile
//! written by one teammate therefore reconstructs byte-identical content for
//! every other teammate without producing lockfile churn.
//!
//! Version 1 lockfiles recorded per-import `timestamps`. They stay loadable
//! through the compatibility loader in [`WorkspaceLockfile::from_yaml_str`],
//! which upgrades them in memory; the next command that persists the lockfile
//! writes the current version.

use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::{
    bundle::{CANONICAL_BUNDLE_FORMAT_VERSION, is_canonical_digest},
    discovery::ArtifactType,
    error::AppError,
    source::SourceKind,
    state::{
        CURRENT_LOCAL_STATE_VERSION, CURRENT_LOCKFILE_VERSION, CURRENT_MANIFEST_VERSION,
        LOCAL_STATE_SCHEMA_POLICY, LOCKFILE_SCHEMA_POLICY, MANIFEST_SCHEMA_POLICY,
        SchemaVersionPolicy, VersionDisposition,
    },
};

/// Default relative path to the workspace lockfile.
pub const DEFAULT_LOCKFILE_PATH: &str = ".agents/skillctl.lock";
/// Current supported lockfile schema version.
pub const DEFAULT_LOCKFILE_VERSION: u32 = CURRENT_LOCKFILE_VERSION;
/// Last lockfile version that recorded per-import local timestamps.
pub const LEGACY_TIMESTAMPED_LOCKFILE_VERSION: u32 = 1;

/// Typed workspace lockfile for `.agents/skillctl.lock`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceLockfile {
    /// Filesystem path to the lockfile.
    #[serde(skip, default = "default_lockfile_pathbuf")]
    pub path: PathBuf,
    /// Lockfile schema version.
    #[serde(default = "default_lockfile_version")]
    pub version: u32,
    /// Related manifest and local-state schema versions.
    #[serde(default)]
    pub state: LockfileStateVersions,
    /// Imported skills pinned by this workspace.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub imports: BTreeMap<String, LockedImport>,
    /// Version declared by the file before an in-memory legacy upgrade.
    #[serde(skip)]
    pub loaded_version: Option<u32>,
}

impl Default for WorkspaceLockfile {
    fn default() -> Self {
        Self::default_at(DEFAULT_LOCKFILE_PATH)
    }
}

impl WorkspaceLockfile {
    /// Build the default workspace lockfile at the given path.
    pub fn default_at(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            version: default_lockfile_version(),
            state: LockfileStateVersions::default(),
            imports: BTreeMap::new(),
            loaded_version: None,
        }
    }

    /// Load the lockfile from the default workspace location.
    pub fn load_from_workspace(working_directory: &Path) -> Result<Self, AppError> {
        Self::load_from_path(working_directory.join(DEFAULT_LOCKFILE_PATH))
    }

    /// Load the lockfile from the default workspace location, or the empty
    /// default when none exists.
    ///
    /// Returns the lockfile and whether no lockfile file existed, so the empty
    /// default applied. Commands that answer instead of refusing use this and
    /// report the substitution themselves, the way `doctor` reports it as a
    /// `lockfile-missing` warning.
    pub fn load_from_workspace_or_default(
        working_directory: &Path,
    ) -> Result<(Self, bool), AppError> {
        match Self::load_from_workspace(working_directory) {
            Ok(lockfile) => Ok((lockfile, false)),
            Err(AppError::FilesystemOperation {
                action: "read lockfile",
                path,
                source,
            }) if source.kind() == io::ErrorKind::NotFound => Ok((Self::default_at(path), true)),
            Err(error) => Err(error),
        }
    }

    /// Load the workspace lockfile for a command that answers from pinned
    /// state.
    ///
    /// `verify` and `update` report against the imports the lockfile pins, and
    /// `pin`, `rollback`, `override`, and `fork` rewrite them. An empty
    /// lockfile would invent a healthy workspace that pins nothing, so a
    /// checkout that lost its lockfile hears a refusal that names both repairs
    /// instead: the committed file in a team checkout, `skillctl init` for a
    /// workspace that pinned nothing yet. Defaults are never substituted, and
    /// every other read failure passes through unchanged.
    pub fn load_required_from_workspace(
        context: &crate::app::AppContext,
        command: &'static str,
    ) -> Result<Self, AppError> {
        match Self::load_from_workspace(&context.working_directory) {
            Ok(lockfile) => Ok(lockfile),
            Err(AppError::FilesystemOperation {
                action: "read lockfile",
                path,
                source,
            }) if source.kind() == io::ErrorKind::NotFound => Err(AppError::LockfileMissing {
                path: PathBuf::from(crate::planner::display_path(context, &path)),
                command,
            }),
            Err(error) => Err(error),
        }
    }

    /// Load, parse, and validate a lockfile from disk.
    pub fn load_from_path(path: impl AsRef<Path>) -> Result<Self, AppError> {
        let path = path.as_ref().to_path_buf();
        let contents =
            fs::read_to_string(&path).map_err(|source| AppError::FilesystemOperation {
                action: "read lockfile",
                path: path.clone(),
                source,
            })?;

        Self::from_yaml_str(path, &contents)
    }

    /// Parse and validate a lockfile from YAML text.
    ///
    /// Lockfiles that declare a supported older version are upgraded in memory.
    /// The upgrade never invents dependency state: it only drops local
    /// bookkeeping that moved to the local state store.
    pub fn from_yaml_str(path: impl Into<PathBuf>, contents: &str) -> Result<Self, AppError> {
        let path = path.into();
        let raw: RawLockfile =
            serde_yaml::from_str(contents).map_err(|source| AppError::LockfileParse {
                path: path.clone(),
                source,
            })?;

        let declared = raw.version.unwrap_or_else(default_lockfile_version);
        // A supported older version is upgraded in memory by this loader, so
        // only an unsupported version is rejected here.
        if matches!(
            LOCKFILE_SCHEMA_POLICY.classify(declared),
            VersionDisposition::Unsupported { .. }
        ) {
            validate_schema_version("version", declared, LOCKFILE_SCHEMA_POLICY, &path)?;
        }

        if declared > LEGACY_TIMESTAMPED_LOCKFILE_VERSION
            && raw
                .imports
                .values()
                .any(|import| import.timestamps.is_some())
        {
            return Err(lockfile_validation(
                &path,
                format!(
                    "imports may not carry timestamps in lockfile version {declared}; local \
                     timestamps belong to the local state store"
                ),
            ));
        }

        let mut imports = BTreeMap::new();
        for (id, import) in raw.imports {
            imports.insert(
                id,
                LockedImport {
                    source: import.source,
                    discovery: import.discovery,
                    revision: import.revision,
                    bundle: import.bundle,
                    hashes: import.hashes,
                },
            );
        }

        let lockfile = Self {
            path,
            version: CURRENT_LOCKFILE_VERSION,
            state: raw.state.unwrap_or_default(),
            imports,
            loaded_version: (declared != CURRENT_LOCKFILE_VERSION).then_some(declared),
        };
        lockfile.validate()?;

        Ok(lockfile)
    }

    /// Whether this lockfile was loaded from a supported older version.
    pub const fn is_legacy_upgrade(&self) -> bool {
        self.loaded_version.is_some()
    }

    /// Serialize the lockfile using a stable, explicit field order.
    pub fn to_yaml_string(&self) -> Result<String, AppError> {
        render_lockfile_yaml(self, &self.path)
    }

    /// Write the lockfile to its configured path using deterministic YAML.
    pub fn write_to_path(&self) -> Result<(), AppError> {
        let contents = self.to_yaml_string()?;

        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|source| AppError::FilesystemOperation {
                action: "create lockfile parent directory",
                path: parent.to_path_buf(),
                source,
            })?;
        }

        // Staged replacement keeps readers from observing a partial file.
        crate::state::write_file_atomically(&self.path, contents.as_bytes())
    }

    /// Validate lockfile invariants required by the spec.
    pub fn validate(&self) -> Result<(), AppError> {
        validate_schema_version("version", self.version, LOCKFILE_SCHEMA_POLICY, &self.path)?;
        self.state.validate(&self.path)?;

        for (id, import) in &self.imports {
            validate_identifier("imports key", id, &self.path)?;
            import.validate(id, &self.path)?;
        }

        Ok(())
    }
}

/// Version snapshot for related state-bearing documents.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LockfileStateVersions {
    /// Manifest schema version this lockfile was produced against.
    #[serde(default = "default_manifest_version")]
    pub manifest_version: u32,
    /// Local state store schema version expected by the writer.
    #[serde(default = "default_local_state_version")]
    pub local_state_version: u32,
}

impl Default for LockfileStateVersions {
    fn default() -> Self {
        Self {
            manifest_version: default_manifest_version(),
            local_state_version: default_local_state_version(),
        }
    }
}

impl LockfileStateVersions {
    fn validate(&self, lockfile_path: &Path) -> Result<(), AppError> {
        validate_schema_version(
            "state.manifest_version",
            self.manifest_version,
            MANIFEST_SCHEMA_POLICY,
            lockfile_path,
        )?;
        // The state store migrates itself on open and is local to one machine,
        // so a lockfile written against an older state schema still loads: the
        // snapshot records which writer produced the file, not a requirement
        // this binary has to meet. Only a version this binary cannot read is a
        // refusal.
        validate_supported_schema_version(
            "state.local_state_version",
            self.local_state_version,
            LOCAL_STATE_SCHEMA_POLICY,
            lockfile_path,
        )?;

        Ok(())
    }
}

/// Pin metadata for one imported skill.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LockedImport {
    /// Source identity and selected subpath.
    pub source: LockedSource,
    /// Discovery index entry a well-known import was installed from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discovery: Option<LockedDiscoveryEntry>,
    /// Resolved immutable revision data.
    pub revision: LockedRevision,
    /// Canonical bundle recorded for the approved content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle: Option<LockedBundle>,
    /// Stable hashes that identify the effective version.
    pub hashes: LockedHashes,
}

impl LockedImport {
    fn validate(&self, id: &str, lockfile_path: &Path) -> Result<(), AppError> {
        self.source.validate(id, lockfile_path)?;
        self.discovery
            .as_ref()
            .map(|entry| entry.validate(id, lockfile_path))
            .transpose()?;
        self.revision.validate(id, lockfile_path)?;
        if let Some(bundle) = &self.bundle {
            bundle.validate(id, lockfile_path)?;
        }
        self.hashes.validate(id, lockfile_path)?;
        self.validate_discovery_pairing(id, lockfile_path)?;
        Ok(())
    }

    /// A well-known import must record the index entry it was installed from,
    /// and no other source kind may carry one.
    fn validate_discovery_pairing(&self, id: &str, lockfile_path: &Path) -> Result<(), AppError> {
        match (&self.source.kind, &self.discovery) {
            (SourceKind::WellKnown, Some(entry)) => {
                if entry.skill != id {
                    return Err(lockfile_validation(
                        lockfile_path,
                        format!(
                            "imports.{id}.discovery.skill must equal the import id '{id}', found \
                             '{}'",
                            entry.skill
                        ),
                    ));
                }
                if entry.digest != self.revision.resolved {
                    return Err(lockfile_validation(
                        lockfile_path,
                        format!(
                            "imports.{id}.discovery.digest must equal the verified revision \
                             '{}', found '{}'",
                            self.revision.resolved, entry.digest
                        ),
                    ));
                }
                Ok(())
            }
            (SourceKind::WellKnown, None) => Err(lockfile_validation(
                lockfile_path,
                format!(
                    "imports.{id} has a well-known source, so it must record the discovery index \
                     entry it was installed from"
                ),
            )),
            (_, Some(_)) => Err(lockfile_validation(
                lockfile_path,
                format!(
                    "imports.{id} has a '{}' source, so it must not record a discovery index \
                     entry",
                    lockfile_kind_label(self.source.kind)
                ),
            )),
            (_, None) => Ok(()),
        }
    }

    /// Canonical bundle digest pinned for this import, when one is recorded.
    pub fn bundle_digest(&self) -> Option<&str> {
        self.bundle.as_ref().map(|bundle| bundle.digest.as_str())
    }
}

/// The discovery index entry a well-known import was installed from.
///
/// The entry is what a later acquisition re-fetches: it names the artifact URL
/// and the digest the index declared, which the verified artifact bytes matched.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LockedDiscoveryEntry {
    /// Skill name the discovery index declared for this artifact.
    pub skill: String,
    /// Distribution type the index declared.
    #[serde(rename = "type")]
    pub artifact_type: ArtifactType,
    /// Artifact URL resolved against the index URL.
    pub url: String,
    /// Digest the index declared, which the installed artifact matched.
    pub digest: String,
}

impl LockedDiscoveryEntry {
    fn validate(&self, id: &str, lockfile_path: &Path) -> Result<(), AppError> {
        let prefix = format!("imports.{id}.discovery");
        validate_non_empty_trimmed(&format!("{prefix}.skill"), &self.skill, lockfile_path)?;
        if let Some(reason) = crate::skill::skill_name_format_error(&self.skill) {
            return Err(lockfile_validation(
                lockfile_path,
                format!("{prefix}.skill declares an invalid skill name: {reason}"),
            ));
        }
        validate_source_url(&format!("{prefix}.url"), &self.url, lockfile_path)?;
        if !is_canonical_digest(&self.digest) {
            return Err(lockfile_validation(
                lockfile_path,
                format!("{prefix}.digest must be a canonical 'sha256:<hex>' digest"),
            ));
        }

        Ok(())
    }
}

/// Immutable source identity captured in the lockfile.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LockedSource {
    /// Normalized source type.
    #[serde(rename = "type")]
    pub kind: SourceKind,
    /// Normalized source URL or file URL.
    pub url: String,
    /// Selected skill path inside the source.
    pub subpath: LockfilePath,
}

impl LockedSource {
    fn validate(&self, id: &str, lockfile_path: &Path) -> Result<(), AppError> {
        let prefix = format!("imports.{id}.source");
        validate_source_url(&format!("{prefix}.url"), &self.url, lockfile_path)?;
        validate_relative_path(
            &format!("{prefix}.subpath"),
            self.subpath.as_str(),
            lockfile_path,
        )?;
        Ok(())
    }
}

/// Resolved immutable revision information for a locked import.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LockedRevision {
    /// Selected commit or archive digest.
    pub resolved: String,
    /// Last observed upstream commit for update checks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream: Option<String>,
}

impl LockedRevision {
    fn validate(&self, id: &str, lockfile_path: &Path) -> Result<(), AppError> {
        let prefix = format!("imports.{id}.revision");
        validate_token(&format!("{prefix}.resolved"), &self.resolved, lockfile_path)?;
        if let Some(upstream) = &self.upstream {
            validate_token(&format!("{prefix}.upstream"), upstream, lockfile_path)?;
        }
        Ok(())
    }
}

/// Canonical bundle identity pinned for one imported skill.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LockedBundle {
    /// Canonical bundle format version of the recorded digest.
    pub format_version: u32,
    /// Canonical bundle digest of the approved content.
    pub digest: String,
    /// Number of regular files in the bundle.
    pub files: u64,
    /// Total bytes of regular files in the bundle.
    pub bytes: u64,
}

impl LockedBundle {
    fn validate(&self, id: &str, lockfile_path: &Path) -> Result<(), AppError> {
        let prefix = format!("imports.{id}.bundle");
        if self.format_version == 0 || self.format_version > CANONICAL_BUNDLE_FORMAT_VERSION {
            return Err(lockfile_validation(
                lockfile_path,
                format!(
                    "{prefix}.format_version {} is not supported; this skillctl supports \
                     {CANONICAL_BUNDLE_FORMAT_VERSION}",
                    self.format_version
                ),
            ));
        }
        if !is_canonical_digest(&self.digest) {
            return Err(lockfile_validation(
                lockfile_path,
                format!("{prefix}.digest must be a canonical 'sha256:<hex>' digest"),
            ));
        }

        Ok(())
    }
}

/// Hashes that define the effective installed version.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LockedHashes {
    /// Source content hash.
    pub content: String,
    /// Overlay content hash.
    pub overlay: String,
    /// Effective version hash derived from revision and content hashes.
    pub effective_version: String,
}

impl LockedHashes {
    fn validate(&self, id: &str, lockfile_path: &Path) -> Result<(), AppError> {
        let prefix = format!("imports.{id}.hashes");
        validate_token(&format!("{prefix}.content"), &self.content, lockfile_path)?;
        validate_token(&format!("{prefix}.overlay"), &self.overlay, lockfile_path)?;
        validate_token(
            &format!("{prefix}.effective_version"),
            &self.effective_version,
            lockfile_path,
        )?;
        Ok(())
    }
}

/// On-disk shape of a lockfile, including fields only older versions wrote.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLockfile {
    #[serde(default)]
    version: Option<u32>,
    #[serde(default)]
    state: Option<LockfileStateVersions>,
    #[serde(default)]
    imports: BTreeMap<String, RawLockedImport>,
}

/// On-disk shape of one import, including the legacy timestamp block.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLockedImport {
    source: LockedSource,
    #[serde(default)]
    discovery: Option<LockedDiscoveryEntry>,
    revision: LockedRevision,
    #[serde(default)]
    bundle: Option<LockedBundle>,
    hashes: LockedHashes,
    /// Local fetch and install timestamps written by lockfile version 1.
    ///
    /// Accepted so old locks stay loadable, then dropped: this is local
    /// bookkeeping, not dependency identity.
    #[serde(default)]
    timestamps: Option<serde_yaml::Value>,
}

/// Portable lockfile path stored with forward slashes.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct LockfilePath(String);

impl LockfilePath {
    /// Create a lockfile path from a string.
    pub fn new(path: impl Into<String>) -> Self {
        Self(path.into())
    }

    /// Borrow the raw path string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn render_lockfile_yaml<T>(value: &T, path: &Path) -> Result<String, AppError>
where
    T: Serialize,
{
    let raw = serde_yaml::to_string(value).map_err(|source| AppError::LockfileSerialize {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(format_lockfile_yaml(&raw))
}

fn format_lockfile_yaml(raw: &str) -> String {
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

fn validate_schema_version(
    field: &str,
    found: u32,
    policy: SchemaVersionPolicy,
    lockfile_path: &Path,
) -> Result<(), AppError> {
    match policy.classify(found) {
        VersionDisposition::Current => Ok(()),
        VersionDisposition::NeedsMigration { from, to } => Err(lockfile_validation(
            lockfile_path,
            format!("{field} version {from} requires migration to {to}"),
        )),
        VersionDisposition::Unsupported {
            found,
            minimum_supported,
            current,
        } => {
            let message = if minimum_supported == current {
                format!("{field} must be {current}, found {found}")
            } else {
                format!(
                    "{field} supports versions {minimum_supported} through {current}, found {found}"
                )
            };
            Err(lockfile_validation(lockfile_path, message))
        }
    }
}

/// Accept every version the policy can still read, not only the current one.
///
/// Use this for a recorded snapshot a newer writer may legitimately lag behind,
/// such as the state schema version a committed lockfile was written against.
fn validate_supported_schema_version(
    field: &str,
    found: u32,
    policy: SchemaVersionPolicy,
    lockfile_path: &Path,
) -> Result<(), AppError> {
    if (policy.minimum_supported()..=policy.current()).contains(&found) {
        return Ok(());
    }

    let message = if policy.minimum_supported() == policy.current() {
        format!("{field} must be {}, found {found}", policy.current())
    } else {
        format!(
            "{field} supports versions {} through {}, found {found}",
            policy.minimum_supported(),
            policy.current()
        )
    };
    Err(lockfile_validation(lockfile_path, message))
}

fn validate_identifier(field: &str, value: &str, lockfile_path: &Path) -> Result<(), AppError> {
    if value.is_empty() {
        return Err(lockfile_validation(
            lockfile_path,
            format!("{field} must not be empty"),
        ));
    }

    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return Err(lockfile_validation(
            lockfile_path,
            format!("{field} must not be empty"),
        ));
    };
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return Err(lockfile_validation(
            lockfile_path,
            format!("{field} must start with a lowercase ASCII letter or digit: '{value}'"),
        ));
    }

    if !chars
        .all(|char| char.is_ascii_lowercase() || char.is_ascii_digit() || matches!(char, '-' | '_'))
    {
        return Err(lockfile_validation(
            lockfile_path,
            format!(
                "{field} must contain only lowercase ASCII letters, digits, '-' or '_': '{value}'"
            ),
        ));
    }

    Ok(())
}

fn validate_source_url(field: &str, value: &str, lockfile_path: &Path) -> Result<(), AppError> {
    validate_non_empty_trimmed(field, value, lockfile_path)?;
    if value.chars().any(char::is_whitespace) {
        return Err(lockfile_validation(
            lockfile_path,
            format!("{field} must not contain whitespace: '{value}'"),
        ));
    }
    Ok(())
}

fn validate_token(field: &str, value: &str, lockfile_path: &Path) -> Result<(), AppError> {
    validate_non_empty_trimmed(field, value, lockfile_path)?;
    if value.chars().any(char::is_whitespace) {
        return Err(lockfile_validation(
            lockfile_path,
            format!("{field} must not contain whitespace: '{value}'"),
        ));
    }
    Ok(())
}

fn validate_non_empty_trimmed(
    field: &str,
    value: &str,
    lockfile_path: &Path,
) -> Result<(), AppError> {
    if value.trim().is_empty() {
        return Err(lockfile_validation(
            lockfile_path,
            format!("{field} must not be empty"),
        ));
    }
    if value.trim() != value {
        return Err(lockfile_validation(
            lockfile_path,
            format!("{field} must not contain leading or trailing whitespace"),
        ));
    }
    if value.chars().any(char::is_control) {
        return Err(lockfile_validation(
            lockfile_path,
            format!("{field} must not contain control characters"),
        ));
    }

    Ok(())
}

fn validate_relative_path(field: &str, value: &str, lockfile_path: &Path) -> Result<(), AppError> {
    validate_non_empty_trimmed(field, value, lockfile_path)?;

    if value.starts_with('/') {
        return Err(lockfile_validation(
            lockfile_path,
            format!("{field} must be relative, found absolute path '{value}'"),
        ));
    }
    if value.contains('\\') {
        return Err(lockfile_validation(
            lockfile_path,
            format!("{field} must use '/' separators: '{value}'"),
        ));
    }
    if value.contains(':') {
        return Err(lockfile_validation(
            lockfile_path,
            format!("{field} must not contain ':' so it remains portable: '{value}'"),
        ));
    }
    if value.ends_with('/') {
        return Err(lockfile_validation(
            lockfile_path,
            format!("{field} must not end with '/': '{value}'"),
        ));
    }

    for segment in value.split('/') {
        if segment.is_empty() {
            return Err(lockfile_validation(
                lockfile_path,
                format!("{field} must not contain empty path segments: '{value}'"),
            ));
        }
        if matches!(segment, "." | "..") {
            return Err(lockfile_validation(
                lockfile_path,
                format!("{field} must not contain '.' or '..' segments: '{value}'"),
            ));
        }
    }

    Ok(())
}

fn lockfile_validation(path: &Path, message: impl Into<String>) -> AppError {
    AppError::LockfileValidation {
        path: path.to_path_buf(),
        message: message.into(),
    }
}

/// Stable label of one locked source kind, for validation messages.
fn lockfile_kind_label(kind: SourceKind) -> &'static str {
    match kind {
        SourceKind::Git => "git",
        SourceKind::LocalPath => "local-path",
        SourceKind::Archive => "archive",
        SourceKind::WellKnown => "well-known",
    }
}

fn default_lockfile_pathbuf() -> PathBuf {
    PathBuf::from(DEFAULT_LOCKFILE_PATH)
}

fn default_lockfile_version() -> u32 {
    DEFAULT_LOCKFILE_VERSION
}

fn default_manifest_version() -> u32 {
    CURRENT_MANIFEST_VERSION
}

fn default_local_state_version() -> u32 {
    CURRENT_LOCAL_STATE_VERSION
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Version-2 lockfile header that records one local state schema version.
    fn lockfile_header(local_state_version: u32) -> String {
        format!(
            "version: 2\n\nstate:\n  manifest_version: 1\n  local_state_version: \
             {local_state_version}\n"
        )
    }

    fn minimal_lockfile() -> String {
        lockfile_header(CURRENT_LOCAL_STATE_VERSION)
    }

    /// Import block the full-lockfile fixture appends to one header.
    const FULL_LOCKFILE_IMPORTS: &str = concat!(
        "\n",
        "imports:\n",
        "  ai-sdk:\n",
        "    source:\n",
        "      type: git\n",
        "      url: https://github.com/vercel/ai.git\n",
        "      subpath: skills/ai-sdk\n",
        "    revision:\n",
        "      resolved: 0123456789abcdef0123456789abcdef01234567\n",
        "      upstream: fedcba9876543210fedcba9876543210fedcba98\n",
        "    bundle:\n",
        "      format_version: 1\n",
        "      digest: sha256:a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2\n",
        "      files: 3\n",
        "      bytes: 1200\n",
        "    hashes:\n",
        "      content: sha256:source-content\n",
        "      overlay: sha256:overlay\n",
        "      effective_version: sha256:effective\n",
    );

    fn full_lockfile() -> String {
        format!(
            "{}{}",
            lockfile_header(CURRENT_LOCAL_STATE_VERSION),
            FULL_LOCKFILE_IMPORTS,
        )
    }

    const LEGACY_LOCKFILE: &str = concat!(
        "version: 1\n",
        "\n",
        "state:\n",
        "  manifest_version: 1\n",
        "  local_state_version: 2\n",
        "\n",
        "imports:\n",
        "  ai-sdk:\n",
        "    source:\n",
        "      type: git\n",
        "      url: https://github.com/vercel/ai.git\n",
        "      subpath: skills/ai-sdk\n",
        "    revision:\n",
        "      resolved: 0123456789abcdef0123456789abcdef01234567\n",
        "    timestamps:\n",
        "      fetched_at: 2026-03-19T10:15:30Z\n",
        "      first_installed_at: 2026-03-19T10:16:00Z\n",
        "      last_updated_at: 2026-03-19T10:17:00Z\n",
        "    hashes:\n",
        "      content: sha256:source-content\n",
        "      overlay: sha256:overlay\n",
        "      effective_version: sha256:effective\n",
    );

    #[test]
    fn default_lockfile_serializes_to_the_minimal_shape() {
        let lockfile = WorkspaceLockfile::default();

        assert_eq!(
            lockfile
                .to_yaml_string()
                .expect("lockfile serializes deterministically"),
            minimal_lockfile()
        );
    }

    #[test]
    fn lockfile_roundtrips_with_stable_serialization() {
        let full = full_lockfile();
        let lockfile = WorkspaceLockfile::from_yaml_str(DEFAULT_LOCKFILE_PATH, &full)
            .expect("lockfile parses");

        assert!(!lockfile.is_legacy_upgrade());
        assert_eq!(
            lockfile
                .to_yaml_string()
                .expect("lockfile roundtrips deterministically"),
            full
        );
    }

    /// Digest a well-known import pins, which its discovery entry repeats.
    const WELL_KNOWN_DIGEST: &str =
        "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn well_known_lockfile() -> String {
        format!(
            concat!(
                "version: 2\n",
                "\n",
                "state:\n",
                "  manifest_version: 1\n",
                "  local_state_version: {}\n",
                "\n",
                "imports:\n",
                "  code-review:\n",
                "    source:\n",
                "      type: well-known\n",
                "      url: https://skills.example.com/.well-known/agent-skills/index.json\n",
                "      subpath: skills/code-review\n",
                "    discovery:\n",
                "      skill: code-review\n",
                "      type: skill-md\n",
                "      url: https://skills.example.com/.well-known/agent-skills/code-review/SKILL.md\n",
                "      digest: {digest}\n",
                "    revision:\n",
                "      resolved: {digest}\n",
                "    hashes:\n",
                "      content: sha256:source-content\n",
                "      overlay: sha256:overlay\n",
                "      effective_version: sha256:effective\n",
            ),
            CURRENT_LOCAL_STATE_VERSION,
            digest = WELL_KNOWN_DIGEST,
        )
    }

    #[test]
    fn well_known_imports_roundtrip_with_their_discovery_entry() {
        let lockfile_text = well_known_lockfile();
        let lockfile = WorkspaceLockfile::from_yaml_str(DEFAULT_LOCKFILE_PATH, &lockfile_text)
            .expect("a well-known import parses");

        let import = lockfile.imports.get("code-review").expect("import loads");
        assert_eq!(import.source.kind, SourceKind::WellKnown);
        let discovery = import.discovery.as_ref().expect("the entry is recorded");
        assert_eq!(discovery.skill, "code-review");
        assert_eq!(discovery.artifact_type, ArtifactType::SkillMd);
        assert_eq!(
            discovery.url,
            "https://skills.example.com/.well-known/agent-skills/code-review/SKILL.md"
        );
        assert_eq!(discovery.digest, WELL_KNOWN_DIGEST);

        assert_eq!(
            lockfile
                .to_yaml_string()
                .expect("lockfile roundtrips deterministically"),
            lockfile_text
        );
    }

    #[test]
    fn a_well_known_import_without_its_discovery_entry_is_rejected() {
        let artifact_url =
            "https://skills.example.com/.well-known/agent-skills/code-review/SKILL.md";
        let stripped: String = well_known_lockfile()
            .lines()
            .filter(|line| {
                !matches!(
                    line.trim(),
                    "discovery:" | "skill: code-review" | "type: skill-md"
                ) && line.trim() != format!("url: {artifact_url}")
                    && line.trim() != format!("digest: {WELL_KNOWN_DIGEST}")
            })
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        assert!(
            !stripped.contains("discovery:"),
            "the fixture must drop the whole block: {stripped}"
        );

        let error = WorkspaceLockfile::from_yaml_str(DEFAULT_LOCKFILE_PATH, &stripped)
            .expect_err("the recorded entry is required");
        assert!(
            error.to_string().contains("discovery index entry"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn a_discovery_entry_on_another_source_kind_is_rejected() {
        let mismatched = well_known_lockfile().replace("type: well-known", "type: git");

        let error = WorkspaceLockfile::from_yaml_str(DEFAULT_LOCKFILE_PATH, &mismatched)
            .expect_err("only well-known sources record an entry");
        assert!(
            error
                .to_string()
                .contains("must not record a discovery index entry"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn a_discovery_digest_that_disagrees_with_the_revision_is_rejected() {
        let disagreed = well_known_lockfile().replace(
            &format!("      digest: {WELL_KNOWN_DIGEST}\n"),
            "      digest: sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\n",
        );
        assert_ne!(disagreed, well_known_lockfile());

        let error = WorkspaceLockfile::from_yaml_str(DEFAULT_LOCKFILE_PATH, &disagreed)
            .expect_err("the entry must pin the verified revision");
        assert!(
            error.to_string().contains("discovery.digest must equal"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn legacy_lockfiles_load_through_the_compatibility_loader() {
        let lockfile = WorkspaceLockfile::from_yaml_str(DEFAULT_LOCKFILE_PATH, LEGACY_LOCKFILE)
            .expect("legacy lockfile loads");

        assert!(lockfile.is_legacy_upgrade());
        assert_eq!(lockfile.loaded_version, Some(1));
        assert_eq!(lockfile.version, CURRENT_LOCKFILE_VERSION);
        let import = lockfile.imports.get("ai-sdk").expect("import is retained");
        assert_eq!(import.bundle, None);
        assert_eq!(
            import.revision.resolved,
            "0123456789abcdef0123456789abcdef01234567"
        );

        // The upgrade writes the current version and drops local bookkeeping.
        let upgraded = lockfile.to_yaml_string().expect("lockfile serializes");
        assert!(
            !upgraded.contains("timestamps"),
            "upgraded lockfile must not carry local timestamps: {upgraded}"
        );
        assert!(
            !upgraded.contains("fetched_at"),
            "upgraded lockfile must not carry fetch timestamps: {upgraded}"
        );
    }

    #[test]
    fn a_lockfile_written_against_an_older_state_schema_still_loads() {
        // The state store migrates itself locally, so a committed lockfile that
        // records an older state schema version is a fact about its writer, not
        // an incompatibility with this binary.
        for version in
            LOCAL_STATE_SCHEMA_POLICY.minimum_supported()..LOCAL_STATE_SCHEMA_POLICY.current()
        {
            let older = lockfile_header(version);
            let lockfile = WorkspaceLockfile::from_yaml_str(DEFAULT_LOCKFILE_PATH, &older)
                .unwrap_or_else(|error| panic!("version {version} loads: {error}"));

            assert_eq!(lockfile.state.local_state_version, version);
            assert_eq!(
                lockfile.to_yaml_string().expect("lockfile serializes"),
                older,
                "a loaded snapshot roundtrips without being rewritten: {older}",
            );
        }
    }

    #[test]
    fn a_lockfile_written_against_an_unreadable_state_schema_is_rejected() {
        let newer = lockfile_header(CURRENT_LOCAL_STATE_VERSION + 1);
        let error = WorkspaceLockfile::from_yaml_str(DEFAULT_LOCKFILE_PATH, &newer)
            .expect_err("a version this binary cannot read is refused");

        assert!(
            error.to_string().contains(&format!(
                "state.local_state_version supports versions 1 through \
                 {CURRENT_LOCAL_STATE_VERSION}"
            )),
            "unexpected error: {error}",
        );
    }

    #[test]
    fn current_lockfiles_reject_legacy_timestamp_blocks() {
        let legacy = LEGACY_LOCKFILE.replace("version: 1", "version: 2");
        let error = WorkspaceLockfile::from_yaml_str(DEFAULT_LOCKFILE_PATH, &legacy)
            .expect_err("timestamps are rejected in version 2");

        assert!(
            error
                .to_string()
                .contains("imports may not carry timestamps"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn unsupported_lockfile_versions_are_rejected() {
        let future = full_lockfile().replace("version: 2", "version: 3");
        let error = WorkspaceLockfile::from_yaml_str(DEFAULT_LOCKFILE_PATH, &future)
            .expect_err("future versions are rejected");

        assert!(
            error.to_string().contains("supports versions 1 through"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn bundle_digests_must_be_canonical() {
        let malformed = full_lockfile().replace(
            "digest: sha256:a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2",
            "digest: not-a-digest",
        );
        let error = WorkspaceLockfile::from_yaml_str(DEFAULT_LOCKFILE_PATH, &malformed)
            .expect_err("malformed bundle digests are rejected");

        assert!(
            error
                .to_string()
                .contains("canonical 'sha256:<hex>' digest"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn lockfile_validation_rejects_non_portable_subpaths() {
        let lockfile = concat!(
            "\n",
            "imports:\n",
            "  ai-sdk:\n",
            "    source:\n",
            "      type: git\n",
            "      url: https://github.com/vercel/ai.git\n",
            "      subpath: ../skills/ai-sdk\n",
            "    revision:\n",
            "      resolved: 0123456789abcdef0123456789abcdef01234567\n",
            "    hashes:\n",
            "      content: sha256:source-content\n",
            "      overlay: sha256:overlay\n",
            "      effective_version: sha256:effective\n",
        );
        let text = format!(
            "{}{}",
            lockfile_header(CURRENT_LOCAL_STATE_VERSION),
            lockfile,
        );

        let error = WorkspaceLockfile::from_yaml_str(DEFAULT_LOCKFILE_PATH, &text)
            .expect_err("invalid subpath is rejected");

        assert!(
            error
                .to_string()
                .contains("imports.ai-sdk.source.subpath must not contain '.' or '..' segments"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn schema_version_policy_distinguishes_current_migrating_and_unsupported_versions() {
        let policy = crate::state::SchemaVersionPolicy::new(2, 1);

        assert_eq!(
            policy.classify(2),
            crate::state::VersionDisposition::Current
        );
        assert_eq!(
            policy.classify(1),
            crate::state::VersionDisposition::NeedsMigration { from: 1, to: 2 }
        );
        assert_eq!(
            policy.classify(0),
            crate::state::VersionDisposition::Unsupported {
                found: 0,
                minimum_supported: 1,
                current: 2,
            }
        );
    }

    #[test]
    fn required_loader_names_both_repairs_when_the_lockfile_is_absent() {
        let workspace = TempWorkspace::new("skillctl-lockfile-required");
        let context = context_in(workspace.path());

        let error = WorkspaceLockfile::load_required_from_workspace(&context, "verify")
            .expect_err("an absent lockfile is refused, not defaulted");

        assert_eq!(
            error.exit_status(),
            crate::error::ExitStatus::ValidationFailure,
            "the refusal is a validation failure: {error}",
        );
        let message = error.to_string();
        assert!(
            message.contains(".agents/skillctl.lock"),
            "the refusal names the missing lockfile: {message}",
        );
        assert!(
            message.contains("'verify'"),
            "the refusal names the command that needs pinned state: {message}",
        );
        assert!(
            message.contains("retrieve the committed .agents/skillctl.lock"),
            "the refusal names the team-checkout repair: {message}",
        );
        assert!(
            message.contains("skillctl init"),
            "the refusal names the new-workspace repair: {message}",
        );
        assert!(
            message.contains("empty lockfile"),
            "the refusal separates the two repairs by their hazard: {message}",
        );
        assert!(
            !message.contains("os error"),
            "the raw filesystem error must not surface: {message}",
        );
    }

    #[test]
    fn required_loader_returns_the_lockfile_when_it_exists() {
        let workspace = TempWorkspace::new("skillctl-lockfile-required-present");
        workspace.write_lockfile(&minimal_lockfile());
        let context = context_in(workspace.path());

        let lockfile =
            WorkspaceLockfile::load_required_from_workspace(&context, "update").expect("loads");

        assert_eq!(lockfile.imports.len(), 0);
        assert_eq!(lockfile.path, workspace.path().join(DEFAULT_LOCKFILE_PATH));
    }

    #[test]
    fn required_loader_passes_an_invalid_lockfile_through() {
        let workspace = TempWorkspace::new("skillctl-lockfile-required-invalid");
        workspace.write_lockfile("version: not-a-number\n");
        let context = context_in(workspace.path());

        let error = WorkspaceLockfile::load_required_from_workspace(&context, "update")
            .expect_err("an invalid lockfile still fails");

        assert!(
            matches!(error, AppError::LockfileParse { .. }),
            "parse failures are not rewritten into a missing-lockfile refusal: {error}",
        );
    }

    #[test]
    fn defaulting_loader_reports_the_substitution_it_made() {
        let workspace = TempWorkspace::new("skillctl-lockfile-defaulted");

        let (lockfile, defaulted) =
            WorkspaceLockfile::load_from_workspace_or_default(workspace.path())
                .expect("an absent lockfile defaults instead of refusing");
        assert!(defaulted, "the substitution must be visible to the caller");
        assert_eq!(lockfile.imports.len(), 0);
        assert_eq!(
            lockfile.path,
            workspace.path().join(DEFAULT_LOCKFILE_PATH),
            "the default keeps the path the refusal would name",
        );

        workspace.write_lockfile(&minimal_lockfile());
        let (lockfile, defaulted) =
            WorkspaceLockfile::load_from_workspace_or_default(workspace.path())
                .expect("a present lockfile loads");
        assert!(!defaulted, "a present lockfile is not a substitution");
        assert_eq!(lockfile.imports.len(), 0);
    }

    fn context_in(working_directory: &Path) -> crate::app::AppContext {
        crate::app::AppContext {
            working_directory: working_directory.to_path_buf(),
            output_mode: crate::app::OutputMode::Json,
            verbosity: crate::app::Verbosity::Normal,
            interaction_mode: crate::app::InteractionMode::NonInteractive,
            selector: crate::app::CommandSelector::default(),
        }
    }

    /// Temporary directory a lockfile loader test reads from.
    struct TempWorkspace {
        path: PathBuf,
    }

    impl TempWorkspace {
        fn new(prefix: &str) -> Self {
            let unique = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time moved backwards")
                .as_nanos();
            let path =
                std::env::temp_dir().join(format!("{prefix}-{}-{unique}.d", std::process::id()));
            std::fs::create_dir_all(&path).expect("workspace directory exists");
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }

        fn write_lockfile(&self, contents: &str) {
            let path = self.path.join(DEFAULT_LOCKFILE_PATH);
            std::fs::create_dir_all(path.parent().expect("parent exists"))
                .expect("agents directory exists");
            std::fs::write(path, contents).expect("lockfile written");
        }
    }

    impl Drop for TempWorkspace {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}
