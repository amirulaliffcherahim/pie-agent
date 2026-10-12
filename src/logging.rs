//! Logging.
//!
//! **stdout is the protocol channel** — it carries the streamed answer and, in
//! `--json` mode, machine-readable events. All diagnostics therefore go to
//! stderr, so a caller can pipe stdout straight into a parser.

use tracing_subscriber::EnvFilter;

/// Install the global subscriber. Safe to call once; later calls are ignored.
///
/// Verbosity comes from `PIE_LOG`, then `RUST_LOG`, then a default of `warn`.
pub fn init() {
    let filter = std::env::var("PIE_LOG")
        .or_else(|_| std::env::var("RUST_LOG"))
        .unwrap_or_else(|_| "pie=info,warn".to_string());

    let filter = EnvFilter::try_new(&filter).unwrap_or_else(|_| EnvFilter::new("warn"));

    // `try_init` so a second call (e.g. from a test harness) is a no-op instead
    // of a panic.
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(filter)
        .with_target(false)
        .with_ansi(crate::config::color_enabled())
        .try_init();
}
