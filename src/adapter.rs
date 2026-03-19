//! Runtime adapter metadata shared by planners and diagnostics.

use serde::{Deserialize, Serialize};

/// Supported agent runtimes in the initial compatibility matrix.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum TargetRuntime {
    /// OpenAI Codex.
    Codex,
    /// Anthropic Claude Code.
    ClaudeCode,
    /// GitHub Copilot coding agent.
    GithubCopilot,
    /// Gemini CLI.
    GeminiCli,
    /// Sourcegraph Amp.
    Amp,
    /// OpenCode.
    Opencode,
}

impl TargetRuntime {
    /// Return the stable runtime identifier used in config and responses.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::ClaudeCode => "claude-code",
            Self::GithubCopilot => "github-copilot",
            Self::GeminiCli => "gemini-cli",
            Self::Amp => "amp",
            Self::Opencode => "opencode",
        }
    }
}

/// Registry placeholder for adapter definitions.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AdapterRegistry;
