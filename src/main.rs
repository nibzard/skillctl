//! Process entry point that turns one rendered result into terminal bytes.

use std::{
    env,
    io::{self, Write},
    process::ExitCode,
};

use skillctl::error::ExitStatus;

fn main() -> ExitCode {
    match skillctl::run_from_args(env::args_os()) {
        Ok(result) => match write_output(&result.output.stdout, &result.output.stderr) {
            Ok(()) => ExitCode::from(result.exit_status.code()),
            Err(error) => exit_status_for_write_error(error, result.exit_status),
        },
        Err(error) => {
            // The command already carries a failure status. A closed stream
            // leaves nowhere to report, so the write result cannot improve the
            // outcome; it must only stay quiet instead of panicking.
            let message = format!("{error}\n");
            let _ = io::stderr().write_all(message.as_bytes());
            ExitCode::from(error.exit_status().code())
        }
    }
}

/// Write the rendered bytes to both terminal streams without panicking macros.
///
/// `print!` and `eprint!` panic when a write fails, which turns a reader that
/// closed the pipe early (`skillctl history --json | head`) into exit code
/// 101 instead of a documented status.
fn write_output(stdout: &str, stderr: &str) -> io::Result<()> {
    let mut out = io::stdout();
    out.write_all(stdout.as_bytes())?;
    out.flush()?;

    let mut err = io::stderr();
    err.write_all(stderr.as_bytes())?;
    err.flush()
}

/// Map one terminal write failure to a process status.
///
/// A reader that closed the pipe is not a command failure, so the command's
/// own status stands: a command that succeeded still exits with success, and a
/// command that failed keeps its documented status instead of turning a failed
/// pipeline into a passing one. Any other write failure means the rendered
/// result never reached the user, so a successful command reports an
/// operational failure, and a failing command keeps the failure it already
/// carried because the write result cannot improve the outcome.
fn exit_status_for_write_error(error: io::Error, command_status: ExitStatus) -> ExitCode {
    if error.kind() == io::ErrorKind::BrokenPipe {
        return ExitCode::from(command_status.code());
    }
    match command_status {
        ExitStatus::Success => ExitCode::FAILURE,
        failing => ExitCode::from(failing.code()),
    }
}
