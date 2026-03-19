//! Lockfile model, deterministic YAML IO, and state-version metadata.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::{
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
        }
    }

    /// Load the lockfile from the default workspace location.
    pub fn load_from_workspace(working_directory: &Path) -> Result<Self, AppError> {
        Self::load_from_path(working_directory.join(DEFAULT_LOCKFILE_PATH))
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
    pub fn from_yaml_str(path: impl Into<PathBuf>, contents: &str) -> Result<Self, AppError> {
        let path = path.into();
        let mut lockfile: Self =
            serde_yaml::from_str(contents).map_err(|source| AppError::LockfileParse {
                path: path.clone(),
                source,
            })?;
        lockfile.path = path;
        lockfile.validate()?;
        Ok(lockfile)
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

        fs::write(&self.path, contents).map_err(|source| AppError::FilesystemOperation {
            action: "write lockfile",
            path: self.path.clone(),
            source,
        })
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
        validate_schema_version(
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
    /// Resolved immutable revision data.
    pub revision: LockedRevision,
    /// Fetched and lifecycle timestamps.
    pub timestamps: LockedTimestamps,
    /// Stable hashes that identify the effective version.
    pub hashes: LockedHashes,
}

impl LockedImport {
    fn validate(&self, id: &str, lockfile_path: &Path) -> Result<(), AppError> {
        self.source.validate(id, lockfile_path)?;
        self.revision.validate(id, lockfile_path)?;
        self.timestamps.validate(id, lockfile_path)?;
        self.hashes.validate(id, lockfile_path)?;
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

/// Lifecycle timestamps captured for an imported skill.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LockedTimestamps {
    /// When the source contents were fetched.
    pub fetched_at: LockfileTimestamp,
    /// When the skill was first installed into this workspace.
    pub first_installed_at: LockfileTimestamp,
    /// When the skill was last updated in this workspace.
    pub last_updated_at: LockfileTimestamp,
}

impl LockedTimestamps {
    fn validate(&self, id: &str, lockfile_path: &Path) -> Result<(), AppError> {
        let prefix = format!("imports.{id}.timestamps");
        validate_timestamp(
            &format!("{prefix}.fetched_at"),
            &self.fetched_at,
            lockfile_path,
        )?;
        validate_timestamp(
            &format!("{prefix}.first_installed_at"),
            &self.first_installed_at,
            lockfile_path,
        )?;
        validate_timestamp(
            &format!("{prefix}.last_updated_at"),
            &self.last_updated_at,
            lockfile_path,
        )?;
        Ok(())
    }
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

/// UTC timestamp rendered in a deterministic RFC3339 shape.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct LockfileTimestamp(String);

impl LockfileTimestamp {
    /// Create a timestamp from a string.
    pub fn new(timestamp: impl Into<String>) -> Self {
        Self(timestamp.into())
    }

    /// Borrow the raw timestamp string.
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

fn validate_timestamp(
    field: &str,
    value: &LockfileTimestamp,
    lockfile_path: &Path,
) -> Result<(), AppError> {
    let value = value.as_str();
    validate_non_empty_trimmed(field, value, lockfile_path)?;

    let bytes = value.as_bytes();
    let matches_shape = bytes.len() == 20
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[10] == b'T'
        && bytes[13] == b':'
        && bytes[16] == b':'
        && bytes[19] == b'Z'
        && bytes.iter().enumerate().all(|(index, byte)| match index {
            4 | 7 | 10 | 13 | 16 | 19 => true,
            _ => byte.is_ascii_digit(),
        });

    if !matches_shape {
        return Err(lockfile_validation(
            lockfile_path,
            format!("{field} must use deterministic UTC RFC3339 timestamps, found '{value}'"),
        ));
    }

    Ok(())
}

fn lockfile_validation(path: &Path, message: impl Into<String>) -> AppError {
    AppError::LockfileValidation {
        path: path.to_path_buf(),
        message: message.into(),
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

    const MINIMAL_LOCKFILE: &str = concat!(
        "version: 1\n",
        "\n",
        "state:\n",
        "  manifest_version: 1\n",
        "  local_state_version: 2\n",
    );

    const FULL_LOCKFILE: &str = concat!(
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
        "      upstream: fedcba9876543210fedcba9876543210fedcba98\n",
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
            MINIMAL_LOCKFILE
        );
    }

    #[test]
    fn lockfile_roundtrips_with_stable_serialization() {
        let lockfile = WorkspaceLockfile::from_yaml_str(DEFAULT_LOCKFILE_PATH, FULL_LOCKFILE)
            .expect("lockfile parses");

        assert_eq!(
            lockfile
                .to_yaml_string()
                .expect("lockfile roundtrips deterministically"),
            FULL_LOCKFILE
        );
    }

    #[test]
    fn lockfile_validation_rejects_non_portable_subpaths() {
        let lockfile = concat!(
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
            "      subpath: ../skills/ai-sdk\n",
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

        let error = WorkspaceLockfile::from_yaml_str(DEFAULT_LOCKFILE_PATH, lockfile)
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
}
