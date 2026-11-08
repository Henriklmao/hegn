use std::process::ExitCode;

use clap::Parser;
use hegn::cli::{Cli, run};

fn main() -> ExitCode {
    // The detached console relay re-executes this binary with a private first
    // argument. It is intentionally not part of the clap surface.
    if hegn::session::relay::requested() {
        return match hegn::session::relay::run_from_current_args() {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("hegn relay: {err:#}");
                ExitCode::FAILURE
            }
        };
    }
    // The detached periodic-snapshot worker uses the same private re-exec
    // mechanism as the console relay.
    if hegn::vm::instance::autosave::requested() {
        return match hegn::vm::instance::autosave::run_from_current_args() {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("hegn autosnapshot: {err:#}");
                ExitCode::FAILURE
            }
        };
    }

    let cli = Cli::parse();
    match run(cli) {
        Ok(code) => ExitCode::from(code.clamp(0, 255) as u8),
        Err(err) => {
            eprintln!("hegn: {err:#}");
            ExitCode::FAILURE
        }
    }
}
