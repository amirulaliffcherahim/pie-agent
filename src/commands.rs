//! Subcommand implementations.
//!
//! `main.rs` only parses, dispatches and maps errors to exit codes. Everything
//! a user actually runs lives here, one file per command, so each can be
//! reviewed without reading the others.

pub mod auth;
pub mod backends;
pub mod models;
pub mod print;
pub mod profile;
pub mod sessions;
pub mod usage;

use crate::cli::{Cli, Command};
use crate::config::Config;
use crate::error::{PieError, Result};

/// Route a parsed command line to its implementation.
pub async fn dispatch(cli: &Cli, cfg: &Config) -> Result<()> {
    // `--list-backends` is a flag, not a subcommand, so it wins over a bare
    // invocation but not over an explicit subcommand.
    if cli.list_backends {
        return backends::list(cfg).await;
    }

    match &cli.command {
        Some(Command::Auth) => auth::run(cfg).await,
        Some(Command::Models { backend, refresh }) => {
            models::run(cfg, backend.as_deref(), *refresh).await
        }
        Some(Command::Usage { backend }) => usage::run(cfg, backend.as_deref()).await,
        Some(Command::Doctor) => backends::doctor(cfg).await,
        // No `cfg`: `pie profile` never resolves a binary, so configuration is
        // not part of its input. Deliberate — it keeps the command provably
        // free of any spawn.
        Some(Command::Profile) => profile::run(),
        // Likewise no `cfg`: `pie sessions` reads pie's own index and nothing
        // else, so it is quota-free and spawn-free by construction.
        Some(Command::Sessions) => sessions::run(),
        None => match cli.print.as_deref() {
            Some(prompt) => print::run(cfg, cli, prompt).await,
            None => print_help(),
        },
    }
}

/// Nothing to run: show help instead of failing.
fn print_help() -> Result<()> {
    use clap::CommandFactory;

    Cli::command()
        .print_help()
        .map_err(|e| PieError::Config(format!("could not print help: {e}")))?;
    println!();
    Ok(())
}
