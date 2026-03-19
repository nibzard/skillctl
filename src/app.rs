//! Shared execution context for CLI, MCP, and TUI flows.

use std::{env, fs, path::PathBuf};

use crate::{
    cli::{GlobalArgs, Scope},
    error::AppError,
};

/// Stable application context assembled once per invocation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AppContext {
    /// Resolved working directory for the current invocation.
    pub working_directory: PathBuf,
    /// Preferred output shape for the invocation.
    pub output_mode: OutputMode,
    /// Requested verbosity level.
    pub verbosity: Verbosity,
    /// Requested interaction mode.
    pub interaction_mode: InteractionMode,
    /// Shared command selectors.
    pub selector: CommandSelector,
}

impl AppContext {
    /// Build an execution context from the parsed global CLI flags.
    pub fn from_global_args(global: &GlobalArgs) -> Result<Self, AppError> {
        let current_directory =
            env::current_dir().map_err(|source| AppError::CurrentWorkingDirectory { source })?;
        let working_directory = match &global.cwd {
            Some(path) if path.is_absolute() => path.clone(),
            Some(path) => current_directory.join(path),
            None => current_directory,
        };

        let metadata = fs::metadata(&working_directory).map_err(|source| {
            AppError::WorkingDirectoryUnavailable {
                path: working_directory.clone(),
                source,
            }
        })?;
        if !metadata.is_dir() {
            return Err(AppError::WorkingDirectoryNotDirectory {
                path: working_directory,
            });
        }

        Ok(Self {
            working_directory,
            output_mode: OutputMode::from_json_flag(global.json),
            verbosity: Verbosity::from_flags(global.quiet, global.verbose),
            interaction_mode: InteractionMode::from_flags(global.no_input, global.interactive),
            selector: CommandSelector {
                skill_name: global.name.clone(),
                scope: global.scope,
                targets: global.target.clone(),
            },
        })
    }
}

/// Output formats supported by the runtime.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutputMode {
    /// Human-readable terminal output.
    Human,
    /// Stable JSON output.
    Json,
}

impl OutputMode {
    /// Convert the global `--json` flag into an output mode.
    pub const fn from_json_flag(json: bool) -> Self {
        if json { Self::Json } else { Self::Human }
    }
}

/// Verbosity levels for command presentation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Verbosity {
    /// Minimal output.
    Quiet,
    /// Default output.
    Normal,
    /// Expanded output.
    Verbose,
}

impl Verbosity {
    /// Convert CLI flags into a normalized verbosity value.
    pub const fn from_flags(quiet: bool, verbose: bool) -> Self {
        if quiet {
            Self::Quiet
        } else if verbose {
            Self::Verbose
        } else {
            Self::Normal
        }
    }
}

/// Interaction policy for commands that may prompt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InteractionMode {
    /// Command decides based on terminal capabilities.
    Auto,
    /// Force prompts when available.
    Interactive,
    /// Never prompt for input.
    NonInteractive,
}

impl InteractionMode {
    /// Convert CLI flags into a normalized interaction mode.
    pub const fn from_flags(no_input: bool, interactive: bool) -> Self {
        if no_input {
            Self::NonInteractive
        } else if interactive {
            Self::Interactive
        } else {
            Self::Auto
        }
    }
}

/// Cross-command selectors that higher layers can reuse.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CommandSelector {
    /// Exact skill name selector.
    pub skill_name: Option<String>,
    /// Optional scope selector.
    pub scope: Option<Scope>,
    /// Optional target filters.
    pub targets: Vec<String>,
}
