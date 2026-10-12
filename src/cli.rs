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
    ///
    /// `global = true` so it is accepted on either side of a subcommand: `pie ocr
    /// file --json` now works as well as `pie --json ocr file`. It was top-level
    /// only, which meant the flag silently had to precede the subcommand.
    #[arg(long, global = true)]
    pub json: bool,

    /// Resume a previous conversation by id.
    ///
    /// The id is backend-specific: agy's `--conversation`, codex's `exec resume`.
    /// `pie` prints the id it was given after every turn, and `--json` carries it
    /// in the `init` event, so a caller can pick it up and pass it back.
    #[arg(long, value_name = "ID")]
    pub resume: Option<String>,

    /// Continue the newest session for this provider and working directory.
    ///
    /// Opt-in on purpose: a plain `pie -p` starts a fresh conversation, because
    /// resume is context continuity rather than a latency win and reusing a
    /// conversation the user did not ask for can answer in the wrong context
    /// (`DESIGN.md` §11). Combining it with `--session` or `--resume` is a
    /// configuration error.
    #[arg(long = "continue")]
    pub continue_session: bool,

    /// Continue one recorded session, named by pie's own id.
    ///
    /// The id is the one pie prints after every turn as `[session] <id>`, and the
    /// one `pie sessions` lists. The vendor, not pie, owns the actual
    /// conversation, so the id to hand back is read from that session's own
    /// transcript.
    #[arg(long = "session", value_name = "ID")]
    pub session: Option<String>,

    /// Continue this session on a different backend, re-bootstrapping from pie's
    /// own transcript.
    ///
    /// `--switch codex --continue` re-sends the conversation the session already
    /// recorded as a fresh prompt for codex: pie's own transcript, compacted once
    /// it is long enough, and then the new task. A vendor conversation id belongs
    /// to the backend that minted it, so the old one is never handed over — the
    /// id the new backend returns is recorded on the same session's next turn.
    /// Combines with `--continue` or `--session`, and is a configuration error
    /// with `--resume`: naming a vendor conversation and leaving its backend are
    /// contradictory.
    #[arg(long, value_name = "BACKEND")]
    pub switch: Option<String>,

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

    /// List the recorded sessions, newest first.
    ///
    /// Reads pie's own `sessions/index.jsonl` only: nothing is spawned, no quota
    /// is spent, and the command writes no session row of its own.
    Sessions,

    /// Convert an image or scanned PDF to text through itsmini.
    ///
    /// Spawns no vendor CLI and spends no model quota: the work happens on
    /// itsmini, and the answer is cached by content hash under
    /// `$PIE_HOME/ocr-cache/`, so the same image is converted once. `--json` is
    /// the top-level flag: `pie --json ocr <file>`.
    Ocr {
        /// Image or scanned PDF to convert.
        #[arg(value_name = "FILE")]
        file: PathBuf,
        /// Language hint passed to itsmini (e.g. `en`, `ch`).
        #[arg(long, value_name = "CODE")]
        lang: Option<String>,
        /// Output format requested from itsmini (default `text`).
        #[arg(long, value_name = "FORMAT")]
        output: Option<String>,
        /// Ignore the cache and ask itsmini again.
        #[arg(long)]
        refresh: bool,
    },

    /// Host a pie capability as an MCP server on stdio, for the delegate CLIs.
    ///
    /// In delegate mode `agy` and `codex` run their own tools, so a pie-native
    /// tool never reaches them; pointing each CLI at this server through that
    /// CLI's own MCP config is the only route that serves all three backends
    /// uniformly (`DESIGN.md` §11).
    ///
    /// Opt-in on purpose, and it advertises exactly one tool: an advertised tool
    /// costs the vendor input tokens on **every** turn of the session, including
    /// turns that never call it.
    Mcp {
        /// The capability to expose. `ocr` is the only server pie hosts.
        #[arg(value_name = "CAPABILITY")]
        capability: String,
    },
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
        assert!(matches!(
            Cli::try_parse_from(["pie", "sessions"]).unwrap().command,
            Some(Command::Sessions)
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

    /// The two M5 selectors parse. Whether they may be *combined* is decided in
    /// `commands::print`, not by clap, so the error names the conflicting flags
    /// as a config error (exit 2) rather than as clap's usage text.
    #[test]
    fn the_session_selectors_parse() {
        let cli = Cli::try_parse_from(["pie", "-p", "hi", "--continue"]).unwrap();
        assert!(cli.continue_session);
        assert!(cli.session.is_none());
        assert!(cli.resume.is_none());

        let cli = Cli::try_parse_from(["pie", "-p", "hi", "--session", "s-1-2"]).unwrap();
        assert_eq!(cli.session.as_deref(), Some("s-1-2"));
        assert!(!cli.continue_session);

        // They are accepted together by the parser so that pie, not clap, owns
        // the message; `commands::print::select` is what refuses them.
        let cli =
            Cli::try_parse_from(["pie", "-p", "hi", "--continue", "--session", "s-1-2"]).unwrap();
        assert!(cli.continue_session && cli.session.is_some());
    }
}
