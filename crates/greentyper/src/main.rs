//! GreenTyper product entry point.

mod app_server;
mod cli;
mod credential_vault;
mod local_process;
mod presentation;
mod product_driver;
mod provider_connection;
mod provider_discovery_catalog;
mod provider_discovery_task;
mod provider_http;
mod provider_http_policy;
mod terminal;

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
