use std::{env, process::ExitCode};

fn main() -> ExitCode {
    match skillctl::run_from_args(env::args_os()) {
        Ok(result) => {
            if !result.output.stdout.is_empty() {
                print!("{}", result.output.stdout);
            }

            if !result.output.stderr.is_empty() {
                eprint!("{}", result.output.stderr);
            }

            ExitCode::from(result.exit_status.code())
        }
        Err(error) => {
            eprintln!("{error}");
            ExitCode::from(error.exit_status().code())
        }
    }
}
