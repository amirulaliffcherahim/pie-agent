//! Command-line surface.
//!
//! Definitions only — every behaviour lives in `main.rs` or the library.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

/// One profile over the subscription-locked coding-agent CLIs.
#[derive(Debug, Parser)]
#[command(
    name = "pie",
    version,
    about = "One profile over agy, codex and claude — driving each vendor's own CLI, never an API key",
    long_about = None,
    after_help = "Subscription-only: pie authenticates through each CLI's own login and never \
                  injects an API key.\n\
                  Examples:\n  \
                  pie --list-backends\n  \
                  pie -p --model agy/gemini-3.8-flash \"summarise this repo\"\n  \
                  pie usage agy\n  \
                  pie doctor"
)]
pub struct Cli {
    /// Run one prompt and exit (delegate mode).
    #[arg(short = 'p', long = "print", value_name = "PROMPT")]
    pub print: Option<String>,

    /// Model as `provider/model`, e.g. `agy/gemini-3.8-flash`.
    #[arg(short = 'm', long = "model", value_name = "PROVIDER/MODEL")]
    pub model: Option<String>,

    /// Reasoning effort, backend-specific.
    #[arg(long, value_name = "EFFORT")]
    pub effort: Option<String>,

    /// Whole-turn deadline in seconds.
    #[arg(long, value_name = "SECONDS")]
    pub timeout: Option<u64>,

    /// Working directory handed to the vendor CLI.
    #[arg(long, value_name = "DIR")]
    pub cwd: Option<PathBuf>,

    /// Emit one JSON object per line instead of human text.
    #[arg(long)]
    pub json: bool,

    /// Resume a previous conversation by id.
    ///
    /// The id is backend-specific: agy's `--conversation`, codex's `exec resume`.
    /// `pie` prints the id it was given after every turn, and `--json` carries it
    /// in the `init` event, so a caller can pick it up and pass it back.
    #[arg(long, value_name = "ID")]
    pub resume: Option<String>,

    /// Print a short argument-error hint.
    #[arg(long, hide = true)]
    pub explain_errors: bool,

    /// List the known backends with their auth state.
    #[arg(long = "list-backends")]
    pub list_backends: bool,

    /// Subcommand, when not running a prompt.
    #[command(subcommand)]
    pub command: Option<Command>,
}

/// Everything that is not a one-shot prompt.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// List models for one backend (or all).
    Models {
        /// Restrict to one backend.
        #[arg(long, value_name = "BACKEND")]
        backend: Option<String>,
        /// Ignore the on-disk cache and ask the CLI again.
        #[arg(long)]
        refresh: bool,
    },

    /// Show auth state for every backend.
    Auth,

    /// Show quota and pie's own session-log totals.
    Usage {
        /// Restrict to one backend.
        backend: Option<String>,
    },

    /// Check binaries, versions and auth without spending quota.
    Doctor,

    /// Show the assembled persona projection and where each part came from.
    ///
    /// Reads pie's own files only: no vendor CLI is spawned and no session row
    /// is written, so this costs nothing to run.
    Profile,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn list_backends_flag_parses() {
        let cli = Cli::try_parse_from(["pie", "--list-backends"]).unwrap();
        assert!(cli.list_backends);
        assert!(cli.print.is_none());
    }

    #[test]
    fn print_mode_with_model_parses() {
        let cli = Cli::try_parse_from([
            "pie",
            "-p",
            "hi",
            "--model",
            "agy/gemini-3.8-flash",
            "--effort",
            "low",
            "--json",
        ])
        .unwrap();
        assert_eq!(cli.print.as_deref(), Some("hi"));
        assert_eq!(cli.model.as_deref(), Some("agy/gemini-3.8-flash"));
        assert_eq!(cli.effort.as_deref(), Some("low"));
        assert!(cli.json);
    }

    #[test]
    fn subcommands_parse() {
        let cli = Cli::try_parse_from(["pie", "usage", "codex"]).unwrap();
        match cli.command {
            Some(Command::Usage { backend }) => assert_eq!(backend.as_deref(), Some("codex")),
            other => panic!("wrong command: {other:?}"),
        }

        let cli = Cli::try_parse_from(["pie", "models", "--refresh"]).unwrap();
        match cli.command {
            Some(Command::Models { refresh, backend }) => {
                assert!(refresh);
                assert!(backend.is_none());
            }
            other => panic!("wrong command: {other:?}"),
        }

        assert!(matches!(
            Cli::try_parse_from(["pie", "auth"]).unwrap().command,
            Some(Command::Auth)
        ));
        assert!(matches!(
            Cli::try_parse_from(["pie", "doctor"]).unwrap().command,
            Some(Command::Doctor)
        ));
        assert!(matches!(
            Cli::try_parse_from(["pie", "profile"]).unwrap().command,
            Some(Command::Profile)
        ));
    }

    #[test]
    fn a_bare_invocation_is_valid_so_we_can_print_help() {
        let cli = Cli::try_parse_from(["pie"]).unwrap();
        assert!(cli.print.is_none());
        assert!(cli.command.is_none());
        assert!(!cli.list_backends);
    }

    #[test]
    fn unknown_flags_are_rejected() {
        assert!(Cli::try_parse_from(["pie", "--nope"]).is_err());
    }
}
