//! GreenTyper product entry point.

mod cli;

use std::process::ExitCode;

fn main() -> ExitCode {
    match cli::run(std::env::args().skip(1)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("greentyper: {error}");
            ExitCode::FAILURE
        }
    }
}
