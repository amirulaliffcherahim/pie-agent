//! Subcommand implementations.
//!
//! `main.rs` only parses, dispatches and maps errors to exit codes. Everything
//! a user actually runs lives here, one file per command, so each can be
//! reviewed without reading the others.

pub mod auth;
pub mod backends;
pub mod mcp;
pub mod models;
pub mod ocr;
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
        // And no `cfg` here either: `pie ocr` reads the itsmini base URL from the
        // environment at call time, so it needs nothing resolved first.
        Some(Command::Ocr {
            file,
            lang,
            output,
            refresh,
        }) => ocr::run(file, lang.as_deref(), output.as_deref(), *refresh, cli.json).await,
        // And no `cfg` here: the MCP server reads itsmini's address and pie's cache
        // directory the same way `pie ocr` does, at call time, and speaks JSON-RPC
        // over this process's own stdin/stdout. It writes nothing to stdout that is
        // not a frame, so nothing may be printed before it starts either.
        Some(Command::Mcp { capability }) => mcp::run(capability).await,
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
