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

    /// Return the runtimes supported by the initial registry in stable order.
    pub const fn all() -> &'static [Self; 6] {
        &[
            Self::Codex,
            Self::ClaudeCode,
            Self::GithubCopilot,
            Self::GeminiCli,
            Self::Amp,
            Self::Opencode,
        ]
    }
}

/// Runtime scope supported by adapter discovery roots.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum TargetScope {
    /// Workspace-local runtime scope.
    Workspace,
    /// User-wide runtime scope.
    User,
}

/// Runtime install-mode risk used when selecting projection modes.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum InstallModeRisk {
    /// Copy mode is safe, but symlink behavior is not trusted by default.
    SymlinkUnstable,
}

/// A documented runtime discovery root plus planner preference ranks.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct DiscoveryRoot {
    /// Scope where the root is discovered.
    pub scope: TargetScope,
    /// Root path exactly as documented by the spec.
    pub path: &'static str,
    /// Lower rank wins when the planner favors neutral roots.
    pub prefer_neutral_rank: u8,
    /// Lower rank wins when the planner favors native roots.
    pub prefer_native_rank: u8,
}

impl DiscoveryRoot {
    const fn new(
        scope: TargetScope,
        path: &'static str,
        prefer_neutral_rank: u8,
        prefer_native_rank: u8,
    ) -> Self {
        Self {
            scope,
            path,
            prefer_neutral_rank,
            prefer_native_rank,
        }
    }
}

/// Static runtime adapter metadata used by planning and diagnostics.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct AdapterMetadata {
    /// Runtime identifier.
    pub target: TargetRuntime,
    /// Scopes documented by the runtime.
    pub supported_scopes: &'static [TargetScope],
    /// Discovery roots across all documented scopes.
    pub discovery_roots: &'static [DiscoveryRoot],
    /// Projection-mode risk used by materialization and doctor flows.
    pub install_mode_risk: InstallModeRisk,
    /// Date the recorded facts were last verified against vendor documentation.
    pub verified_on: &'static str,
    /// Vendor documentation pages that back the recorded discovery roots.
    pub documentation_sources: &'static [&'static str],
}

impl AdapterMetadata {
    /// Return whether the adapter supports the requested scope.
    pub fn supports_scope(&self, scope: TargetScope) -> bool {
        self.supported_scopes.contains(&scope)
    }

    /// Return the documented roots for one scope in stable declaration order.
    pub fn roots_for_scope(&self, scope: TargetScope) -> Vec<&'static DiscoveryRoot> {
        self.discovery_roots
            .iter()
            .filter(|root| root.scope == scope)
            .collect()
    }
}

/// Static registry of supported runtime adapters.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AdapterRegistry;

impl AdapterRegistry {
    /// Create a registry handle for the built-in runtime adapters.
    pub const fn new() -> Self {
        Self
    }

    /// Return every supported adapter in stable runtime order.
    pub fn all(&self) -> &'static [AdapterMetadata] {
        ALL_ADAPTERS
    }

    /// Lookup metadata for a supported runtime.
    pub fn get(&self, target: TargetRuntime) -> &'static AdapterMetadata {
        match target {
            TargetRuntime::Codex => &CODEX_ADAPTER,
            TargetRuntime::ClaudeCode => &CLAUDE_CODE_ADAPTER,
            TargetRuntime::GithubCopilot => &GITHUB_COPILOT_ADAPTER,
            TargetRuntime::GeminiCli => &GEMINI_CLI_ADAPTER,
            TargetRuntime::Amp => &AMP_ADAPTER,
            TargetRuntime::Opencode => &OPENCODE_ADAPTER,
        }
    }
}

const BOTH_SCOPES: &[TargetScope] = &[TargetScope::Workspace, TargetScope::User];

/// Date the adapter facts were last verified against the listed sources.
const VERIFIED_ON: &str = "2026-11-05";
const CODEX_SOURCES: &[&str] = &["https://developers.openai.com/codex/skills"];
const CLAUDE_CODE_SOURCES: &[&str] = &["https://code.claude.com/docs/en/skills"];
const GITHUB_COPILOT_SOURCES: &[&str] = &[
    "https://docs.github.com/en/copilot/concepts/agents/about-agent-skills",
    "https://docs.github.com/en/copilot/how-tos/use-copilot-agents/coding-agent/create-skills",
];
const GEMINI_CLI_SOURCES: &[&str] = &["https://geminicli.com/docs/cli/skills/"];
const AMP_SOURCES: &[&str] = &["https://ampcode.com/docs/customize/skills"];
const OPENCODE_SOURCES: &[&str] = &["https://opencode.ai/docs/skills"];

const CODEX_ROOTS: &[DiscoveryRoot] = &[
    DiscoveryRoot::new(TargetScope::Workspace, ".agents/skills", 0, 0),
    DiscoveryRoot::new(TargetScope::User, "~/.agents/skills", 0, 0),
];
const CLAUDE_CODE_ROOTS: &[DiscoveryRoot] = &[
    DiscoveryRoot::new(TargetScope::Workspace, ".claude/skills", 0, 0),
    DiscoveryRoot::new(TargetScope::User, "~/.claude/skills", 0, 0),
];
const GITHUB_COPILOT_ROOTS: &[DiscoveryRoot] = &[
    DiscoveryRoot::new(TargetScope::Workspace, ".github/skills", 0, 0),
    DiscoveryRoot::new(TargetScope::Workspace, ".claude/skills", 1, 1),
    DiscoveryRoot::new(TargetScope::Workspace, ".agents/skills", 2, 2),
    DiscoveryRoot::new(TargetScope::User, "~/.copilot/skills", 0, 0),
    DiscoveryRoot::new(TargetScope::User, "~/.agents/skills", 2, 2),
];
const GEMINI_CLI_ROOTS: &[DiscoveryRoot] = &[
    DiscoveryRoot::new(TargetScope::Workspace, ".gemini/skills", 1, 0),
    DiscoveryRoot::new(TargetScope::Workspace, ".agents/skills", 0, 1),
    DiscoveryRoot::new(TargetScope::User, "~/.gemini/skills", 1, 0),
    DiscoveryRoot::new(TargetScope::User, "~/.agents/skills", 0, 1),
];
const AMP_ROOTS: &[DiscoveryRoot] = &[
    DiscoveryRoot::new(TargetScope::Workspace, ".agents/skills", 0, 0),
    DiscoveryRoot::new(TargetScope::Workspace, ".claude/skills", 1, 1),
    DiscoveryRoot::new(TargetScope::User, "~/.config/agents/skills", 0, 0),
    DiscoveryRoot::new(TargetScope::User, "~/.config/amp/skills", 1, 1),
    DiscoveryRoot::new(TargetScope::User, "~/.agents/skills", 2, 2),
    DiscoveryRoot::new(TargetScope::User, "~/.claude/skills", 3, 3),
];
const OPENCODE_ROOTS: &[DiscoveryRoot] = &[
    DiscoveryRoot::new(TargetScope::Workspace, ".opencode/skills", 1, 0),
    DiscoveryRoot::new(TargetScope::Workspace, ".claude/skills", 2, 2),
    DiscoveryRoot::new(TargetScope::Workspace, ".agents/skills", 0, 1),
    DiscoveryRoot::new(TargetScope::User, "~/.config/opencode/skills", 1, 0),
    DiscoveryRoot::new(TargetScope::User, "~/.claude/skills", 2, 2),
    DiscoveryRoot::new(TargetScope::User, "~/.agents/skills", 0, 1),
];

const CODEX_ADAPTER: AdapterMetadata = AdapterMetadata {
    target: TargetRuntime::Codex,
    supported_scopes: BOTH_SCOPES,
    discovery_roots: CODEX_ROOTS,
    install_mode_risk: InstallModeRisk::SymlinkUnstable,
    verified_on: VERIFIED_ON,
    documentation_sources: CODEX_SOURCES,
};
const CLAUDE_CODE_ADAPTER: AdapterMetadata = AdapterMetadata {
    target: TargetRuntime::ClaudeCode,
    supported_scopes: BOTH_SCOPES,
    discovery_roots: CLAUDE_CODE_ROOTS,
    install_mode_risk: InstallModeRisk::SymlinkUnstable,
    verified_on: VERIFIED_ON,
    documentation_sources: CLAUDE_CODE_SOURCES,
};
const GITHUB_COPILOT_ADAPTER: AdapterMetadata = AdapterMetadata {
    target: TargetRuntime::GithubCopilot,
    supported_scopes: BOTH_SCOPES,
    discovery_roots: GITHUB_COPILOT_ROOTS,
    install_mode_risk: InstallModeRisk::SymlinkUnstable,
    verified_on: VERIFIED_ON,
    documentation_sources: GITHUB_COPILOT_SOURCES,
};
const GEMINI_CLI_ADAPTER: AdapterMetadata = AdapterMetadata {
    target: TargetRuntime::GeminiCli,
    supported_scopes: BOTH_SCOPES,
    discovery_roots: GEMINI_CLI_ROOTS,
    install_mode_risk: InstallModeRisk::SymlinkUnstable,
    verified_on: VERIFIED_ON,
    documentation_sources: GEMINI_CLI_SOURCES,
};
const AMP_ADAPTER: AdapterMetadata = AdapterMetadata {
    target: TargetRuntime::Amp,
    supported_scopes: BOTH_SCOPES,
    discovery_roots: AMP_ROOTS,
    install_mode_risk: InstallModeRisk::SymlinkUnstable,
    verified_on: VERIFIED_ON,
    documentation_sources: AMP_SOURCES,
};
const OPENCODE_ADAPTER: AdapterMetadata = AdapterMetadata {
    target: TargetRuntime::Opencode,
    supported_scopes: BOTH_SCOPES,
    discovery_roots: OPENCODE_ROOTS,
    install_mode_risk: InstallModeRisk::SymlinkUnstable,
    verified_on: VERIFIED_ON,
    documentation_sources: OPENCODE_SOURCES,
};
const ALL_ADAPTERS: &[AdapterMetadata] = &[
    CODEX_ADAPTER,
    CLAUDE_CODE_ADAPTER,
    GITHUB_COPILOT_ADAPTER,
    GEMINI_CLI_ADAPTER,
    AMP_ADAPTER,
    OPENCODE_ADAPTER,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_exposes_all_supported_adapters() {
        let registry = AdapterRegistry::new();
        let targets: Vec<_> = registry
            .all()
            .iter()
            .map(|adapter| adapter.target)
            .collect();

        assert_eq!(targets, TargetRuntime::all().to_vec());
    }

    #[test]
    fn codex_metadata_tracks_documented_discovery() {
        let registry = AdapterRegistry::new();
        let codex = registry.get(TargetRuntime::Codex);

        assert!(codex.supports_scope(TargetScope::Workspace));
        assert!(codex.supports_scope(TargetScope::User));
        assert_eq!(codex.install_mode_risk, InstallModeRisk::SymlinkUnstable);
        assert_eq!(
            codex
                .roots_for_scope(TargetScope::Workspace)
                .iter()
                .map(|root| root.path)
                .collect::<Vec<_>>(),
            vec![".agents/skills"]
        );
    }

    #[test]
    fn opencode_metadata_captures_ranked_discovery_roots() {
        let registry = AdapterRegistry::new();
        let opencode = registry.get(TargetRuntime::Opencode);

        let workspace_roots: Vec<_> = opencode
            .roots_for_scope(TargetScope::Workspace)
            .iter()
            .map(|root| (root.path, root.prefer_neutral_rank, root.prefer_native_rank))
            .collect();

        assert_eq!(opencode.install_mode_risk, InstallModeRisk::SymlinkUnstable);
        assert_eq!(
            workspace_roots,
            vec![
                (".opencode/skills", 1, 0),
                (".claude/skills", 2, 2),
                (".agents/skills", 0, 1),
            ]
        );
    }

    #[test]
    fn copilot_metadata_includes_documented_neutral_roots() {
        let registry = AdapterRegistry::new();
        let copilot = registry.get(TargetRuntime::GithubCopilot);

        let workspace_roots: Vec<_> = copilot
            .roots_for_scope(TargetScope::Workspace)
            .iter()
            .map(|root| root.path)
            .collect();
        let user_roots: Vec<_> = copilot
            .roots_for_scope(TargetScope::User)
            .iter()
            .map(|root| root.path)
            .collect();

        assert_eq!(
            workspace_roots,
            vec![".github/skills", ".claude/skills", ".agents/skills"]
        );
        assert_eq!(
            user_roots,
            vec!["~/.copilot/skills", "~/.agents/skills"],
            "~/.claude/skills is no longer documented for personal scope",
        );
    }

    #[test]
    fn amp_metadata_includes_documented_claude_compatible_roots() {
        let registry = AdapterRegistry::new();
        let amp = registry.get(TargetRuntime::Amp);

        let workspace_roots: Vec<_> = amp
            .roots_for_scope(TargetScope::Workspace)
            .iter()
            .map(|root| root.path)
            .collect();
        let user_roots: Vec<_> = amp
            .roots_for_scope(TargetScope::User)
            .iter()
            .map(|root| root.path)
            .collect();

        assert_eq!(workspace_roots, vec![".agents/skills", ".claude/skills"]);
        assert_eq!(
            user_roots,
            vec![
                "~/.config/agents/skills",
                "~/.config/amp/skills",
                "~/.agents/skills",
                "~/.claude/skills",
            ]
        );
    }

    #[test]
    fn every_adapter_records_verification_metadata() {
        let registry = AdapterRegistry::new();

        for adapter in registry.all() {
            assert_eq!(
                adapter.verified_on.len(),
                10,
                "{} must record an ISO verification date",
                adapter.target.as_str()
            );
            assert!(
                adapter.verified_on.starts_with("20"),
                "{} must record a plausible verification date",
                adapter.target.as_str()
            );
            assert!(
                adapter
                    .documentation_sources
                    .iter()
                    .all(|source| source.starts_with("https://")),
                "{} must cite vendor documentation",
                adapter.target.as_str()
            );
        }
    }
}
