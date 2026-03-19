//! Source detection, normalization, staging, and install inspection.

use std::{
    ffi::OsStr,
    fs::{self, File},
    io::{self, Read},
    path::{Path, PathBuf},
    process::Command,
};

use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use url::Url;

use crate::{
    adapter::{AdapterRegistry, TargetRuntime, TargetScope},
    app::AppContext,
    error::AppError,
    response::AppResponse,
    skill::SkillDefinition,
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

/// Inspect, normalize, and stage an install source before later lifecycle steps.
pub fn inspect_install_source(
    working_directory: &Path,
    request: &InstallRequest,
) -> Result<InstallInspection, AppError> {
    let resolved = resolve_install_source(&request.source, working_directory)?;
    let prepared = prepare_source(&resolved)?;
    let candidates = detect_install_candidates(&prepared.root, &prepared.source.raw)?;

    Ok(InstallInspection {
        source: prepared.source,
        revision: prepared.revision,
        candidates,
    })
}

/// Handle `skillctl install`.
pub fn handle_install(
    context: &AppContext,
    request: InstallRequest,
) -> Result<AppResponse, AppError> {
    let inspection = inspect_install_source(&context.working_directory, &request)?;
    let candidate_count = inspection.candidates.len();
    let plural = if candidate_count == 1 { "" } else { "s" };
    let summary = format!(
        "Detected {candidate_count} install candidate{plural} from {} at {}",
        inspection.source.display, inspection.revision.resolved
    );

    Ok(AppResponse::success("install")
        .with_summary(summary)
        .with_data(json!(inspection)))
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
    let path = source
        .local_path
        .as_ref()
        .expect("local path sources always carry a path")
        .clone();

    Ok(PreparedSource {
        source: source.normalized.clone(),
        revision: SourceRevision {
            resolved: hash_directory_contents(&path)?,
            upstream: None,
        },
        root: path,
        _staging_dir: None,
    })
}

fn prepare_archive(source: &ResolvedInstallSource) -> Result<PreparedSource, AppError> {
    let archive_path = source
        .local_path
        .as_ref()
        .expect("archive sources always carry a path")
        .clone();
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

    extract_archive(&archive_path, &extract_root, &source.normalized.raw)?;
    let root = normalized_extraction_root(&extract_root)?;

    Ok(PreparedSource {
        source: source.normalized.clone(),
        revision: SourceRevision {
            resolved: hash_file_contents(&archive_path)?,
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
            candidates.push(InstallCandidate {
                display_name: skill.name.as_str().to_string(),
                name: skill.name.as_str().to_string(),
                source_path: selected_subpath.clone(),
                selected_subpath,
                compatible_targets: detection_root.compatible_targets.to_vec(),
                compatibility_hints: build_compatibility_hints(detection_root),
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
            &extract_root.display().to_string(),
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

fn hash_directory_contents(root: &Path) -> Result<String, AppError> {
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
            .expect("path is under root")
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
