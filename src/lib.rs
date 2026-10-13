//! pie — one profile over the subscription-locked coding-agent CLIs.
//!
//! `pie` does not reimplement any vendor API. It spawns each vendor's own CLI
//! (`agy --print`, `codex exec --json`, `claude -p`) with that CLI's own login
//! and quota, and normalises the resulting streams into one shape. See
//! `planning/PLAN.md` for the why and `planning/DESIGN.md` for the contract.
//!
//! Invariants enforced here (see `planning/ACCEPTANCE.md` §2):
//! * no console window is ever shown for a child process (`proc`)
//! * API-key env vars never reach a child (`config::scrub_keys`)
//! * a malformed stdout line never aborts an otherwise healthy run (`jsonl`)
//! * no panic on external input — hence the `deny` below

#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)
)]
#![warn(missing_debug_implementations)]

pub mod auth;
pub mod cache;
pub mod cli;
pub mod commands;
pub mod compact;
pub mod config;
pub mod error;
pub mod jsonl;
pub mod learn;
pub mod logging;
pub mod mcp;
pub mod memory;
pub mod ocr;
pub mod out;
pub mod paths;
pub mod process;
pub mod profile;
pub mod provider;
pub mod report;
pub mod schedule;
pub mod session;
pub mod session_log;
pub mod tool_loop;
pub mod tools;
pub mod tui;
pub mod watch;

/// Test-only helpers. Compiled out of every non-test build.
#[cfg(test)]
pub(crate) mod testing;

/// Crate version, recorded for CLI-drift detection (`RISKS.md` §3).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Human-facing name used in tables and error prefixes.
pub const NAME: &str = "pie";
