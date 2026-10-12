//! Entry point.
//!
//! Deliberately tiny: parse, resolve configuration, hand off, map the error to
//! an exit code. All behaviour lives in the library, which is what makes the
//! commands testable and this file reviewable in ten seconds.

use clap::Parser;

use pie::cli::Cli;
use pie::commands;
use pie::config::{Config, Overrides};
use pie::error::PieError;
use pie::process;

#[tokio::main]
async fn main() {
    std::process::exit(run().await);
}

async fn run() -> i32 {
    pie::logging::init();

    let cli = Cli::parse();

    let cfg = match Config::resolve(&Overrides {
        default_model: cli.model.clone(),
        timeout_secs: cli.timeout,
    }) {
        Ok(cfg) => cfg,
        Err(error) => return fail(&error),
    };

    match commands::dispatch(&cli, &cfg).await {
        Ok(()) => 0,
        Err(error) => fail(&error),
    }
}

/// Report a failure and return its exit code.
///
/// The message is redacted on the way out: a vendor CLI's stderr can echo a
/// credential, and `pie` must never be the thing that prints it (`RISKS.md` §10).
fn fail(error: &PieError) -> i32 {
    eprintln!("pie: {}", process::redact(&error.to_string()));
    error.exit_code()
}
