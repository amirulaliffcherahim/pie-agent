//! Entry point.
//!
//! Deliberately tiny: parse, resolve configuration, hand off, map the error to
//! an exit code. All behaviour lives in the library, which is what makes the
//! commands testable and this file reviewable in ten seconds.

#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)
)]

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

    // Warned here rather than only in `doctor`: an inherited `PIE_STRIP_ENV=0`
    // (shell profile, parent process) used to leave `pie -p` running with the
    // credential scrub silently disabled, and the warning was visible only to
    // someone who happened to run `pie doctor`. stderr, so stdout stays a
    // protocol channel.
    if !cfg.scrub_enabled() {
        eprintln!(
            "pie: WARNING: PIE_STRIP_ENV is set, so API-key variables are NOT stripped from \
             child processes. A vendor CLI may switch from your subscription to \
             pay-as-you-go billing. Unset PIE_STRIP_ENV to restore the scrub."
        );
    }

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
