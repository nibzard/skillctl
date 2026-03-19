//! Skill inventory, parsing, and inspection domain entry points.

use std::{
    collections::BTreeMap,
    fs, io,
    path::{Component, Path, PathBuf},
};

use serde_yaml::Value;

use crate::{app::AppContext, error::AppError, response::AppResponse};

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
    /// All parsed frontmatter fields, including vendor-specific extensions.
    pub fields: BTreeMap<String, Value>,
    /// Vendor-specific fields that should be passed through unchanged.
    pub vendor_fields: BTreeMap<String, Value>,
}

/// Vendor-specific metadata files associated with a skill directory.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SkillVendorMetadata {
    /// Raw file contents keyed by stable relative path under the skill root.
    pub files: BTreeMap<PathBuf, String>,
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
    /// Markdown body after the frontmatter block.
    pub body: String,
    /// Parsed frontmatter, including vendor-specific passthrough fields.
    pub frontmatter: SkillFrontmatter,
    /// Supported vendor-specific metadata files preserved alongside the skill.
    pub vendor_metadata: SkillVendorMetadata,
}

/// Parsed canonical workspace skill definition.
pub type WorkspaceSkill = SkillDefinition;

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
        let (frontmatter_source, body) = split_frontmatter_sections(source, &manifest_path)?;
        let fields = parse_frontmatter(&frontmatter_source, &manifest_path)?;

        validate_optional_standard_fields(&fields, &manifest_path)?;

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
            body,
            frontmatter: SkillFrontmatter {
                vendor_fields: extract_vendor_frontmatter(&fields),
                fields,
            },
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
pub fn handle_list(_context: &AppContext, _request: ListRequest) -> Result<AppResponse, AppError> {
    Err(AppError::NotYetImplemented { command: "list" })
}

/// Handle `skillctl remove`.
pub fn handle_remove(
    _context: &AppContext,
    _request: RemoveRequest,
) -> Result<AppResponse, AppError> {
    Err(AppError::NotYetImplemented { command: "remove" })
}

/// Handle `skillctl explain`.
pub fn handle_explain(
    _context: &AppContext,
    _request: ExplainRequest,
) -> Result<AppResponse, AppError> {
    Err(AppError::NotYetImplemented { command: "explain" })
}

/// Handle `skillctl enable`.
pub fn handle_enable(
    _context: &AppContext,
    _request: EnableRequest,
) -> Result<AppResponse, AppError> {
    Err(AppError::NotYetImplemented { command: "enable" })
}

/// Handle `skillctl disable`.
pub fn handle_disable(
    _context: &AppContext,
    _request: DisableRequest,
) -> Result<AppResponse, AppError> {
    Err(AppError::NotYetImplemented { command: "disable" })
}

/// Handle `skillctl path`.
pub fn handle_path(_context: &AppContext, _request: PathRequest) -> Result<AppResponse, AppError> {
    Err(AppError::NotYetImplemented { command: "path" })
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

fn split_frontmatter_sections(
    source: &str,
    skill_path: &Path,
) -> Result<(String, String), AppError> {
    let source = source.strip_prefix('\u{feff}').unwrap_or(source);
    let mut lines = source.lines();

    if lines.next() != Some("---") {
        return Err(skill_validation(
            skill_path,
            "SKILL.md must start with a '---' frontmatter delimiter",
        ));
    }

    let mut frontmatter_lines = Vec::new();
    let mut body_lines = Vec::new();
    let mut in_body = false;

    for line in lines {
        if !in_body && line == "---" {
            in_body = true;
            continue;
        }

        if in_body {
            body_lines.push(line);
        } else {
            frontmatter_lines.push(line);
        }
    }

    if !in_body {
        return Err(skill_validation(
            skill_path,
            "SKILL.md frontmatter must end with a closing '---' delimiter",
        ));
    }

    Ok((
        frontmatter_lines.join("\n"),
        body_lines.join("\n").trim().to_string(),
    ))
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
    if value.is_empty() {
        return Err(skill_validation(
            skill_path,
            "SKILL.md field 'name' must not be empty",
        ));
    }
    if value.len() > 64 {
        return Err(skill_validation(
            skill_path,
            format!(
                "SKILL.md field 'name' must be at most 64 characters, found {}",
                value.len()
            ),
        ));
    }
    if value.starts_with('-') || value.ends_with('-') || value.contains("--") {
        return Err(skill_validation(
            skill_path,
            format!(
                "SKILL.md field 'name' must use lowercase letters, digits, and single hyphens: '{value}'"
            ),
        ));
    }
    if !value
        .bytes()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(skill_validation(
            skill_path,
            format!(
                "SKILL.md field 'name' must use lowercase letters, digits, and hyphens only: '{value}'"
            ),
        ));
    }

    Ok(())
}

fn validate_description(value: &str, skill_path: &Path) -> Result<(), AppError> {
    if value.trim().is_empty() {
        return Err(skill_validation(
            skill_path,
            "SKILL.md field 'description' must not be empty",
        ));
    }
    if value.len() > 1_024 {
        return Err(skill_validation(
            skill_path,
            format!(
                "SKILL.md field 'description' must be at most 1024 characters, found {}",
                value.len()
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
                    root.display()
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

    Ok(())
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
            parsed.body,
            "# Release Notes\nUse this skill to summarize changes."
        );
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
