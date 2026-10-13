//! Subcommand implementations.
//!
//! `main.rs` only parses, dispatches and maps errors to exit codes. Everything
//! a user actually runs lives here, one file per command, so each can be
//! reviewed without reading the others.

pub mod auth;
pub mod backends;
pub mod learn;
pub mod mcp;
pub mod memory;
pub mod models;
pub mod ocr;
pub mod print;
pub mod profile;
pub mod schedule;
pub mod sessions;
pub mod tui;
pub mod usage;
pub mod watch;

use std::io::IsTerminal;

use crate::cli::{Cli, Command, MemoryAction, ScheduleAction, WatchScope};
use crate::config::Config;
use crate::error::{PieError, Result};
use crate::out::outln;

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
        // `pie tui` is the one command that DOES take `cfg`, because it drives
        // real turns: it resolves the backend, loads the same persona projection
        // `pie -p` does, and records each turn to the same log and session store.
        // It refuses to start without a terminal, so this arm is safe to reach
        // from a pipe.
        Some(Command::Tui) => tui::run(cfg).await,
        // `pie learn` is the one M7 command that takes `cfg`, because it is the one that
        // can reach a provider. It is quota-free unless `--model` is given, and `--model`
        // is refused without a configured budget *before* anything spawns.
        Some(Command::Learn {
            dry_run,
            report,
            model,
        }) => {
            learn::run(
                cfg,
                &learn::Options {
                    dry_run: *dry_run,
                    report: *report,
                    model: model.clone(),
                },
            )
            .await
        }
        // No `cfg`, and that is the point: `pie memory` reads and writes pie's own memory
        // directory and resolves no backend, so "spends nothing" is a property of its
        // inputs rather than a promise about its behaviour.
        Some(Command::Memory { action }) => match action {
            Some(MemoryAction::Revert { id }) => memory::revert(id),
            _ => memory::show(),
        },
        // `status` is quota-free; `run` goes through `schedule::run_job`, which consults
        // the budget *before* spawning and refuses under the shipped zero budget.
        Some(Command::Schedule { action }) => match action {
            Some(ScheduleAction::Run { job }) => schedule::run(cfg, Some(job)).await,
            _ => schedule::run(cfg, None).await,
        },
        // The watchtower's two halves are opt-in independently: an empty `news_sources`
        // means the run fetches nothing, and the shipped cadence/budget mean a *scheduled*
        // run refuses before probing. `sources_from` names each URL after its host so the
        // report has something readable without a second config key.
        Some(Command::Watch {
            scope,
            dry_run,
            month,
        }) => {
            watch::run(
                cfg,
                &watch::Options {
                    scope: scope.unwrap_or(WatchScope::All).into(),
                    dry_run: *dry_run,
                    month: month.clone(),
                    sources: watch::sources_from(&cfg.jobs.news_sources),
                    stack: cfg.jobs.watch_stack.clone(),
                },
            )
            .await
        }
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
        // The bare invocation is the one place the default moved (`DESIGN.md` §21.1):
        // it opens the TUI when **both** streams are terminals, and prints help
        // otherwise. M6's guarantee was "the TUI is not the default"; M12's is "it
        // requires a terminal on both streams". Same outcome, different mechanism —
        // and the mechanism is weaker unless it is asserted, which is why the
        // decision is a function of the two facts rather than an inline pair of
        // `is_terminal()` calls: the terminal branch cannot be reached on a machine
        // with no pty, but the function can (`ACCEPTANCE.md` §20, row C).
        None => match cli.print.as_deref() {
            Some(prompt) => print::run(cfg, cli, prompt).await,
            None if enters_tui(
                std::io::stdin().is_terminal(),
                std::io::stdout().is_terminal(),
            ) =>
            {
                // Deliberately `pie tui`'s own entry point rather than a second one:
                // `tui::run` re-checks the same two facts before it touches the
                // terminal and refuses with the message that branch already owns, so
                // this arm cannot start a screen `pie tui` would have declined.
                tui::run(cfg).await
            }
            None => print_help(),
        },
    }
}

/// Does a **bare** `pie` open the TUI?
///
/// Yes in exactly one case: stdin **and** stdout are both terminals. Anything else —
/// a pipe on either stream, a file, a null device, a CI runner — gets help.
///
/// This is a function rather than an expression inside [`dispatch`] for the reason
/// `ACCEPTANCE.md` §20 records: the terminal branch cannot be exercised on a machine
/// with no pty to allocate, so the decision is factored out where a test can assert it
/// in both directions, and only the pipe branch is asserted end-to-end. `pie tui`
/// keeps its own check (`tui::ensure_terminal`), so an explicit request is still
/// refused with its existing message; this function governs the default alone.
pub fn enters_tui(stdin_is_terminal: bool, stdout_is_terminal: bool) -> bool {
    stdin_is_terminal && stdout_is_terminal
}

/// Nothing to run: show help instead of failing.
fn print_help() -> Result<()> {
    use clap::CommandFactory;

    Cli::command()
        .print_help()
        .map_err(|e| PieError::Config(format!("could not print help: {e}")))?;
    outln!();
    Ok(())
}
