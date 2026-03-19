//! Command dispatch and presentation for the `skillctl` runtime.

use std::fmt::Write as _;

use clap::CommandFactory;

use crate::{
    app::{AppContext, OutputMode},
    cli::{Cli, Command},
    doctor,
    error::{AppError, ExitStatus},
    history, manifest, materialize, mcp, overlay, planner,
    response::{AppResponse, RenderedOutput},
    skill, source, telemetry, tui,
};

/// Fully rendered command result ready for the process boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunResult {
    /// Structured terminal output.
    pub output: RenderedOutput,
    /// Exit status for the process.
    pub exit_status: ExitStatus,
}

/// Execute a parsed CLI invocation and render the resulting output.
pub fn run(cli: Cli) -> Result<RunResult, AppError> {
    let output_mode = OutputMode::from_json_flag(cli.global.json);
    let command_name = cli.command.as_ref().map_or("skillctl", Command::name);
    let context = match AppContext::from_global_args(&cli.global) {
        Ok(context) => context,
        Err(error) => return render_failure(output_mode, command_name, error),
    };

    match cli.command {
        None => Ok(RunResult {
            output: RenderedOutput {
                stdout: help_text(),
                stderr: String::new(),
            },
            exit_status: ExitStatus::Success,
        }),
        Some(command) => run_command(command, &context),
    }
}

fn run_command(command: Command, context: &AppContext) -> Result<RunResult, AppError> {
    match dispatch(&command, context) {
        Ok(response) => {
            let exit_status = response.exit_status();
            let output = render_response(context.output_mode, response)?;

            Ok(RunResult {
                output,
                exit_status,
            })
        }
        Err(error) => render_failure(context.output_mode, command.name(), error),
    }
}

fn dispatch(command: &Command, context: &AppContext) -> Result<AppResponse, AppError> {
    match command {
        Command::Init => manifest::handle_init(context, manifest::InitRequest),
        Command::List => skill::handle_list(context, skill::ListRequest),
        Command::Install(args) => {
            source::handle_install(context, source::InstallRequest::new(args.source.clone()))
        }
        Command::Remove(args) => {
            skill::handle_remove(context, skill::RemoveRequest::new(args.skill.clone()))
        }
        Command::Sync => materialize::handle_sync(context, materialize::SyncRequest),
        Command::Update(args) => {
            planner::handle_update(context, planner::UpdateRequest::new(args.skill.clone()))
        }
        Command::Pin(args) => history::handle_pin(
            context,
            history::PinRequest::new(args.skill.clone(), args.ref_spec.clone()),
        ),
        Command::Rollback(args) => history::handle_rollback(
            context,
            history::RollbackRequest::new(args.skill.clone(), args.version_or_commit.clone()),
        ),
        Command::History(args) => {
            history::handle_history(context, history::HistoryRequest::new(args.skill.clone()))
        }
        Command::Doctor => doctor::handle_doctor(context, doctor::DoctorRequest),
        Command::Explain(args) => {
            skill::handle_explain(context, skill::ExplainRequest::new(args.skill.clone()))
        }
        Command::Override(args) => {
            overlay::handle_override(context, overlay::OverrideRequest::new(args.skill.clone()))
        }
        Command::Fork(args) => {
            overlay::handle_fork(context, overlay::ForkRequest::new(args.skill.clone()))
        }
        Command::Enable(args) => {
            skill::handle_enable(context, skill::EnableRequest::new(args.skill.clone()))
        }
        Command::Disable(args) => {
            skill::handle_disable(context, skill::DisableRequest::new(args.skill.clone()))
        }
        Command::Path(args) => {
            skill::handle_path(context, skill::PathRequest::new(args.skill.clone()))
        }
        Command::Validate => doctor::handle_validate(context, doctor::ValidateRequest),
        Command::Clean => materialize::handle_clean(context, materialize::CleanRequest),
        Command::Tui => tui::handle_open(context, tui::OpenTuiRequest),
        Command::Telemetry { command } => telemetry::handle_command(context, command),
        Command::Mcp { command } => mcp::handle_command(context, command),
    }
}

fn render_failure(
    output_mode: OutputMode,
    command: &'static str,
    error: AppError,
) -> Result<RunResult, AppError> {
    let exit_status = error.exit_status();
    let output = render_response(
        output_mode,
        AppResponse::failure(command, error.to_string()),
    )?;

    Ok(RunResult {
        output,
        exit_status,
    })
}

fn render_response(
    output_mode: OutputMode,
    response: AppResponse,
) -> Result<RenderedOutput, AppError> {
    match output_mode {
        OutputMode::Json => Ok(RenderedOutput {
            stdout: format!("{}\n", serde_json::to_string_pretty(&response)?),
            stderr: String::new(),
        }),
        OutputMode::Human => Ok(render_human_response(response)),
    }
}

fn render_human_response(response: AppResponse) -> RenderedOutput {
    if response.ok {
        let mut stdout = String::new();

        if let Some(summary) = response.summary {
            let _ = writeln!(stdout, "{summary}");
        } else {
            let _ = writeln!(stdout, "{} completed successfully", response.command);
        }

        for warning in response.warnings {
            let _ = writeln!(stdout, "warning: {warning}");
        }

        RenderedOutput {
            stdout,
            stderr: String::new(),
        }
    } else {
        let mut stderr = String::new();
        for error in response.errors {
            let _ = writeln!(stderr, "{error}");
        }

        RenderedOutput {
            stdout: String::new(),
            stderr,
        }
    }
}

fn help_text() -> String {
    let mut command = Cli::command();
    let mut buffer = Vec::new();
    command.write_long_help(&mut buffer).expect("write help");

    let mut help = String::from_utf8(buffer).expect("clap help is valid utf-8");
    help.push('\n');
    help
}
