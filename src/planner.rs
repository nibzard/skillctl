//! Planning domain types and update entry points.

use crate::{app::AppContext, error::AppError, response::AppResponse};

/// Placeholder projection plan.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ProjectionPlan;

/// Typed request for `skillctl update`.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct UpdateRequest {
    /// Optional managed skill filter.
    pub skill: Option<String>,
}

impl UpdateRequest {
    /// Create an update request from parsed CLI arguments.
    pub fn new(skill: Option<String>) -> Self {
        Self { skill }
    }
}

/// Handle `skillctl update`.
pub fn handle_update(
    _context: &AppContext,
    _request: UpdateRequest,
) -> Result<AppResponse, AppError> {
    Err(AppError::NotYetImplemented { command: "update" })
}
