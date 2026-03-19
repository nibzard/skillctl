use std::process::ExitCode;

use clap::Parser;
use skillctl::cli::Cli;

fn main() -> ExitCode {
    let cli = Cli::parse();

    match skillctl::run(cli) {
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
