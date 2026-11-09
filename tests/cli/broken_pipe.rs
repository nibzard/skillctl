//! Process exit behavior when the reader of an output pipe closes early.
//!
//! `print!` and `eprint!` panic on a broken pipe, which replaces the
//! documented exit code with 101. These tests pin the quiet behavior: a reader
//! that closed the pipe is not a command failure, and a command that already
//! failed keeps the exit status it documented.

use std::{fs::File, os::fd::OwnedFd, os::unix::net::UnixStream, process::Stdio};

use super::*;

/// Run the binary with one output stream connected to a socket whose only
/// reader is already gone, so every write on that stream fails with a broken
/// pipe no matter how small the payload is.
fn run_with_closed_stream(arguments: &[&str], close_stdout: bool) -> std::process::Output {
    let (writer, reader) = UnixStream::pair().expect("socket pair is created");
    drop(reader);

    let closed = Stdio::from(File::from(OwnedFd::from(writer)));
    let mut command = ProcessCommand::new(env!("CARGO_BIN_EXE_skillctl"));
    command.stdin(Stdio::null()).args(arguments);
    if close_stdout {
        command.stdout(closed);
    } else {
        command.stderr(closed);
    }
    command.output().expect("binary launches")
}

#[test]
fn closed_stdout_pipe_exits_successfully_without_panicking() {
    let output = run_with_closed_stream(&["--help"], true);

    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        output.stderr.is_empty(),
        "a closed pipe reader must not print a panic: {}",
        String::from_utf8_lossy(&output.stderr),
    );
}

/// A failing command must keep its documented status even when its rendered
/// payload meets a closed pipe, so a CI pipeline cannot pass silently on a
/// command that failed.
#[test]
fn closed_stderr_pipe_keeps_the_failing_command_status() {
    // `explain` without its required skill fails clap parsing in human mode,
    // so the rendered payload goes to stderr.
    let output = run_with_closed_stream(&["explain"], false);

    assert_eq!(
        output.status.code(),
        Some(3),
        "stdout: {}",
        String::from_utf8_lossy(&output.stdout),
    );
    assert!(output.stdout.is_empty());
}

/// The mirror of the stderr case: a JSON-mode parse error renders its failure
/// payload to stdout, so a closed stdout reader must not turn the documented
/// validation failure into a passing pipeline either.
#[test]
fn closed_stdout_pipe_keeps_the_failing_command_status() {
    let output = run_with_closed_stream(&["--json", "explain"], true);

    assert_eq!(
        output.status.code(),
        Some(3),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn failing_command_with_open_streams_keeps_its_documented_status() {
    let output = ProcessCommand::new(env!("CARGO_BIN_EXE_skillctl"))
        .stdin(Stdio::null())
        .args(["explain"])
        .output()
        .expect("binary launches");

    assert_eq!(
        output.status.code(),
        Some(3),
        "stdout: {} stderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}
