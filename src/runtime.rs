//! Command dispatch and presentation for the `skillctl` runtime.

use std::{ffi::OsString, fmt::Write as _};

use clap::{CommandFactory, Parser, error::ErrorKind};

use crate::{
    app::{AppContext, OutputMode, Verbosity},
    builtin,
    cli::{Cli, Command, McpCommand},
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

/// Parse raw CLI arguments, normalize parse failures, and execute the command.
pub fn run_from_args<I, T>(args: I) -> Result<RunResult, AppError>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString>,
{
    let raw_args: Vec<OsString> = args.into_iter().map(Into::into).collect();
    let output_mode = output_mode_from_raw_args(&raw_args);

    match Cli::try_parse_from(raw_args.iter().cloned()) {
        Ok(cli) => run(cli),
        Err(error) => render_parse_error(&raw_args, output_mode, error),
    }
}

/// Execute a parsed CLI invocation and render the resulting output.
pub fn run(cli: Cli) -> Result<RunResult, AppError> {
    let output_mode = OutputMode::from_json_flag(cli.global.json);
    let verbosity = Verbosity::from_flags(cli.global.quiet, cli.global.verbose);
    let command_name = cli.command.as_ref().map_or("skillctl", Command::name);
    let context = match AppContext::from_global_args(&cli.global) {
        Ok(context) => context,
        Err(error) => return render_failure(output_mode, verbosity, command_name, error),
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
    if matches!(
        command,
        Command::Mcp {
            command: McpCommand::Serve
        }
    ) {
        builtin::ensure_bundled_skill(context, false)?;
        mcp::serve(context)?;
        return Ok(RunResult {
            output: RenderedOutput::default(),
            exit_status: ExitStatus::Success,
        });
    }

    match execute_command(&command, context) {
        Ok(response) => {
            let exit_status = response.exit_status();
            let output = render_response(context.output_mode, context.verbosity, response)?;

            Ok(RunResult {
                output,
                exit_status,
            })
        }
        Err(error) => render_failure(
            context.output_mode,
            context.verbosity,
            command.name(),
            error,
        ),
    }
}

/// Execute one parsed command through the shared lifecycle layer.
pub fn execute_command(command: &Command, context: &AppContext) -> Result<AppResponse, AppError> {
    if should_bootstrap_bundled_skill(command) {
        builtin::ensure_bundled_skill(context, false)?;
    }
    dispatch(command, context)
}

fn should_bootstrap_bundled_skill(command: &Command) -> bool {
    !matches!(command, Command::Tui)
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
    verbosity: Verbosity,
    command: &'static str,
    error: AppError,
) -> Result<RunResult, AppError> {
    let exit_status = error.exit_status();
    let output = render_response(
        output_mode,
        verbosity,
        AppResponse::failure(command, error.to_string()),
    )?;

    Ok(RunResult {
        output,
        exit_status,
    })
}

fn render_response(
    output_mode: OutputMode,
    verbosity: Verbosity,
    response: AppResponse,
) -> Result<RenderedOutput, AppError> {
    match output_mode {
        OutputMode::Json => Ok(RenderedOutput {
            stdout: format!("{}\n", serde_json::to_string_pretty(&response)?),
            stderr: String::new(),
        }),
        OutputMode::Human => Ok(render_human_response(verbosity, response)),
    }
}

fn render_human_response(verbosity: Verbosity, response: AppResponse) -> RenderedOutput {
    if response.ok {
        let mut stdout = String::new();
        let has_data = response_has_data(&response);

        if !matches!(verbosity, Verbosity::Quiet) {
            if let Some(summary) = response.summary {
                let _ = writeln!(stdout, "{summary}");
            } else {
                let _ = writeln!(stdout, "{} completed successfully", response.command);
            }
        }

        for warning in &response.warnings {
            let _ = writeln!(stdout, "warning: {warning}");
        }

        if matches!(verbosity, Verbosity::Verbose) && has_data {
            let _ = writeln!(stdout, "{}", render_response_data(&response.data));
        }

        RenderedOutput {
            stdout,
            stderr: String::new(),
        }
    } else {
        let mut stderr = String::new();
        let has_data = response_has_data(&response);
        for error in &response.errors {
            let _ = writeln!(stderr, "{error}");
        }

        if matches!(verbosity, Verbosity::Verbose) && has_data {
            let _ = writeln!(stderr, "{}", render_response_data(&response.data));
        }

        RenderedOutput {
            stdout: String::new(),
            stderr,
        }
    }
}

fn render_response_data(data: &serde_json::Value) -> String {
    serde_json::to_string_pretty(data).unwrap_or_else(|error| {
        format!("{{\"render_error\":\"failed to serialize structured response data: {error}\"}}")
    })
}

fn render_parse_error(
    raw_args: &[OsString],
    output_mode: OutputMode,
    error: clap::Error,
) -> Result<RunResult, AppError> {
    match error.kind() {
        ErrorKind::DisplayHelp | ErrorKind::DisplayVersion => Ok(RunResult {
            output: rendered_text_output(error.to_string(), !error.use_stderr()),
            exit_status: ExitStatus::Success,
        }),
        _ => {
            let message = error.to_string().trim_end().to_string();

            if matches!(output_mode, OutputMode::Json) {
                let output = render_response(
                    output_mode,
                    Verbosity::Normal,
                    AppResponse::failure(parse_error_command(raw_args), message),
                )?;
                Ok(RunResult {
                    output,
                    exit_status: ExitStatus::ValidationFailure,
                })
            } else {
                Ok(RunResult {
                    output: rendered_text_output(message, false),
                    exit_status: ExitStatus::ValidationFailure,
                })
            }
        }
    }
}

fn rendered_text_output(message: String, stdout: bool) -> RenderedOutput {
    let mut output = message;
    if !output.ends_with('\n') {
        output.push('\n');
    }

    if stdout {
        RenderedOutput {
            stdout: output,
            stderr: String::new(),
        }
    } else {
        RenderedOutput {
            stdout: String::new(),
            stderr: output,
        }
    }
}

fn output_mode_from_raw_args(args: &[OsString]) -> OutputMode {
    if args
        .iter()
        .skip(1)
        .take_while(|argument| argument.as_os_str() != "--")
        .any(|argument| argument.as_os_str() == "--json")
    {
        OutputMode::Json
    } else {
        OutputMode::Human
    }
}

fn parse_error_command(args: &[OsString]) -> &'static str {
    let mut expect_value = false;
    let mut tokens = args.iter().skip(1).map(|arg| arg.to_string_lossy());

    while let Some(token) = tokens.next() {
        if expect_value {
            expect_value = false;
            continue;
        }

        if token == "--" {
            break;
        }

        if token == "--name" || token == "--scope" || token == "--target" || token == "--cwd" {
            expect_value = true;
            continue;
        }

        if token.starts_with("--name=")
            || token.starts_with("--scope=")
            || token.starts_with("--target=")
            || token.starts_with("--cwd=")
        {
            continue;
        }

        if token.starts_with('-') {
            continue;
        }

        return match token.as_ref() {
            "init" => "init",
            "list" => "list",
            "install" | "i" => "install",
            "remove" => "remove",
            "sync" => "sync",
            "update" => "update",
            "pin" => "pin",
            "rollback" => "rollback",
            "history" => "history",
            "doctor" => "doctor",
            "explain" => "explain",
            "override" => "override",
            "fork" => "fork",
            "enable" => "enable",
            "disable" => "disable",
            "path" => "path",
            "validate" => "validate",
            "clean" => "clean",
            "tui" => "tui",
            "telemetry" => parse_nested_command(tokens),
            "mcp" => parse_nested_command(tokens),
            _ => "skillctl",
        };
    }

    "skillctl"
}

fn parse_nested_command<'a, I>(tokens: I) -> &'static str
where
    I: Iterator<Item = std::borrow::Cow<'a, str>>,
{
    for token in tokens {
        if token == "--" {
            break;
        }
        if token.starts_with('-') {
            continue;
        }
        return match token.as_ref() {
            "status" => "telemetry-status",
            "enable" => "telemetry-enable",
            "disable" => "telemetry-disable",
            "serve" => "mcp-serve",
            _ => "skillctl",
        };
    }

    "skillctl"
}

fn response_has_data(response: &AppResponse) -> bool {
    match &response.data {
        serde_json::Value::Null => false,
        serde_json::Value::Array(items) => !items.is_empty(),
        serde_json::Value::Object(fields) => !fields.is_empty(),
        _ => true,
    }
}

fn help_text() -> String {
    let mut command = Cli::command();
    let mut buffer = Vec::new();
    if command.write_long_help(&mut buffer).is_err() {
        return "failed to render help text\n".to_string();
    }

    let mut help = String::from_utf8_lossy(&buffer).into_owned();
    help.push('\n');
    help
}
