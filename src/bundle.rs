//! Canonical bundle format, hashing rules, and content verification.
//!
//! A canonical bundle is a versioned, sorted inventory of every file and
//! directory in one imported skill tree. The bundle digest computed from that
//! inventory is the only digest skillctl treats as the identity of supplied
//! content. It is recorded in the portable lockfile, used to key the immutable
//! local artifact store, and re-derived on every restore before installed
//! content is trusted.
//!
//! The format is a compatibility boundary. Changing any rule below changes
//! digest values and requires a new [`CANONICAL_BUNDLE_FORMAT_VERSION`] plus a
//! documented migration, never a silent reinterpretation of an existing digest.
//! The rules are specified in `docs/bundle-format.md`.
//!
//! Rules implemented here:
//!
//! * Paths are relative, UTF-8, and use `/` separators with no empty, `.`, or
//!   `..` segments, no drive letters, no backslashes, and no control
//!   characters. Windows-reserved device names are rejected.
//! * Entries are sorted by path bytes and carry a file digest, a size in bytes,
//!   and a mode of `file`, `executable`, or `directory`.
//! * Every distributed file is inventoried, not only `SKILL.md`. Transport
//!   metadata such as timestamps is never hashed.
//! * Content is hashed exactly as supplied. No line-ending, Unicode, or
//!   permission normalization happens after approval.
//! * Symlinks, hard links, device nodes, FIFOs, sockets, duplicate normalized
//!   paths, and case or Unicode-simple-fold path collisions are rejected.
//! * File count, entry count, total bytes, per-file bytes, path depth, path
//!   length, and segment length are bounded by [`BundleLimits`].

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Read,
    path::{Component, Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::AppError;

/// Current canonical bundle manifest format version.
pub const CANONICAL_BUNDLE_FORMAT_VERSION: u32 = 1;

/// Prefix carried by every canonical bundle digest.
pub const BUNDLE_DIGEST_PREFIX: &str = "sha256:";

/// Number of hexadecimal characters in a SHA-256 digest.
const SHA256_HEX_LENGTH: usize = 64;

/// Reserved MS-DOS device names that cannot be written portably.
const RESERVED_SEGMENT_NAMES: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Documented bounds applied while capturing a canonical bundle.
///
/// The defaults are part of the import contract described in
/// `docs/bundle-format.md`. They bound archive expansion, path depth, and
/// resource use before any content is hashed or trusted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BundleLimits {
    /// Maximum number of regular file entries in one bundle.
    pub max_file_entries: usize,
    /// Maximum number of directory entries in one bundle.
    pub max_directory_entries: usize,
    /// Maximum sum of all file sizes in one bundle.
    pub max_total_bytes: u64,
    /// Maximum size of any single file.
    pub max_file_bytes: u64,
    /// Maximum number of path segments below the bundle root.
    pub max_path_depth: usize,
    /// Maximum UTF-8 byte length of one full relative path.
    pub max_path_bytes: usize,
    /// Maximum UTF-8 byte length of one path segment.
    pub max_segment_bytes: usize,
}

impl Default for BundleLimits {
    fn default() -> Self {
        Self {
            max_file_entries: 4096,
            max_directory_entries: 4096,
            max_total_bytes: 64 * 1024 * 1024,
            max_file_bytes: 32 * 1024 * 1024,
            max_path_depth: 32,
            max_path_bytes: 1024,
            max_segment_bytes: 255,
        }
    }
}

impl BundleLimits {
    /// Reject a single file that exceeds the per-file or total size bound.
    fn check_file(
        &self,
        root: &Path,
        path: &str,
        size: u64,
        running_total: u64,
    ) -> Result<(), AppError> {
        if size > self.max_file_bytes {
            return Err(bundle_error(
                root,
                format!(
                    "file '{path}' is {size} bytes, which exceeds the {} byte per-file limit",
                    self.max_file_bytes
                ),
            ));
        }
        if running_total.saturating_add(size) > self.max_total_bytes {
            return Err(bundle_error(
                root,
                format!(
                    "bundle content exceeds the {} byte total size limit",
                    self.max_total_bytes
                ),
            ));
        }
        Ok(())
    }

    /// Reject a bundle whose entry counts exceed the documented bounds.
    fn check_counts(&self, root: &Path, files: usize, directories: usize) -> Result<(), AppError> {
        if files > self.max_file_entries {
            return Err(bundle_error(
                root,
                format!(
                    "bundle contains {files} files, which exceeds the {} file limit",
                    self.max_file_entries
                ),
            ));
        }
        if directories > self.max_directory_entries {
            return Err(bundle_error(
                root,
                format!(
                    "bundle contains {directories} directories, which exceeds the {} directory \
                     limit",
                    self.max_directory_entries
                ),
            ));
        }
        Ok(())
    }
}

/// Inventory mode of one canonical bundle entry.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum BundleEntryMode {
    /// Regular file without executable permission.
    File,
    /// Regular file with executable permission.
    Executable,
    /// Directory.
    Directory,
}

impl BundleEntryMode {
    /// Stable tag used inside the digest payload.
    const fn tag(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Executable => "executable",
            Self::Directory => "directory",
        }
    }
}

/// One sorted inventory entry of a canonical bundle.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BundleEntry {
    /// Normalized relative path with `/` separators.
    pub path: String,
    /// Inventory mode of the entry.
    pub mode: BundleEntryMode,
    /// `sha256:` digest of the file bytes. Directories carry no digest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
    /// Size of the file in bytes. Directories record zero.
    pub size: u64,
}

/// A captured canonical bundle manifest.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BundleManifest {
    /// Canonical bundle format version of this manifest.
    pub format_version: u32,
    /// Sorted inventory entries.
    pub entries: Vec<BundleEntry>,
}

/// Differences found when a tree is compared with a bundle manifest.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct BundleDiff {
    /// Entries pinned by the manifest that are absent on disk.
    pub missing: Vec<String>,
    /// Files present on disk that the manifest does not pin.
    pub unexpected: Vec<String>,
    /// Entries whose bytes hash to a different digest or size.
    pub modified: Vec<String>,
    /// Entries whose mode changed between the manifest and disk.
    pub mode_changed: Vec<String>,
}

impl BundleDiff {
    /// Whether the tree matches the manifest exactly.
    pub fn is_clean(&self) -> bool {
        self.missing.is_empty()
            && self.unexpected.is_empty()
            && self.modified.is_empty()
            && self.mode_changed.is_empty()
    }

    /// Human-readable summary of every difference.
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if !self.missing.is_empty() {
            parts.push(format!("missing: {}", self.missing.join(", ")));
        }
        if !self.unexpected.is_empty() {
            parts.push(format!("unexpected: {}", self.unexpected.join(", ")));
        }
        if !self.modified.is_empty() {
            parts.push(format!("modified: {}", self.modified.join(", ")));
        }
        if !self.mode_changed.is_empty() {
            parts.push(format!("mode changed: {}", self.mode_changed.join(", ")));
        }
        if parts.is_empty() {
            "content matches the pinned bundle".to_string()
        } else {
            parts.join("; ")
        }
    }
}

impl BundleManifest {
    /// Capture the canonical bundle of one tree using the default limits.
    pub fn capture(root: &Path) -> Result<Self, AppError> {
        Self::capture_with_limits(root, &BundleLimits::default())
    }

    /// Capture the canonical bundle of one tree under explicit limits.
    pub fn capture_with_limits(root: &Path, limits: &BundleLimits) -> Result<Self, AppError> {
        let metadata = fs::metadata(root).map_err(|source| AppError::FilesystemOperation {
            action: "inspect bundle root",
            path: root.to_path_buf(),
            source,
        })?;
        if !metadata.is_dir() {
            return Err(AppError::PathConflict {
                path: root.to_path_buf(),
                expected: "directory",
            });
        }

        let mut relatives = Vec::new();
        collect_tree(root, root, &mut relatives, limits)?;

        let mut entries = Vec::with_capacity(relatives.len());
        let mut total_bytes = 0_u64;
        let mut directories = 0_usize;
        let mut files = 0_usize;

        for relative in relatives {
            let path = normalize_relative_path(&relative)
                .map_err(|message| bundle_error(root, message))?;
            let full_path = root.join(&relative);
            let entry_metadata = fs::symlink_metadata(&full_path).map_err(|source| {
                AppError::FilesystemOperation {
                    action: "inspect bundle entry",
                    path: full_path.clone(),
                    source,
                }
            })?;

            if entry_metadata.is_dir() {
                directories += 1;
                entries.push(BundleEntry {
                    path,
                    mode: BundleEntryMode::Directory,
                    digest: None,
                    size: 0,
                });
                continue;
            }

            let size = entry_metadata.len();
            total_bytes = total_bytes.saturating_add(size);
            limits.check_file(root, &path, size, total_bytes)?;
            files += 1;
            entries.push(BundleEntry {
                path,
                mode: if is_executable(&entry_metadata) {
                    BundleEntryMode::Executable
                } else {
                    BundleEntryMode::File
                },
                digest: Some(hash_file(&full_path)?),
                size,
            });
        }

        entries.sort_by(|left, right| left.path.cmp(&right.path));
        limits.check_counts(root, files, directories)?;
        let manifest = Self {
            format_version: CANONICAL_BUNDLE_FORMAT_VERSION,
            entries,
        };
        manifest.validate(root)?;
        Ok(manifest)
    }

    /// Compute the canonical bundle digest of this manifest.
    pub fn digest(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.canonical_payload());
        format!("{BUNDLE_DIGEST_PREFIX}{:x}", hasher.finalize())
    }

    /// Deterministic byte payload covered by the bundle digest.
    fn canonical_payload(&self) -> Vec<u8> {
        let mut payload = Vec::new();
        payload.extend_from_slice(
            format!(
                "skillctl-bundle-v{}\nentries:{}\n",
                self.format_version,
                self.entries.len()
            )
            .as_bytes(),
        );
        for entry in &self.entries {
            payload.extend_from_slice(entry.path.as_bytes());
            payload.push(0);
            payload.extend_from_slice(entry.mode.tag().as_bytes());
            payload.push(0);
            payload.extend_from_slice(entry.digest.as_deref().unwrap_or("").as_bytes());
            payload.push(0);
            payload.extend_from_slice(entry.size.to_string().as_bytes());
            payload.push(0);
            payload.push(b'\n');
        }
        payload
    }

    /// Total size of all regular files in the bundle.
    pub fn total_bytes(&self) -> u64 {
        self.entries.iter().map(|entry| entry.size).sum()
    }

    /// Number of regular files in the bundle.
    pub fn file_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|entry| entry.mode != BundleEntryMode::Directory)
            .count()
    }

    /// Serialize the manifest as deterministic JSON.
    pub fn to_json_string(&self) -> Result<String, AppError> {
        serde_json::to_string_pretty(self).map_err(|source| AppError::BundleSerialization {
            message: source.to_string(),
        })
    }

    /// Parse and validate a stored bundle manifest.
    pub fn from_json_str(contents: &str) -> Result<Self, AppError> {
        let manifest: Self =
            serde_json::from_str(contents).map_err(|source| AppError::BundleValidation {
                message: format!("bundle manifest is not valid JSON: {source}"),
            })?;
        manifest.validate(Path::new("<bundle-manifest>"))?;
        Ok(manifest)
    }

    /// Validate a manifest that was captured or loaded from storage.
    pub fn validate(&self, root: &Path) -> Result<(), AppError> {
        if self.format_version == 0 || self.format_version > CANONICAL_BUNDLE_FORMAT_VERSION {
            return Err(bundle_error(
                root,
                format!(
                    "bundle format version {} is not supported; this skillctl supports version \
                     {CANONICAL_BUNDLE_FORMAT_VERSION}",
                    self.format_version
                ),
            ));
        }

        let mut seen = BTreeSet::new();
        let mut folded = BTreeSet::new();
        let mut previous: Option<&str> = None;

        for entry in &self.entries {
            let expected = normalize_relative_path(Path::new(&entry.path))
                .map_err(|message| bundle_error(root, message))?;
            if expected != entry.path {
                return Err(bundle_error(
                    root,
                    format!("bundle path '{}' is not in normalized form", entry.path),
                ));
            }
            if let Some(previous) = previous
                && previous >= entry.path.as_str()
            {
                return Err(bundle_error(
                    root,
                    format!(
                        "bundle entries must be sorted by path; '{}' follows '{}'",
                        entry.path, previous
                    ),
                ));
            }
            previous = Some(entry.path.as_str());

            if !seen.insert(entry.path.clone()) {
                return Err(bundle_error(
                    root,
                    format!("bundle contains the duplicate path '{}'", entry.path),
                ));
            }
            if !folded.insert(entry.path.to_lowercase()) {
                return Err(bundle_error(
                    root,
                    format!(
                        "bundle path '{}' collides with another entry after case folding",
                        entry.path
                    ),
                ));
            }

            match entry.mode {
                BundleEntryMode::Directory => {
                    if entry.digest.is_some() {
                        return Err(bundle_error(
                            root,
                            format!("directory entry '{}' must not carry a digest", entry.path),
                        ));
                    }
                    if entry.size != 0 {
                        return Err(bundle_error(
                            root,
                            format!("directory entry '{}' must record zero bytes", entry.path),
                        ));
                    }
                }
                BundleEntryMode::File | BundleEntryMode::Executable => {
                    let Some(digest) = entry.digest.as_deref() else {
                        return Err(bundle_error(
                            root,
                            format!("file entry '{}' must carry a digest", entry.path),
                        ));
                    };
                    if !is_canonical_digest(digest) {
                        return Err(bundle_error(
                            root,
                            format!("file entry '{}' carries a malformed digest", entry.path),
                        ));
                    }
                }
            }
        }

        Ok(())
    }

    /// Compare one tree against this manifest without modifying anything.
    pub fn diff_tree(&self, root: &Path) -> Result<BundleDiff, AppError> {
        let observed = Self::capture(root)?;
        let pinned: BTreeMap<&str, &BundleEntry> = self
            .entries
            .iter()
            .map(|entry| (entry.path.as_str(), entry))
            .collect();
        let actual: BTreeMap<&str, &BundleEntry> = observed
            .entries
            .iter()
            .map(|entry| (entry.path.as_str(), entry))
            .collect();

        let mut diff = BundleDiff::default();
        for (path, pinned_entry) in &pinned {
            match actual.get(path) {
                None => diff.missing.push((*path).to_string()),
                Some(actual_entry) => {
                    if pinned_entry.mode != actual_entry.mode {
                        diff.mode_changed.push((*path).to_string());
                    }
                    if pinned_entry.digest != actual_entry.digest
                        || pinned_entry.size != actual_entry.size
                    {
                        diff.modified.push((*path).to_string());
                    }
                }
            }
        }
        for path in actual.keys() {
            if !pinned.contains_key(path) {
                diff.unexpected.push((*path).to_string());
            }
        }

        Ok(diff)
    }
}

/// Collect every relative path below `root`, rejecting unsupported entry types.
fn collect_tree(
    root: &Path,
    current: &Path,
    relatives: &mut Vec<PathBuf>,
    limits: &BundleLimits,
) -> Result<(), AppError> {
    let depth = match current.strip_prefix(root) {
        Ok(relative) => relative.components().count(),
        Err(_) => 0,
    };
    if depth > limits.max_path_depth {
        return Err(bundle_error(
            root,
            format!(
                "bundle path depth exceeds the {} segment limit",
                limits.max_path_depth
            ),
        ));
    }

    let mut entries = fs::read_dir(current)
        .map_err(|source| AppError::FilesystemOperation {
            action: "read bundle directory",
            path: current.to_path_buf(),
            source,
        })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| AppError::FilesystemOperation {
            action: "read bundle directory entry",
            path: current.to_path_buf(),
            source,
        })?;
    entries.sort_by_key(std::fs::DirEntry::file_name);

    for entry in entries {
        let path = entry.path();
        let metadata =
            fs::symlink_metadata(&path).map_err(|source| AppError::FilesystemOperation {
                action: "inspect bundle entry",
                path: path.clone(),
                source,
            })?;
        let file_type = metadata.file_type();

        if file_type.is_symlink() {
            return Err(bundle_error(
                root,
                format!(
                    "symlink '{}' is not supported inside a canonical bundle",
                    path.display()
                ),
            ));
        }
        if file_type.is_dir() {
            relatives.push(
                path.strip_prefix(root)
                    .map_err(|_| AppError::BundleValidation {
                        message: format!(
                            "path '{}' escaped the bundle root '{}'",
                            path.display(),
                            root.display()
                        ),
                    })?
                    .to_path_buf(),
            );
            collect_tree(root, &path, relatives, limits)?;
            continue;
        }
        if !file_type.is_file() {
            return Err(bundle_error(
                root,
                format!(
                    "entry '{}' is not a regular file or directory and cannot be bundled",
                    path.display()
                ),
            ));
        }
        if hard_linked(&metadata) {
            return Err(bundle_error(
                root,
                format!(
                    "hard-linked file '{}' cannot be bundled; hard links hide independent \
                     content changes",
                    path.display()
                ),
            ));
        }
        relatives.push(
            path.strip_prefix(root)
                .map_err(|_| AppError::BundleValidation {
                    message: format!(
                        "path '{}' escaped the bundle root '{}'",
                        path.display(),
                        root.display()
                    ),
                })?
                .to_path_buf(),
        );
    }

    Ok(())
}

/// Normalize one relative path into its canonical bundle form.
pub fn normalize_relative_path(path: &Path) -> Result<String, String> {
    let mut segments = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(segment) => {
                let segment = segment
                    .to_str()
                    .ok_or_else(|| format!("path '{}' is not valid UTF-8", path.display()))?;
                segments.push(segment);
            }
            _ => {
                return Err(format!(
                    "path '{}' must remain a relative path without '.', '..', or root segments",
                    path.display()
                ));
            }
        }
    }

    if segments.is_empty() {
        return Err("bundle paths must not be empty".to_string());
    }

    let mut bytes = 0_usize;
    for segment in &segments {
        if segment.is_empty() {
            return Err(format!(
                "path '{}' contains an empty segment",
                path.display()
            ));
        }
        if segment.ends_with('.') || segment.ends_with(' ') {
            return Err(format!(
                "path segment '{segment}' ends in '.' or a space, which is not portable"
            ));
        }
        if segment
            .chars()
            .any(|character| character.is_control() || matches!(character, '\\' | ':' | '?'))
        {
            return Err(format!(
                "path segment '{segment}' contains a character that is not portable across \
                 supported filesystems"
            ));
        }
        if RESERVED_SEGMENT_NAMES.contains(&segment.to_ascii_uppercase().as_str()) {
            return Err(format!(
                "path segment '{segment}' is a reserved device name on Windows"
            ));
        }
        let segment_bytes = segment.len();
        if segment_bytes > 255 {
            return Err(format!(
                "path segment '{segment}' is {segment_bytes} bytes, which exceeds the 255 byte \
                 segment limit"
            ));
        }
        bytes += segment_bytes + 1;
    }

    if bytes > 1024 {
        return Err(format!(
            "path '{}' is {bytes} bytes, which exceeds the 1024 byte path limit",
            path.display()
        ));
    }

    Ok(segments.join("/"))
}

/// Hash one regular file with SHA-256 and return the canonical digest string.
fn hash_file(path: &Path) -> Result<String, AppError> {
    let mut file = fs::File::open(path).map_err(|source| AppError::FilesystemOperation {
        action: "open file for bundle hashing",
        path: path.to_path_buf(),
        source,
    })?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|source| AppError::FilesystemOperation {
                action: "read file for bundle hashing",
                path: path.to_path_buf(),
                source,
            })?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }

    Ok(format!("{BUNDLE_DIGEST_PREFIX}{:x}", hasher.finalize()))
}

/// Whether the platform records an executable permission bit.
fn is_executable(metadata: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        false
    }
}

/// Whether a regular file has more than one hard link.
fn hard_linked(metadata: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        metadata.nlink() > 1
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        false
    }
}

/// Whether a string is a well-formed `sha256:<64 hex>` digest.
pub fn is_canonical_digest(value: &str) -> bool {
    let Some(hex) = value.strip_prefix(BUNDLE_DIGEST_PREFIX) else {
        return false;
    };
    hex.len() == SHA256_HEX_LENGTH && hex.chars().all(|character| character.is_ascii_hexdigit())
}

/// Directory name of one digest inside the content-addressed artifact store.
pub fn artifact_directory_name(digest: &str) -> Result<&str, AppError> {
    let name =
        digest
            .strip_prefix(BUNDLE_DIGEST_PREFIX)
            .ok_or_else(|| AppError::BundleValidation {
                message: format!("digest '{digest}' is not a canonical sha256 digest"),
            })?;
    if !is_canonical_digest(digest) {
        return Err(AppError::BundleValidation {
            message: format!("digest '{digest}' is not a canonical sha256 digest"),
        });
    }
    Ok(name)
}

fn bundle_error(root: &Path, message: impl Into<String>) -> AppError {
    AppError::BundleValidation {
        message: format!("{}: {}", root.display(), message.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    struct BundleFixture {
        root: PathBuf,
    }

    impl BundleFixture {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "skillctl-bundle-{}-{name}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("time moved backwards")
                    .as_nanos()
            ));
            fs::create_dir_all(&root).expect("fixture root exists");
            Self { root }
        }

        fn write(&self, relative: &str, contents: &str) -> PathBuf {
            let path = self.root.join(relative);
            fs::create_dir_all(path.parent().expect("parent exists"))
                .expect("parent directory exists");
            fs::write(&path, contents).expect("file written");
            path
        }

        fn manifest(&self) -> BundleManifest {
            BundleManifest::capture(&self.root).expect("bundle captures")
        }
    }

    impl Drop for BundleFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn bundle_capture_is_deterministic_and_order_independent() {
        let first = BundleFixture::new("deterministic");
        first.write("SKILL.md", "# skill\n");
        first.write("references/alpha.md", "alpha\n");
        first.write("references/beta.md", "beta\n");

        let second = BundleFixture::new("deterministic");
        second.write("references/beta.md", "beta\n");
        second.write("SKILL.md", "# skill\n");
        second.write("references/alpha.md", "alpha\n");

        let first_manifest = first.manifest();
        assert_eq!(first_manifest.digest(), second.manifest().digest());
        assert_eq!(first_manifest.file_count(), 3);
        assert_eq!(first_manifest.total_bytes(), 19);
        assert_eq!(
            first_manifest
                .entries
                .iter()
                .map(|entry| entry.path.as_str())
                .collect::<Vec<_>>(),
            vec![
                "SKILL.md",
                "references",
                "references/alpha.md",
                "references/beta.md"
            ]
        );
    }

    #[test]
    fn bundle_digest_covers_paths_modes_sizes_and_content() {
        let base = BundleFixture::new("sensitivity");
        base.write("SKILL.md", "one\n");
        let baseline = base.manifest().digest();

        let renamed = BundleFixture::new("sensitivity");
        renamed.write("OTHER.md", "one\n");
        assert_ne!(baseline, renamed.manifest().digest(), "path is covered");

        let resized = BundleFixture::new("sensitivity");
        resized.write("SKILL.md", "one\ntwo\n");
        assert_ne!(baseline, resized.manifest().digest(), "content is covered");

        let empty_directory = BundleFixture::new("sensitivity");
        empty_directory.write("nested/.keep", "x\n");
        assert_ne!(baseline, empty_directory.manifest().digest());
    }

    #[test]
    fn executable_files_are_recorded_in_the_inventory() {
        let fixture = BundleFixture::new("executable");
        let script = fixture.write("scripts/run.sh", "#!/bin/sh\nexit 0\n");
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755))
            .expect("executable bit set");

        let manifest = fixture.manifest();
        let entry = manifest
            .entries
            .iter()
            .find(|entry| entry.path == "scripts/run.sh")
            .expect("script is inventoried");
        assert_eq!(entry.mode, BundleEntryMode::Executable);
        assert!(manifest.digest().starts_with(BUNDLE_DIGEST_PREFIX));

        fs::set_permissions(&script, fs::Permissions::from_mode(0o644))
            .expect("executable bit cleared");
        let cleared = fixture.manifest();
        assert_ne!(
            manifest.digest(),
            cleared.digest(),
            "permission changes must change the bundle digest"
        );
    }

    #[test]
    fn empty_directories_are_part_of_the_inventory() {
        let fixture = BundleFixture::new("empty-directory");
        fs::create_dir_all(fixture.root.join("references/empty")).expect("directory exists");
        fixture.write("SKILL.md", "# skill\n");

        let manifest = fixture.manifest();
        assert!(
            manifest
                .entries
                .iter()
                .any(|entry| entry.path == "references/empty"
                    && entry.mode == BundleEntryMode::Directory)
        );
    }

    #[test]
    fn timestamp_changes_do_not_change_the_bundle_digest() {
        let fixture = BundleFixture::new("timestamps");
        let path = fixture.write("SKILL.md", "# skill\n");
        let before = fixture.manifest().digest();

        let old = std::time::SystemTime::UNIX_EPOCH;
        let file = fs::File::options()
            .write(true)
            .open(&path)
            .expect("file opens");
        file.set_modified(old).expect("mtime reset");

        assert_eq!(
            before,
            fixture.manifest().digest(),
            "transport timestamps are not part of source identity"
        );
    }

    #[test]
    fn symlinks_and_hard_links_are_rejected() {
        let symlinked = BundleFixture::new("symlink");
        symlinked.write("SKILL.md", "# skill\n");
        #[cfg(unix)]
        std::os::unix::fs::symlink(
            symlinked.root.join("SKILL.md"),
            symlinked.root.join("linked.md"),
        )
        .expect("symlink created");

        let error = BundleManifest::capture(&symlinked.root)
            .expect_err("symlinks are rejected inside a bundle");
        assert!(
            error.to_string().contains("symlink"),
            "unexpected error: {error}"
        );

        let hardlinked = BundleFixture::new("hardlink");
        let original = hardlinked.write("SKILL.md", "# skill\n");
        fs::hard_link(&original, hardlinked.root.join("copy.md")).expect("hard link created");
        let error = BundleManifest::capture(&hardlinked.root)
            .expect_err("hard links are rejected inside a bundle");
        assert!(
            error.to_string().contains("hard-linked"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn case_and_unicode_folds_are_rejected_as_collisions() {
        let fixture = BundleFixture::new("case-fold");
        fixture.write("SKILL.md", "# skill\n");
        fixture.write("Reference/alpha.md", "alpha\n");
        fixture.write("reference/ALPHA.md", "alpha\n");

        let error = BundleManifest::capture(&fixture.root)
            .expect_err("case-folded collisions are rejected");
        assert!(
            error
                .to_string()
                .contains("collides with another entry after case folding"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn nonportable_paths_are_rejected() {
        for path in ["../escape.md", "trailing ", "CON", "a?b"] {
            assert!(
                normalize_relative_path(Path::new(path)).is_err(),
                "path '{path}' must be rejected"
            );
        }
        assert!(normalize_relative_path(Path::new("a/b/c.md")).is_ok());
    }

    #[test]
    fn documented_limits_are_enforced() {
        let limits = BundleLimits {
            max_file_entries: 2,
            max_file_bytes: 4,
            max_total_bytes: 8,
            ..BundleLimits::default()
        };

        let fixture = BundleFixture::new("limits");
        fixture.write("SKILL.md", "# skill\n");

        let error = BundleManifest::capture_with_limits(&fixture.root, &limits)
            .expect_err("per-file limit is enforced");
        assert!(
            error.to_string().contains("per-file limit"),
            "unexpected error: {error}"
        );

        let fixture = BundleFixture::new("limits-count");
        fixture.write("a", "1");
        fixture.write("b", "2");
        fixture.write("c", "3");
        let error = BundleManifest::capture_with_limits(&fixture.root, &limits)
            .expect_err("file count limit is enforced");
        assert!(
            error.to_string().contains("file limit"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn manifest_json_round_trips_and_rejects_unsorted_entries() {
        let fixture = BundleFixture::new("roundtrip");
        fixture.write("SKILL.md", "# skill\n");
        fixture.write("references/alpha.md", "alpha\n");
        let manifest = fixture.manifest();

        let encoded = manifest.to_json_string().expect("manifest serializes");
        let decoded = BundleManifest::from_json_str(&encoded).expect("manifest parses");
        assert_eq!(manifest, decoded);
        assert_eq!(manifest.digest(), decoded.digest());

        let mut reordered = decoded;
        reordered.entries.reverse();
        let encoded = reordered.to_json_string().expect("manifest serializes");
        let error =
            BundleManifest::from_json_str(&encoded).expect_err("unsorted entries are rejected");
        assert!(
            error.to_string().contains("sorted by path"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn diff_tree_reports_missing_unexpected_modified_and_mode_changes() {
        let fixture = BundleFixture::new("diff");
        fixture.write("SKILL.md", "# skill\n");
        let script = fixture.write("scripts/run.sh", "#!/bin/sh\nexit 0\n");
        let manifest = fixture.manifest();

        assert!(manifest.diff_tree(&fixture.root).expect("diff").is_clean());

        fs::write(fixture.root.join("SKILL.md"), "# changed\n").expect("content changed");
        fs::remove_file(&script).expect("script removed");
        fixture.write("extra.md", "extra\n");
        let diff = manifest.diff_tree(&fixture.root).expect("diff");
        assert_eq!(diff.modified, vec!["SKILL.md".to_string()]);
        assert_eq!(diff.missing, vec!["scripts/run.sh".to_string()]);
        assert_eq!(diff.unexpected, vec!["extra.md".to_string()]);

        let fixture = BundleFixture::new("diff-mode");
        let script = fixture.write("scripts/run.sh", "#!/bin/sh\nexit 0\n");
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755))
            .expect("executable bit set");
        let manifest = fixture.manifest();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o644))
            .expect("executable bit cleared");
        let diff = manifest.diff_tree(&fixture.root).expect("diff");
        assert_eq!(diff.mode_changed, vec!["scripts/run.sh".to_string()]);
    }

    #[test]
    fn digest_strings_are_validated() {
        assert!(is_canonical_digest(
            "sha256:a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2"
        ));
        assert!(!is_canonical_digest("a1b2"));
        assert!(!is_canonical_digest("md5:a1b2"));
        assert!(
            artifact_directory_name(
                "sha256:a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2"
            )
            .is_ok()
        );
    }
    /// Pins the committed fixture digest, which is the hash-format
    /// compatibility boundary described in `tests/fixtures/bundle/README.md`.
    ///
    /// The value was reproduced independently from the documented payload rules,
    /// so a change here means the format changed, not just the implementation.
    #[test]
    fn committed_fixture_digest_is_pinned() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/bundle/canonical");
        let manifest = BundleManifest::capture(&fixture).expect("fixture bundle captures");

        assert_eq!(
            manifest
                .entries
                .iter()
                .map(|entry| entry.path.as_str())
                .collect::<Vec<_>>(),
            vec![
                "SKILL.md",
                "references",
                "references/notes.md",
                "scripts",
                "scripts/run.sh",
            ]
        );
        assert_eq!(
            manifest
                .entries
                .iter()
                .find(|entry| entry.path == "scripts/run.sh")
                .expect("script is inventoried")
                .mode,
            BundleEntryMode::Executable,
            "the executable bit is part of the inventory"
        );
        assert_eq!(
            manifest.digest(),
            "sha256:0766869ba27a516348be813c39a52a8ca862ce7338708117a1b0044e9d86bc0f"
        );
    }
}
