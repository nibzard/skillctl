//! Skill inventory and inspection domain entry points.

use std::path::PathBuf;

use crate::{app::AppContext, error::AppError, response::AppResponse};

/// Default relative path to canonical workspace skills.
pub const DEFAULT_SKILLS_DIR: &str = ".agents/skills";

/// Strongly typed skill identifier.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkillName(pub String);

/// Placeholder definition for a canonical workspace skill.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceSkill {
    /// Identifier for the skill.
    pub name: SkillName,
    /// Filesystem path to the skill root.
    pub root: PathBuf,
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
