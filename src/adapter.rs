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

/// High-level precedence behavior documented by a runtime.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PrecedenceBehavior {
    /// The runtime scans workspace roots from the current directory upward.
    AncestorWalk,
    /// The runtime has explicit enterprise, personal, then project precedence.
    EnterprisePersonalProject,
    /// The runtime documents project and personal roots without a shared neutral alias.
    ProjectAndPersonal,
    /// The runtime evaluates workspace, then user, then extension-provided skills.
    WorkspaceUserExtension,
    /// The runtime documents multiple compatible roots in the same scope.
    MultiRootChain,
}

/// Compatibility role of a documented discovery root.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DiscoveryRootKind {
    /// A neutral or cross-runtime root.
    Neutral,
    /// The runtime's vendor-native root.
    Native,
    /// A documented compatibility root owned by another ecosystem.
    Compatible,
}

/// Runtime install-mode risk used when selecting projection modes.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum InstallModeRisk {
    /// Copy mode is safe, but symlink behavior is not trusted by default.
    SymlinkUnstable,
    /// The runtime is documented as safe for both copy and symlink projection.
    CopySafe,
}

/// A documented runtime discovery root plus planner preference ranks.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct DiscoveryRoot {
    /// Scope where the root is discovered.
    pub scope: TargetScope,
    /// Root path exactly as documented by the spec.
    pub path: &'static str,
    /// Compatibility role for shared-root planning.
    pub kind: DiscoveryRootKind,
    /// Lower rank wins when the planner favors neutral roots.
    pub prefer_neutral_rank: u8,
    /// Lower rank wins when the planner favors native roots.
    pub prefer_native_rank: u8,
}

impl DiscoveryRoot {
    const fn new(
        scope: TargetScope,
        path: &'static str,
        kind: DiscoveryRootKind,
        prefer_neutral_rank: u8,
        prefer_native_rank: u8,
    ) -> Self {
        Self {
            scope,
            path,
            kind,
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
    /// High-level precedence behavior relevant to diagnostics.
    pub precedence: PrecedenceBehavior,
    /// Whether the runtime documents a neutral shared root in at least one scope.
    pub supports_neutral_roots: bool,
    /// Projection-mode risk used by future materialization and doctor flows.
    pub install_mode_risk: InstallModeRisk,
    /// Additional metadata files the runtime understands.
    pub extra_metadata_files: &'static [&'static str],
    /// Notable compatibility or loading notes surfaced by diagnostics.
    pub compatibility_notes: &'static [&'static str],
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
const NO_EXTRA_METADATA: &[&str] = &[];

const CODEX_ROOTS: &[DiscoveryRoot] = &[
    DiscoveryRoot::new(
        TargetScope::Workspace,
        ".agents/skills",
        DiscoveryRootKind::Neutral,
        0,
        0,
    ),
    DiscoveryRoot::new(
        TargetScope::User,
        "~/.agents/skills",
        DiscoveryRootKind::Neutral,
        0,
        0,
    ),
];
const CLAUDE_CODE_ROOTS: &[DiscoveryRoot] = &[
    DiscoveryRoot::new(
        TargetScope::Workspace,
        ".claude/skills",
        DiscoveryRootKind::Native,
        0,
        0,
    ),
    DiscoveryRoot::new(
        TargetScope::User,
        "~/.claude/skills",
        DiscoveryRootKind::Native,
        0,
        0,
    ),
];
const GITHUB_COPILOT_ROOTS: &[DiscoveryRoot] = &[
    DiscoveryRoot::new(
        TargetScope::Workspace,
        ".github/skills",
        DiscoveryRootKind::Native,
        0,
        0,
    ),
    DiscoveryRoot::new(
        TargetScope::Workspace,
        ".claude/skills",
        DiscoveryRootKind::Compatible,
        1,
        1,
    ),
    DiscoveryRoot::new(
        TargetScope::User,
        "~/.copilot/skills",
        DiscoveryRootKind::Native,
        0,
        0,
    ),
    DiscoveryRoot::new(
        TargetScope::User,
        "~/.claude/skills",
        DiscoveryRootKind::Compatible,
        1,
        1,
    ),
];
const GEMINI_CLI_ROOTS: &[DiscoveryRoot] = &[
    DiscoveryRoot::new(
        TargetScope::Workspace,
        ".gemini/skills",
        DiscoveryRootKind::Native,
        1,
        0,
    ),
    DiscoveryRoot::new(
        TargetScope::Workspace,
        ".agents/skills",
        DiscoveryRootKind::Neutral,
        0,
        1,
    ),
    DiscoveryRoot::new(
        TargetScope::User,
        "~/.gemini/skills",
        DiscoveryRootKind::Native,
        1,
        0,
    ),
    DiscoveryRoot::new(
        TargetScope::User,
        "~/.agents/skills",
        DiscoveryRootKind::Neutral,
        0,
        1,
    ),
];
const AMP_ROOTS: &[DiscoveryRoot] = &[
    DiscoveryRoot::new(
        TargetScope::Workspace,
        ".agents/skills",
        DiscoveryRootKind::Neutral,
        0,
        0,
    ),
    DiscoveryRoot::new(
        TargetScope::User,
        "~/.config/agents/skills",
        DiscoveryRootKind::Neutral,
        0,
        0,
    ),
    DiscoveryRoot::new(
        TargetScope::User,
        "~/.config/amp/skills",
        DiscoveryRootKind::Native,
        1,
        1,
    ),
];
const OPENCODE_ROOTS: &[DiscoveryRoot] = &[
    DiscoveryRoot::new(
        TargetScope::Workspace,
        ".opencode/skills",
        DiscoveryRootKind::Native,
        1,
        0,
    ),
    DiscoveryRoot::new(
        TargetScope::Workspace,
        ".claude/skills",
        DiscoveryRootKind::Compatible,
        2,
        2,
    ),
    DiscoveryRoot::new(
        TargetScope::Workspace,
        ".agents/skills",
        DiscoveryRootKind::Neutral,
        0,
        1,
    ),
    DiscoveryRoot::new(
        TargetScope::User,
        "~/.config/opencode/skills",
        DiscoveryRootKind::Native,
        1,
        0,
    ),
    DiscoveryRoot::new(
        TargetScope::User,
        "~/.claude/skills",
        DiscoveryRootKind::Compatible,
        2,
        2,
    ),
    DiscoveryRoot::new(
        TargetScope::User,
        "~/.agents/skills",
        DiscoveryRootKind::Neutral,
        0,
        1,
    ),
];

const CODEX_ADAPTER: AdapterMetadata = AdapterMetadata {
    target: TargetRuntime::Codex,
    supported_scopes: BOTH_SCOPES,
    discovery_roots: CODEX_ROOTS,
    precedence: PrecedenceBehavior::AncestorWalk,
    supports_neutral_roots: true,
    install_mode_risk: InstallModeRisk::SymlinkUnstable,
    extra_metadata_files: &["agents/openai.yaml"],
    compatibility_notes: &[
        "scans workspace skills from the current directory up to the repo root",
        "duplicate names may both appear during discovery",
    ],
};
const CLAUDE_CODE_ADAPTER: AdapterMetadata = AdapterMetadata {
    target: TargetRuntime::ClaudeCode,
    supported_scopes: BOTH_SCOPES,
    discovery_roots: CLAUDE_CODE_ROOTS,
    precedence: PrecedenceBehavior::EnterprisePersonalProject,
    supports_neutral_roots: false,
    install_mode_risk: InstallModeRisk::SymlinkUnstable,
    extra_metadata_files: NO_EXTRA_METADATA,
    compatibility_notes: &[
        "nested .claude/skills discovery is supported",
        "plugin skills participate in runtime loading",
    ],
};
const GITHUB_COPILOT_ADAPTER: AdapterMetadata = AdapterMetadata {
    target: TargetRuntime::GithubCopilot,
    supported_scopes: BOTH_SCOPES,
    discovery_roots: GITHUB_COPILOT_ROOTS,
    precedence: PrecedenceBehavior::ProjectAndPersonal,
    supports_neutral_roots: false,
    install_mode_risk: InstallModeRisk::SymlinkUnstable,
    extra_metadata_files: NO_EXTRA_METADATA,
    compatibility_notes: &[".claude/skills is a documented shared-root compatibility path"],
};
const GEMINI_CLI_ADAPTER: AdapterMetadata = AdapterMetadata {
    target: TargetRuntime::GeminiCli,
    supported_scopes: BOTH_SCOPES,
    discovery_roots: GEMINI_CLI_ROOTS,
    precedence: PrecedenceBehavior::WorkspaceUserExtension,
    supports_neutral_roots: true,
    install_mode_risk: InstallModeRisk::SymlinkUnstable,
    extra_metadata_files: NO_EXTRA_METADATA,
    compatibility_notes: &["workspace roots win before user and extension-provided skills"],
};
const AMP_ADAPTER: AdapterMetadata = AdapterMetadata {
    target: TargetRuntime::Amp,
    supported_scopes: BOTH_SCOPES,
    discovery_roots: AMP_ROOTS,
    precedence: PrecedenceBehavior::MultiRootChain,
    supports_neutral_roots: true,
    install_mode_risk: InstallModeRisk::SymlinkUnstable,
    extra_metadata_files: NO_EXTRA_METADATA,
    compatibility_notes: &["user scope includes both neutral and amp-specific documented roots"],
};
const OPENCODE_ADAPTER: AdapterMetadata = AdapterMetadata {
    target: TargetRuntime::Opencode,
    supported_scopes: BOTH_SCOPES,
    discovery_roots: OPENCODE_ROOTS,
    precedence: PrecedenceBehavior::MultiRootChain,
    supports_neutral_roots: true,
    install_mode_risk: InstallModeRisk::SymlinkUnstable,
    extra_metadata_files: NO_EXTRA_METADATA,
    compatibility_notes: &[
        "native skill management is available through the runtime",
        ".claude/skills and .agents/skills are documented compatibility roots",
    ],
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
    fn codex_metadata_tracks_upward_discovery_and_openai_metadata() {
        let registry = AdapterRegistry::new();
        let codex = registry.get(TargetRuntime::Codex);

        assert!(codex.supports_scope(TargetScope::Workspace));
        assert!(codex.supports_scope(TargetScope::User));
        assert!(codex.supports_neutral_roots);
        assert_eq!(codex.precedence, PrecedenceBehavior::AncestorWalk);
        assert_eq!(codex.install_mode_risk, InstallModeRisk::SymlinkUnstable);
        assert_eq!(codex.extra_metadata_files, &["agents/openai.yaml"]);
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
    fn opencode_metadata_captures_neutral_native_and_claude_compatible_roots() {
        let registry = AdapterRegistry::new();
        let opencode = registry.get(TargetRuntime::Opencode);

        let workspace_roots: Vec<_> = opencode
            .roots_for_scope(TargetScope::Workspace)
            .iter()
            .map(|root| {
                (
                    root.path,
                    root.kind,
                    root.prefer_neutral_rank,
                    root.prefer_native_rank,
                )
            })
            .collect();

        assert_eq!(opencode.precedence, PrecedenceBehavior::MultiRootChain);
        assert!(opencode.supports_neutral_roots);
        assert_eq!(opencode.install_mode_risk, InstallModeRisk::SymlinkUnstable);
        assert_eq!(
            workspace_roots,
            vec![
                (".opencode/skills", DiscoveryRootKind::Native, 1, 0),
                (".claude/skills", DiscoveryRootKind::Compatible, 2, 2),
                (".agents/skills", DiscoveryRootKind::Neutral, 0, 1),
            ]
        );
    }
}
