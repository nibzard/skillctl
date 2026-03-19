//! CLI definitions for `skillctl`.
//!
//! The command surface is defined here so parsing stays strongly typed and can
//! grow independently from command execution logic.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};

const ROOT_LONG_ABOUT: &str = "\
skillctl manages the local lifecycle of open SKILL.md skills across multiple agent runtimes.

Model:
  Canonical workspace skills live in .agents/skills.
  Imported sources are pinned in .agents/skillctl.lock and cached in ~/.skillctl/store/imports.
  Optional overlays live in .agents/overlays/<skill>.
  Effective skills are projected as generated runtime-visible copies.
  Local history and telemetry consent live in ~/.skillctl/state.db.

Use 'skillctl doctor' to diagnose missing, stale, shadowed, detached, or conflicting skills.";

const ROOT_AFTER_LONG_HELP: &str = "\
Quickstart:
  skillctl init
  skillctl sync
  skillctl install ../shared-skills --interactive
  skillctl explain release-notes
  skillctl doctor

Troubleshooting shortcuts:
  skillctl explain <skill> [--target <runtime>]
  skillctl path <skill>
  skillctl history [skill]
  skillctl doctor

Supported runtimes:
  codex, claude-code, github-copilot, gemini-cli, amp, opencode

Status:
  'tui' opens a read-only dashboard over the same state and inspection model as the CLI.
  Opening it does not bootstrap bundled skills or write new history entries.
  'mcp serve' exposes the same lifecycle operations to agents over MCP.";

const INIT_LONG_ABOUT: &str = "\
Create the default .agents workspace layout for skillctl.

This bootstraps:
  .agents/skills
  .agents/overlays
  .agents/skillctl.yaml
  .agents/skillctl.lock

By default skillctl also adds generated runtime roots to .git/info/exclude instead of mutating .gitignore.";

const INIT_AFTER_LONG_HELP: &str = "\
Examples:
  skillctl init
  skillctl --cwd path/to/repo init

Next steps:
  skillctl sync
  skillctl install ../shared-skills --interactive";

const LIST_LONG_ABOUT: &str = "\
List managed installed skills from the local state store.

The result includes scope, source, pinned revision, projection roots, drift counters, and whether the skill is detached, forked, or overlay-managed.";

const LIST_AFTER_LONG_HELP: &str = "\
Examples:
  skillctl list
  skillctl list --json
  skillctl --scope user list";

const INSTALL_LONG_ABOUT: &str = "\
Inspect a Git URL, local directory, or local archive for skill candidates, select exact skills, pin the installed revision, update manifest and lockfile state, and materialize projections for the chosen scope.

Interactive installs can prompt for candidate and scope selection.
Non-interactive installs never guess and require an exact selector such as --name <skill> when multiple candidates exist.";

const INSTALL_AFTER_LONG_HELP: &str = "\
Examples:
  skillctl install ../shared-skills --interactive
  skillctl install ./skills.tar.gz --name release-notes --scope user
  skillctl i https://github.com/acme/skills.git --name ai-sdk --scope workspace

Notes:
  Imported sources are cached under ~/.skillctl/store/imports.
  Use 'skillctl path <skill>' or 'skillctl explain <skill>' to inspect the result.";

const REMOVE_LONG_ABOUT: &str = "\
Remove a managed skill from workspace or user scope.

skillctl removes managed imports, cached stored sources, and generated projections for the selected skill. It does not delete canonical local skills or overlays unless they are part of a detached workspace copy.";

const REMOVE_AFTER_LONG_HELP: &str = "\
Examples:
  skillctl remove release-notes
  skillctl --scope user remove skillctl";

const SYNC_LONG_ABOUT: &str = "\
Recompute the effective-skill graph and materialize generated runtime-visible copies.

sync resolves canonical skills, imports, overlays, conflicts, and planned roots, then refreshes only the directories previously managed by skillctl.";

const SYNC_AFTER_LONG_HELP: &str = "\
Examples:
  skillctl sync
  skillctl --scope workspace --target codex --target gemini-cli sync

Use 'skillctl doctor' after sync if a runtime still does not load the expected skill.";

const UPDATE_LONG_ABOUT: &str = "\
Check managed imported skills against their upstream source, record update-check history, detect overlays or local drift, and recommend the next safe action.

The current implementation reports plans and follow-up actions instead of blindly overwriting local changes.";

const UPDATE_AFTER_LONG_HELP: &str = "\
Examples:
  skillctl update
  skillctl update ai-sdk --scope workspace
  skillctl update ai-sdk --json

Recommended follow-up actions:
  apply: safe to refresh the pinned revision
  create-overlay: move local edits into .agents/overlays/<skill>
  detach: keep a full local copy under .agents/skills
  skip: keep the current pin for now

Use 'skillctl history <skill>' and 'skillctl explain <skill>' to inspect drift.";

const PIN_LONG_ABOUT: &str = "\
Resolve and pin an imported skill to an exact revision.

pin updates the manifest, lockfile, stored import cache, install record, and projections so later update and rollback flows have an exact baseline.";

const PIN_AFTER_LONG_HELP: &str = "\
Examples:
  skillctl pin ai-sdk main
  skillctl pin ai-sdk 0123456789abcdef";

const ROLLBACK_LONG_ABOUT: &str = "\
Re-activate a previously recorded version or commit for a managed imported skill.

rollback restores the pinned revision, updates projections, and records the transition in local history.";

const ROLLBACK_AFTER_LONG_HELP: &str = "\
Examples:
  skillctl rollback ai-sdk 0123456789abcdef
  skillctl rollback ai-sdk sha256:effective-version

Use 'skillctl history <skill>' to discover prior revisions and rollback points.";

const HISTORY_LONG_ABOUT: &str = "\
Show the local install and modification ledger for one skill or for the whole workspace.

History entries include installs, update checks, applied projections, pins, rollbacks, overlays, forks, cleanup, and telemetry consent changes.";

const HISTORY_AFTER_LONG_HELP: &str = "\
Examples:
  skillctl history
  skillctl history ai-sdk
  skillctl history ai-sdk --json";

const DOCTOR_LONG_ABOUT: &str = "\
Diagnose missing, stale, shadowed, detached, or conflicting skills across enabled runtimes.

doctor validates manifests, lockfile state, overlays, projections, precedence roots, vendor-specific metadata, trust signals, and generated-copy drift.";

const DOCTOR_AFTER_LONG_HELP: &str = "\
Examples:
  skillctl doctor
  skillctl doctor --target opencode
  skillctl doctor --json

Start here when:
  a runtime cannot see a skill
  a generated copy looks stale
  explain shows a conflict or shadowed winner
  update reports drift or trust warnings";

const EXPLAIN_LONG_ABOUT: &str = "\
Explain which candidate wins for a projected skill name, why it won, which runtimes can see it, and whether the active copy differs from the pinned managed source.

Use explain to inspect winner selection, shadowed candidates, same-name conflicts, target roots, and detached or forked state.";

const EXPLAIN_AFTER_LONG_HELP: &str = "\
Examples:
  skillctl explain ai-sdk
  skillctl explain ai-sdk --target codex
  skillctl explain ai-sdk --scope workspace --json

Follow up with 'skillctl path <skill>' for concrete filesystem locations.";

const OVERRIDE_LONG_ABOUT: &str = "\
Create or reuse an overlay directory for a managed imported skill.

Overlays live under .agents/overlays/<skill> and replace matching upstream files without forking the entire imported skill.";

const OVERRIDE_AFTER_LONG_HELP: &str = "\
Examples:
  skillctl override ai-sdk

After editing overlay files, run 'skillctl sync' or 'skillctl update <skill>' to inspect the new effective version.";

const FORK_LONG_ABOUT: &str = "\
Detach a managed imported workspace skill into canonical local ownership under .agents/skills.

fork copies the current effective skill, merges any overlay content into the local copy, removes managed import state for that skill, and marks the install as detached from upstream lifecycle management.";

const FORK_AFTER_LONG_HELP: &str = "\
Examples:
  skillctl fork ai-sdk

Use this when you want full local ownership instead of keeping an upgradeable imported skill.";

const ENABLE_LONG_ABOUT: &str = "\
Enable a managed import so it participates in resolution and projection again.";

const ENABLE_AFTER_LONG_HELP: &str = "\
Examples:
  skillctl enable ai-sdk";

const DISABLE_LONG_ABOUT: &str = "\
Disable a managed import without deleting its recorded history or cached source.

Disabled imports stay on disk but stop participating in the effective-skill graph until re-enabled.";

const DISABLE_AFTER_LONG_HELP: &str = "\
Examples:
  skillctl disable ai-sdk";

const PATH_LONG_ABOUT: &str = "\
Show canonical, stored, overlay, planned-root, and projected paths for a managed skill.

Use this when you need to know exactly which directory is canonical, where the cached import lives, and which runtime roots should contain generated copies.";

const PATH_AFTER_LONG_HELP: &str = "\
Examples:
  skillctl path ai-sdk
  skillctl path ai-sdk --target codex --json";

const VALIDATE_LONG_ABOUT: &str = "\
Validate manifests, lockfile state, skill directories, and overlay mappings without checking runtime projection drift.

Use validate for structural correctness and use doctor when you also need runtime-facing diagnostics.";

const VALIDATE_AFTER_LONG_HELP: &str = "\
Examples:
  skillctl validate
  skillctl validate --json";

const CLEAN_LONG_ABOUT: &str = "\
Remove only skillctl-generated projections and generated cached state that is no longer needed.

clean never deletes canonical workspace skills or overlays unless you explicitly remove the managed skill through the lifecycle commands.";

const CLEAN_AFTER_LONG_HELP: &str = "\
Examples:
  skillctl clean
  skillctl --scope workspace clean";

const TUI_LONG_ABOUT: &str = "\
Open the terminal inspection UI.

The terminal inspection UI is a read-only dashboard for installed versions,
update availability, overlays, local modifications, target visibility, pin or
rollback context, and recent history.

Opening it does not bootstrap bundled skills or write new history entries.

Every suggested action maps to a documented CLI command.";

const TUI_AFTER_LONG_HELP: &str = "\
Examples:
  skillctl tui
  skillctl --scope workspace --name ai-sdk tui
  skillctl --target codex tui

Mapped actions:
  refresh update state: skillctl update [skill]
  inspect visibility: skillctl explain <skill>
  inspect filesystem roots: skillctl path <skill>
  inspect history: skillctl history [skill]
  pin or roll back: skillctl pin <skill> <ref> / skillctl rollback <skill> <version-or-commit>";

const TELEMETRY_LONG_ABOUT: &str = "\
Inspect or change public-only telemetry consent.

Telemetry is enabled by default for public install and update events after a first-run notice. Local history is always kept even when telemetry is disabled.";

const TELEMETRY_AFTER_LONG_HELP: &str = "\
Examples:
  skillctl telemetry status
  skillctl telemetry disable
  skillctl telemetry enable";

const TELEMETRY_STATUS_LONG_ABOUT: &str = "\
Show the effective telemetry policy, stored consent, and whether the first-run notice has already been seen.";

const TELEMETRY_ENABLE_LONG_ABOUT: &str = "\
Enable public-only remote telemetry for future public install and update events.";

const TELEMETRY_DISABLE_LONG_ABOUT: &str = "\
Disable remote telemetry while keeping local history, pins, and diagnostics fully available.";

const MCP_LONG_ABOUT: &str = "\
Run the MCP surface for skillctl.

The MCP server exposes the same lifecycle operations and JSON envelopes as the CLI.";

const MCP_AFTER_LONG_HELP: &str = "\
Available v1 tools:
  skills_list -> skillctl list --json
  skills_install -> skillctl install <source> --json
  skills_remove -> skillctl remove <skill> --json
  skills_sync -> skillctl sync --json
  skills_update -> skillctl update [skill] --json
  skills_rollback -> skillctl rollback <skill> <version-or-commit> --json
  skills_history -> skillctl history [skill] --json
  skills_explain -> skillctl explain <skill> --json
  skills_override_create -> skillctl override <skill> --json
  skills_validate -> skillctl validate --json
  skills_doctor -> skillctl doctor --json
  skills_telemetry_status -> skillctl telemetry status --json";

const MCP_SERVE_LONG_ABOUT: &str = "\
Start the MCP server process.

This command serves newline-delimited JSON-RPC over stdin/stdout and exposes the v1 lifecycle tools.";

/// Top-level CLI parser for `skillctl`.
#[derive(Debug, Parser)]
#[command(
    name = "skillctl",
    version,
    about = "Local-first cross-agent skill manager for the open SKILL.md ecosystem",
    long_about = ROOT_LONG_ABOUT,
    after_long_help = ROOT_AFTER_LONG_HELP
)]
pub struct Cli {
    /// Global execution flags shared across commands.
    #[command(flatten)]
    pub global: GlobalArgs,

    /// Parsed top-level subcommand, or `None` when rendering help.
    #[command(subcommand)]
    pub command: Option<Command>,
}

/// Execution flags that every command can consume.
#[derive(Clone, Debug, Default, Args)]
pub struct GlobalArgs {
    /// Emit machine-readable JSON instead of human text.
    #[arg(long, global = true)]
    pub json: bool,

    /// Suppress non-essential human output.
    #[arg(long, global = true, conflicts_with = "verbose")]
    pub quiet: bool,

    /// Emit additional human output.
    #[arg(long, global = true, conflicts_with = "quiet")]
    pub verbose: bool,

    /// Disable interactive prompts.
    #[arg(long, global = true, conflicts_with = "interactive")]
    pub no_input: bool,

    /// Force interactive prompts when the command supports them.
    #[arg(long, global = true, conflicts_with = "no_input")]
    pub interactive: bool,

    /// Exact skill name selector for commands that need one.
    #[arg(long, global = true)]
    pub name: Option<String>,

    /// Preferred execution scope.
    #[arg(long, global = true, value_enum)]
    pub scope: Option<Scope>,

    /// Runtime target filter.
    #[arg(
        long,
        global = true,
        help = "Runtime target filter. Repeat to narrow commands to enabled manifest targets"
    )]
    pub target: Vec<String>,

    /// Working directory override for command execution.
    #[arg(
        long,
        global = true,
        help = "Working directory override used to locate the workspace manifest"
    )]
    pub cwd: Option<PathBuf>,
}

/// Supported execution scopes for runtime planning.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum Scope {
    /// Workspace-local scope.
    Workspace,
    /// User-wide scope.
    User,
}

/// Parsed top-level commands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Initialize a workspace-local skillctl layout.
    #[command(long_about = INIT_LONG_ABOUT, after_long_help = INIT_AFTER_LONG_HELP)]
    Init,
    /// List managed skills.
    #[command(long_about = LIST_LONG_ABOUT, after_long_help = LIST_AFTER_LONG_HELP)]
    List,
    /// Install a skill from a Git URL, local path, or archive.
    #[command(
        alias = "i",
        long_about = INSTALL_LONG_ABOUT,
        after_long_help = INSTALL_AFTER_LONG_HELP
    )]
    Install(InstallArgs),
    /// Remove a managed skill.
    #[command(long_about = REMOVE_LONG_ABOUT, after_long_help = REMOVE_AFTER_LONG_HELP)]
    Remove(SkillArg),
    /// Recompute and materialize projections.
    #[command(long_about = SYNC_LONG_ABOUT, after_long_help = SYNC_AFTER_LONG_HELP)]
    Sync,
    /// Check for skill updates and recommend next actions.
    #[command(long_about = UPDATE_LONG_ABOUT, after_long_help = UPDATE_AFTER_LONG_HELP)]
    Update(OptionalSkillArg),
    /// Pin a skill to a specific revision.
    #[command(long_about = PIN_LONG_ABOUT, after_long_help = PIN_AFTER_LONG_HELP)]
    Pin(PinArgs),
    /// Roll back a skill to a previous version or commit.
    #[command(long_about = ROLLBACK_LONG_ABOUT, after_long_help = ROLLBACK_AFTER_LONG_HELP)]
    Rollback(RollbackArgs),
    /// Show version and modification history.
    #[command(long_about = HISTORY_LONG_ABOUT, after_long_help = HISTORY_AFTER_LONG_HELP)]
    History(OptionalSkillArg),
    /// Diagnose missing, stale, shadowed, or conflicting skills.
    #[command(long_about = DOCTOR_LONG_ABOUT, after_long_help = DOCTOR_AFTER_LONG_HELP)]
    Doctor,
    /// Explain a skill's winner, target visibility, and drift.
    #[command(long_about = EXPLAIN_LONG_ABOUT, after_long_help = EXPLAIN_AFTER_LONG_HELP)]
    Explain(SkillArg),
    /// Create an overlay for a managed skill.
    #[command(long_about = OVERRIDE_LONG_ABOUT, after_long_help = OVERRIDE_AFTER_LONG_HELP)]
    Override(SkillArg),
    /// Detach a managed skill into a local canonical copy.
    #[command(long_about = FORK_LONG_ABOUT, after_long_help = FORK_AFTER_LONG_HELP)]
    Fork(SkillArg),
    /// Enable a managed skill.
    #[command(long_about = ENABLE_LONG_ABOUT, after_long_help = ENABLE_AFTER_LONG_HELP)]
    Enable(SkillArg),
    /// Disable a managed skill.
    #[command(long_about = DISABLE_LONG_ABOUT, after_long_help = DISABLE_AFTER_LONG_HELP)]
    Disable(SkillArg),
    /// Show filesystem paths for a managed skill.
    #[command(long_about = PATH_LONG_ABOUT, after_long_help = PATH_AFTER_LONG_HELP)]
    Path(SkillArg),
    /// Validate manifests, skills, and projections.
    #[command(long_about = VALIDATE_LONG_ABOUT, after_long_help = VALIDATE_AFTER_LONG_HELP)]
    Validate,
    /// Remove generated state and stale projections.
    #[command(long_about = CLEAN_LONG_ABOUT, after_long_help = CLEAN_AFTER_LONG_HELP)]
    Clean,
    /// Open the terminal UI.
    #[command(long_about = TUI_LONG_ABOUT, after_long_help = TUI_AFTER_LONG_HELP)]
    Tui,
    /// Inspect or change telemetry settings.
    #[command(long_about = TELEMETRY_LONG_ABOUT, after_long_help = TELEMETRY_AFTER_LONG_HELP)]
    Telemetry {
        /// Parsed telemetry subcommand.
        #[command(subcommand)]
        command: TelemetryCommand,
    },
    /// Run the MCP server.
    #[command(long_about = MCP_LONG_ABOUT, after_long_help = MCP_AFTER_LONG_HELP)]
    Mcp {
        /// Parsed MCP subcommand.
        #[command(subcommand)]
        command: McpCommand,
    },
}

impl Command {
    /// Return the stable command identifier used in responses.
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Init => "init",
            Self::List => "list",
            Self::Install(_) => "install",
            Self::Remove(_) => "remove",
            Self::Sync => "sync",
            Self::Update(_) => "update",
            Self::Pin(_) => "pin",
            Self::Rollback(_) => "rollback",
            Self::History(_) => "history",
            Self::Doctor => "doctor",
            Self::Explain(_) => "explain",
            Self::Override(_) => "override",
            Self::Fork(_) => "fork",
            Self::Enable(_) => "enable",
            Self::Disable(_) => "disable",
            Self::Path(_) => "path",
            Self::Validate => "validate",
            Self::Clean => "clean",
            Self::Tui => "tui",
            Self::Telemetry { command } => command.name(),
            Self::Mcp { command } => command.name(),
        }
    }
}

/// Arguments for `install`.
#[derive(Debug, Args)]
pub struct InstallArgs {
    /// Source repository, path, or archive to install from.
    pub source: String,
}

/// Arguments for commands that require a skill name.
#[derive(Debug, Args)]
pub struct SkillArg {
    /// Managed skill name.
    pub skill: String,
}

/// Arguments for commands that optionally target a single skill.
#[derive(Debug, Args)]
pub struct OptionalSkillArg {
    /// Optional managed skill name.
    pub skill: Option<String>,
}

/// Arguments for `pin`.
#[derive(Debug, Args)]
pub struct PinArgs {
    /// Managed skill name.
    pub skill: String,
    /// Exact revision to pin.
    #[arg(name = "ref")]
    pub ref_spec: String,
}

/// Arguments for `rollback`.
#[derive(Debug, Args)]
pub struct RollbackArgs {
    /// Managed skill name.
    pub skill: String,
    /// Previous version identifier or pinned commit.
    pub version_or_commit: String,
}

/// Subcommands for telemetry management.
#[derive(Debug, Subcommand)]
pub enum TelemetryCommand {
    /// Show current telemetry settings.
    #[command(long_about = TELEMETRY_STATUS_LONG_ABOUT)]
    Status,
    /// Enable telemetry.
    #[command(long_about = TELEMETRY_ENABLE_LONG_ABOUT)]
    Enable,
    /// Disable telemetry.
    #[command(long_about = TELEMETRY_DISABLE_LONG_ABOUT)]
    Disable,
}

impl TelemetryCommand {
    /// Return the stable command identifier used in responses.
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Status => "telemetry-status",
            Self::Enable => "telemetry-enable",
            Self::Disable => "telemetry-disable",
        }
    }
}

/// Subcommands for the MCP surface.
#[derive(Debug, Subcommand)]
pub enum McpCommand {
    /// Start the MCP server.
    #[command(long_about = MCP_SERVE_LONG_ABOUT)]
    Serve,
}

impl McpCommand {
    /// Return the stable command identifier used in responses.
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Serve => "mcp-serve",
        }
    }
}
