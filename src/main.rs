use std::process::ExitCode;

use clap::Parser;
use hegn::cli::{Cli, run};

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(code) => ExitCode::from(code.clamp(0, 255) as u8),
        Err(err) => {
            eprintln!("hegn: {err:#}");
            ExitCode::FAILURE
        }
    }
}
