//! CLI definitions for `skillctl`.
//!
//! The command surface is defined here so parsing stays strongly typed and can
//! grow independently from command execution logic.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

/// Top-level CLI parser for `skillctl`.
#[derive(Debug, Parser)]
#[command(
    name = "skillctl",
    version,
    about = "Local-first cross-agent skill manager for the open SKILL.md ecosystem",
    long_about = None
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
    #[arg(long, global = true)]
    pub target: Vec<String>,

    /// Working directory override for command execution.
    #[arg(long, global = true)]
    pub cwd: Option<PathBuf>,
}

/// Supported execution scopes for runtime planning.
#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
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
    Init,
    /// List managed skills.
    List,
    /// Install a skill from a Git URL, local path, or archive.
    #[command(alias = "i")]
    Install(InstallArgs),
    /// Remove a managed skill.
    Remove(SkillArg),
    /// Recompute and materialize projections.
    Sync,
    /// Check for and apply skill updates.
    Update(OptionalSkillArg),
    /// Pin a skill to a specific revision.
    Pin(PinArgs),
    /// Roll back a skill to a previous version or commit.
    Rollback(RollbackArgs),
    /// Show version and modification history.
    History(OptionalSkillArg),
    /// Diagnose skill loading and projection problems.
    Doctor,
    /// Explain a skill's active source and visibility.
    Explain(SkillArg),
    /// Create an overlay for a managed skill.
    Override(SkillArg),
    /// Detach a managed skill into a local canonical copy.
    Fork(SkillArg),
    /// Enable a managed skill.
    Enable(SkillArg),
    /// Disable a managed skill.
    Disable(SkillArg),
    /// Show filesystem paths for a managed skill.
    Path(SkillArg),
    /// Validate manifests, skills, and projections.
    Validate,
    /// Remove generated state and stale projections.
    Clean,
    /// Open the terminal UI.
    Tui,
    /// Inspect or change telemetry settings.
    Telemetry {
        /// Parsed telemetry subcommand.
        #[command(subcommand)]
        command: TelemetryCommand,
    },
    /// Run the MCP server.
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
    Status,
    /// Enable telemetry.
    Enable,
    /// Disable telemetry.
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
