//! `pie sessions` — list the sessions pie has recorded.
//!
//! A view over `$PIE_HOME/sessions/index.jsonl` only. That is the whole reason
//! the index exists (`DESIGN.md` §11): listing sessions must not mean reading
//! every transcript, and it must not mean spawning anything either. Nothing here
//! spends quota, and the command records no session row of its own —
//! `tests/sessions.rs` asserts both, including a static check that this file has
//! no path to a spawn and no path to a transcript write.

use crate::error::Result;
use crate::paths;
use crate::session;
use crate::session_log::now_millis;

/// Column widths. The preview gets the rest of the line.
const ID_WIDTH: usize = 26;
const PROVIDER_WIDTH: usize = 8;
const TURNS_WIDTH: usize = 5;
const AGE_WIDTH: usize = 9;

/// `pie sessions`
pub fn run() -> Result<()> {
    let store = paths::sessions_dir();
    let sessions = session::list()?;

    if sessions.is_empty() {
        // Not an error and not an empty table: the useful thing to say is how a
        // session gets recorded in the first place, and where the store is.
        println!("no sessions recorded yet — `pie -p <PROMPT>` records one");
        println!("store: {}", store.display());
        return Ok(());
    }

    println!(
        "{:<ID_WIDTH$} {:<PROVIDER_WIDTH$} {:>TURNS_WIDTH$} {:>AGE_WIDTH$} PREVIEW",
        "SESSION", "PROVIDER", "TURNS", "UPDATED"
    );

    let now = now_millis();
    for summary in &sessions {
        println!(
            "{:<ID_WIDTH$} {:<PROVIDER_WIDTH$} {:>TURNS_WIDTH$} {:>AGE_WIDTH$} {}",
            fit(&summary.id, ID_WIDTH),
            fit(&summary.provider, PROVIDER_WIDTH),
            summary.turns,
            age_label(now, summary.updated_at),
            summary.preview,
        );
    }

    println!();
    println!("{} session(s) in {}", sessions.len(), store.display());
    Ok(())
}

/// `updated_at` as an age, because "1776000000000" is not a time a human reads.
///
/// The absolute value stays in the index (and in the id, which carries its mint
/// timestamp), so rendering it relatively here loses nothing.
fn age_label(now: u64, updated_at: u64) -> String {
    const MINUTE: u64 = 60_000;
    const HOUR: u64 = 60 * MINUTE;
    const DAY: u64 = 24 * HOUR;

    // Saturating: a clock that moved backwards (or a hand-edited index) must not
    // produce a nonsense age or a panic.
    let age = now.saturating_sub(updated_at);

    match age {
        a if a < MINUTE => "just now".to_string(),
        a if a < HOUR => format!("{}m ago", a / MINUTE),
        a if a < DAY => format!("{}h ago", a / HOUR),
        a => format!("{}d ago", a / DAY),
    }
}

/// Truncate a cell to `width`, marking the cut.
fn fit(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_string();
    }
    let kept: String = text.chars().take(width.saturating_sub(1)).collect();
    format!("{kept}…")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn ages_are_rendered_in_the_largest_useful_unit() {
        let now = 1_000_000_000_000u64;
        assert_eq!(age_label(now, now), "just now");
        assert_eq!(age_label(now, now - 59_000), "just now");
        assert_eq!(age_label(now, now - 60_000), "1m ago");
        assert_eq!(age_label(now, now - 3_600_000), "1h ago");
        assert_eq!(age_label(now, now - 86_400_000), "1d ago");
        assert_eq!(age_label(now, now - 10 * 86_400_000), "10d ago");
    }

    /// A clock that moved backwards must produce a readable label, not a panic.
    #[test]
    fn a_future_timestamp_reads_as_just_now() {
        assert_eq!(age_label(1_000, 9_999), "just now");
    }

    #[test]
    fn cells_are_truncated_on_a_char_boundary() {
        assert_eq!(fit("agy", 8), "agy");
        assert_eq!(fit(&"é".repeat(20), 5).chars().count(), 5);
        assert!(fit(&"x".repeat(30), ID_WIDTH).ends_with('…'));
    }
}
