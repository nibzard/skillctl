//! Workspace manifest domain entry points.

use std::{
    collections::BTreeSet,
    fs, io,
    path::{Path, PathBuf},
};

use serde_json::json;

use crate::{
    app::AppContext, error::AppError, overlay::DEFAULT_OVERLAYS_DIR, response::AppResponse,
    skill::DEFAULT_SKILLS_DIR,
};

/// Default relative path to the workspace manifest.
pub const DEFAULT_MANIFEST_PATH: &str = ".agents/skillctl.yaml";

const DEFAULT_MANIFEST_CONTENT: &str = concat!(
    "version: 1\n",
    "\n",
    "targets:\n",
    "  - codex\n",
    "  - gemini-cli\n",
    "  - opencode\n",
);

const DEFAULT_GIT_EXCLUDE_PATH: &str = ".git/info/exclude";
const GENERATED_RUNTIME_ROOT_EXCLUDES: &[&str] = &[
    "/.claude/skills/",
    "/.github/skills/",
    "/.gemini/skills/",
    "/.opencode/skills/",
];

/// Placeholder model for the workspace manifest.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceManifest {
    /// Filesystem path to the manifest.
    pub path: PathBuf,
}

impl Default for WorkspaceManifest {
    fn default() -> Self {
        Self {
            path: PathBuf::from(DEFAULT_MANIFEST_PATH),
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
            fs::write(path, DEFAULT_MANIFEST_CONTENT).map_err(|source| {
                AppError::FilesystemOperation {
                    action: "write manifest",
                    path: path.to_path_buf(),
                    source,
                }
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
