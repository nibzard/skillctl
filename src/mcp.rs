//! MCP bridge domain entry points.

use crate::{app::AppContext, cli::McpCommand, error::AppError, response::AppResponse};

/// Placeholder MCP tool identifier.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum McpTool {
    /// Start the server process.
    Serve,
}

/// Handle the `skillctl mcp` command family.
pub fn handle_command(
    _context: &AppContext,
    command: &McpCommand,
) -> Result<AppResponse, AppError> {
    match command {
        McpCommand::Serve => Err(AppError::NotYetImplemented {
            command: "mcp-serve",
        }),
    }
}
