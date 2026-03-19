//! Stable response types shared by the CLI, MCP server, and TUI presenters.

use serde::Serialize;
use serde_json::{Map, Value};

use crate::error::ExitStatus;

/// Stable command response contract.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct AppResponse {
    /// Whether the command completed successfully.
    pub ok: bool,
    /// Stable command identifier.
    pub command: &'static str,
    /// Non-fatal warnings.
    pub warnings: Vec<String>,
    /// Fatal or user-visible errors.
    pub errors: Vec<String>,
    /// Command-specific structured payload.
    pub data: Value,
    /// Optional human-readable summary for non-JSON output.
    #[serde(skip)]
    pub summary: Option<String>,
    /// Optional explicit process status override for diagnostics-heavy commands.
    #[serde(skip)]
    pub status_override: Option<ExitStatus>,
}

impl AppResponse {
    /// Create a successful response with an empty data payload.
    pub fn success(command: &'static str) -> Self {
        Self {
            ok: true,
            command,
            warnings: Vec::new(),
            errors: Vec::new(),
            data: Value::Object(Map::new()),
            summary: None,
            status_override: None,
        }
    }

    /// Create a failed response with a single error message.
    pub fn failure(command: &'static str, error: impl Into<String>) -> Self {
        Self {
            ok: false,
            command,
            warnings: Vec::new(),
            errors: vec![error.into()],
            data: Value::Object(Map::new()),
            summary: None,
            status_override: None,
        }
    }

    /// Attach a human-readable summary used by the terminal presenter.
    pub fn with_summary(mut self, summary: impl Into<String>) -> Self {
        self.summary = Some(summary.into());
        self
    }

    /// Attach a structured data payload.
    pub fn with_data(mut self, data: Value) -> Self {
        self.data = data;
        self
    }

    /// Override the process exit status while preserving the response payload.
    pub fn with_exit_status(mut self, status: ExitStatus) -> Self {
        self.status_override = Some(status);
        self
    }

    /// Attach a warning and preserve success semantics.
    pub fn with_warning(mut self, warning: impl Into<String>) -> Self {
        self.warnings.push(warning.into());
        self
    }

    /// Compute the exit status implied by the response content.
    pub const fn exit_status(&self) -> ExitStatus {
        if let Some(status) = self.status_override {
            return status;
        }
        if self.ok {
            if self.warnings.is_empty() {
                ExitStatus::Success
            } else {
                ExitStatus::SuccessWithWarnings
            }
        } else {
            ExitStatus::OperationalError
        }
    }
}

/// Rendered terminal output separated by stream.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RenderedOutput {
    /// Bytes destined for stdout.
    pub stdout: String,
    /// Bytes destined for stderr.
    pub stderr: String,
}
