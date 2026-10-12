//! Extractive, quota-free transcript compaction (M5 stage C).
//!
//! `DESIGN.md` §11: past `agent.compact_after_turns` turns, older turns collapse
//! into a *state block*. The collapse is **extractive** — the head and a tail
//! window are kept, and the middle is replaced by a structured list of what was
//! asked and what changed — because a model-based summary would spend
//! subscription quota on the user's behalf without asking (`RISKS.md` §13).
//! Summarising history with a model is M7's `learn` job, which owns exactly that
//! decision; compaction owns the free half of it.
//!
//! # What this module is, and is not
//!
//! A pure function over already-loaded [`TurnRow`]s. No I/O, no child process,
//! no model call, no clock, no randomness:
//!
//! * **Deterministic.** The same rows and the same budget produce byte-identical
//!   text. Nothing ambient may enter it — no timestamp, session id, cwd, model id
//!   or quota figure. That is the rule the profile projection already obeys
//!   (`DESIGN.md` §10), for the reason `TOKEN-ECONOMICS.md` §4 gives: the
//!   vendor's prefix cache is a *leading byte run*, so a single volatile value in
//!   the sent text invalidates the whole cached prefix on every invocation.
//! * **Non-destructive.** A stored row is never deleted, rewritten or reordered.
//!   That is not a promise in a comment: [`compact`] takes a shared slice and
//!   hands the elided rows *back* to the caller as borrows ([`Compacted::dropped`]),
//!   so the transcript stays recoverable by inspection and the function could not
//!   delete anything even if it tried. Compaction changes what is *sent*, never
//!   what is *kept*.
//! * **Extractive.** Nothing is paraphrased. [`Compacted::head`] and
//!   [`Compacted::tail`] are the transcript's own text, and the block is built
//!   only from the rows it stands in for.
//!
//! Thresholds arrive as arguments. This module reads no configuration: the caller
//! supplies a [`Budget`] and decides when compaction is due ([`is_due`]);
//! [`Budget::default`] and [`DEFAULT_COMPACT_AFTER_TURNS`] exist so a caller has
//! something defensible to start from.
//!
//! # Ordering is a correctness rule, not tidiness
//!
//! The pieces must be placed in exactly one order, and [`assemble`] is that order:
//!
//! ```text
//! persona   static     the profile projection — byte-identical every turn
//! head      stable     the first turns: written once, then constant
//! block     volatile   the extracted state block
//! tail      volatile   the sliding tail window
//! task      volatile   the user's new request
//! ```
//!
//! Volatility is monotone — nothing stable may follow something volatile — so the
//! block sits **after** the static persona and **before** the volatile task text.
//! A payload that appends at the end keeps whatever prefix the vendor cached; a
//! payload that inserts near the front throws it away. That is the whole reason
//! the block has a fixed position instead of being prepended to the task.
//!
//! The caller's obligation, stated once: the compacted history goes in front of
//! the new task text and after the persona. For agy and codex the persona is
//! folded in front of the prompt by the provider (`DESIGN.md` §10), so that caller
//! passes `None` here and lets the provider do it; for claude the persona travels
//! in `--append-system-prompt`, which is also in front. Passing `Some(persona)`
//! folds it here with [`crate::profile::fold`] — the same rule the providers use —
//! so the two paths cannot drift apart.
//!
//! # What the block deliberately leaves out
//!
//! A [`TurnRow`] carries nine fields. Only four are both deterministic and about
//! the work itself: `prompt`, `response`, `ok`, `stop_reason`. The rest is either
//! volatile (`ts`, `conversation_id`) or per-row metadata that varies inside one
//! session (`provider`, `model`, `usage`), so none of it is rendered:
//!
//! * **Asked** lists the dropped turns' prompts — first non-blank line, trimmed,
//!   length-capped, with consecutive repeats collapsed.
//! * **Changed** ledgers their outcomes: how many succeeded, how many failed, and
//!   which failure classes. `DESIGN.md` §11 puts a failed turn's reason in
//!   `stop_reason`, so that is what a failure class is read from.
//!
//! Responses are *not* in the block. Half an answer inside a summary is worse than
//! no answer at all, and the elided rows come back intact for anything that needs
//! them.
//!
//! Rendered for a five-turn session with a `Budget { head: 2, tail: 2 }` — the
//! exact bytes `the_block_has_the_documented_shape` pins:
//!
//! ```text
//! ## Compacted history (1 of 5 turns elided)
//! ### Asked
//! - ask 2
//! ### Changed
//! - 1 succeeded
//! ```

use crate::profile;
use crate::session::TurnRow;

/// Turns kept verbatim at the start of a transcript by [`Budget::default`].
///
/// The head is the session's seed — the request that set the work up — and it is
/// the one part of the history that stops changing once written, which is why it
/// is kept at all rather than dropped with the middle.
pub const DEFAULT_HEAD_TURNS: usize = 2;

/// Turns kept verbatim at the end of a transcript by [`Budget::default`].
///
/// The tail carries the work in flight. It is necessarily volatile — the window
/// slides forward on every new turn — so it belongs after everything stable.
pub const DEFAULT_TAIL_TURNS: usize = 8;

/// The `agent.compact_after_turns` default from `DESIGN.md` §11.
///
/// The caller's trigger, not this module's: [`is_due`] compares a row count
/// against the value the caller supplies, because the configured value belongs to
/// `config.rs` and compaction must not read configuration on its own.
pub const DEFAULT_COMPACT_AFTER_TURNS: usize = 40;

/// Most ask lines the block carries before it reports the remainder.
///
/// The bound is the point of compaction: a 500-turn middle must not produce a
/// 500-line block. It is a count of *lines*, after consecutive repeats collapse.
pub const MAX_ASK_LINES: usize = 24;

/// Characters kept of one ask line, before the ellipsis.
pub const ASK_CHARS: usize = 120;

/// Characters kept of one failure class in the outcome ledger.
pub const REASON_CHARS: usize = 40;

/// Separator between the compacted pieces, in [`Compacted::render`].
///
/// One blank line, so the pieces read as separate blocks and no two of them can
/// merge into one line if a piece happens to end without a newline.
pub const PIECE_SEPARATOR: &str = "\n\n";

/// Heading of the state block.
pub const BLOCK_HEADING: &str = "## Compacted history";

/// Heading of the list of dropped asks.
pub const ASKED_HEADING: &str = "### Asked";

/// Heading of the outcome ledger.
pub const CHANGED_HEADING: &str = "### Changed";

/// Label in front of a retained prompt.
pub const USER_LABEL: &str = "user: ";

/// Label in front of a retained response.
pub const ASSISTANT_LABEL: &str = "assistant: ";

/// How many turns survive verbatim at each end of a transcript.
///
/// Both counts are in *turns*, and a transcript no longer than `head + tail` is
/// kept whole — nothing is elided at the threshold, only past it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budget {
    /// Turns kept from the start of the transcript.
    pub head: usize,
    /// Turns kept from the end of the transcript.
    pub tail: usize,
}

impl Budget {
    /// A budget of `head` leading and `tail` trailing turns.
    pub const fn new(head: usize, tail: usize) -> Self {
        Self { head, tail }
    }

    /// Turns that survive when a transcript is longer than this.
    pub const fn keeps(&self) -> usize {
        self.head.saturating_add(self.tail)
    }
}

impl Default for Budget {
    fn default() -> Self {
        Self::new(DEFAULT_HEAD_TURNS, DEFAULT_TAIL_TURNS)
    }
}

/// The compacted prompt prefix, as the pieces the caller must place in order.
///
/// Produced only by [`compact`], so the pieces and [`Compacted::dropped`] cannot
/// disagree about which rows were elided.
#[derive(Debug, Clone, PartialEq)]
pub struct Compacted<'a> {
    /// Turns kept verbatim from the start of the transcript. May be empty.
    pub head: String,
    /// The state block standing in for the elided middle. Empty when nothing was
    /// elided — the signal that compaction is a no-op for this transcript.
    pub block: String,
    /// Turns kept verbatim from the end of the transcript. May be empty.
    pub tail: String,
    /// Every elided row, oldest first, borrowed from the caller's own slice.
    ///
    /// The projection is not the storage: these rows are still in the
    /// transcript, and handing back borrows rather than copies is how this type
    /// states that a compaction can never have deleted them.
    pub dropped: Vec<&'a TurnRow>,
    /// Rows the transcript held, elided ones included.
    total: usize,
}

impl Compacted<'_> {
    /// Rows the transcript held.
    pub fn turns(&self) -> usize {
        self.total
    }

    /// Rows the block stands in for.
    pub fn elided(&self) -> usize {
        self.dropped.len()
    }

    /// Rows kept verbatim.
    pub fn kept(&self) -> usize {
        self.total.saturating_sub(self.dropped.len())
    }

    /// Whether every piece is blank — an empty transcript, or one with nothing in
    /// it to send.
    pub fn is_empty(&self) -> bool {
        self.head.is_empty() && self.block.is_empty() && self.tail.is_empty()
    }

    /// The three pieces in the order they must be sent: head, block, tail.
    ///
    /// The order is fixed here so a caller cannot assemble the history in a
    /// different one by accident; the persona and the task text are the caller's
    /// half of [`assemble`].
    pub fn render(&self) -> String {
        let mut pieces: Vec<&str> = Vec::new();
        for piece in [&self.head, &self.block, &self.tail] {
            if !piece.is_empty() {
                pieces.push(piece);
            }
        }
        pieces.join(PIECE_SEPARATOR)
    }
}

/// Compact a transcript: keep the head, keep a tail window, extract the middle.
///
/// Returns the pieces plus the elided rows (see [`Compacted`]). `rows` is only
/// read: the transcript on disk is untouched, and the elided rows are borrowed
/// rather than consumed.
///
/// A transcript of `budget.keeps()` rows or fewer is kept whole and `block` comes
/// back empty, so calling this below the threshold changes nothing.
pub fn compact(rows: &[TurnRow], budget: Budget) -> Compacted<'_> {
    let total = rows.len();
    let head_end = budget.head.min(total);
    // `max(head_end)` is what stops the two windows from overlapping into a
    // negative middle when the tail is longer than what is left after the head:
    // a short transcript then keeps everything, split once at the head.
    let tail_start = total.saturating_sub(budget.tail).max(head_end);

    let middle = &rows[head_end..tail_start];
    let dropped: Vec<&TurnRow> = middle.iter().collect();

    Compacted {
        head: render_turns(&rows[..head_end]),
        block: render_block(total, &dropped),
        tail: render_turns(&rows[tail_start..]),
        dropped,
        total,
    }
}

/// Whether a transcript of `turns` rows is past the compaction threshold.
///
/// `DESIGN.md` §11: *past* `agent.compact_after_turns` turns. The threshold is an
/// argument, not a constant lookup, because the configured value is
/// `config.rs`'s; [`DEFAULT_COMPACT_AFTER_TURNS`] is the contract's default.
pub fn is_due(turns: usize, compact_after: usize) -> bool {
    turns > compact_after
}

/// Compose the prompt in the one order that protects the vendor's cached prefix:
/// persona, then the compacted history, then the task text.
///
/// Pass `persona: None` when the caller hands the persona to the provider
/// separately (agy and codex fold it in front of the prompt, and would otherwise
/// receive it twice). Passing `Some(persona)` folds it here through
/// [`crate::profile::fold`], so the block still lands after the persona.
///
/// With nothing elided and no persona this returns `task` unchanged, so switching
/// a caller over to compacted histories cannot alter the payload of a session
/// that never crossed the threshold.
pub fn assemble(persona: Option<&str>, compacted: &Compacted<'_>, task: &str) -> String {
    let history = compacted.render();

    let body = match (history.is_empty(), task.trim().is_empty()) {
        (true, _) => task.to_string(),
        (false, true) => history,
        (false, false) => format!("{history}{PIECE_SEPARATOR}{task}"),
    };

    profile::fold(persona, &body)
}

/// Render the retained turns of one window, oldest first.
fn render_turns(rows: &[TurnRow]) -> String {
    let rendered: Vec<String> = rows
        .iter()
        .map(render_turn)
        .filter(|turn| !turn.is_empty())
        .collect();
    rendered.join(PIECE_SEPARATOR)
}

/// Render one retained turn, or an empty string when it recorded nothing.
///
/// Returning `""` rather than a labelled blank is the whole point: a turn that
/// asked nothing and answered nothing must contribute nothing, not a gap in the
/// transcript that the next piece then has to be joined around.
fn render_turn(row: &TurnRow) -> String {
    let prompt = row.prompt.trim();
    let response = row.response.trim();

    let mut turn = String::new();
    if !prompt.is_empty() {
        turn.push_str(USER_LABEL);
        turn.push_str(prompt);
    }
    if !response.is_empty() {
        if !turn.is_empty() {
            turn.push('\n');
        }
        turn.push_str(ASSISTANT_LABEL);
        turn.push_str(response);
    }
    turn
}

/// The state block standing in for the elided middle; empty when nothing was.
fn render_block(total: usize, dropped: &[&TurnRow]) -> String {
    if dropped.is_empty() {
        return String::new();
    }

    let elided = dropped.len();
    let mut lines: Vec<String> = vec![format!(
        "{BLOCK_HEADING} ({elided} of {total} turns elided)"
    )];

    let asks = ask_lines(dropped);
    if !asks.is_empty() {
        lines.push(ASKED_HEADING.to_string());
        lines.extend(asks);
    }

    lines.push(CHANGED_HEADING.to_string());
    let succeeded = dropped.iter().filter(|row| row.ok).count();
    if succeeded > 0 {
        lines.push(format!("- {succeeded} succeeded"));
    }
    let failed = elided.saturating_sub(succeeded);
    if failed > 0 {
        let reasons = failure_reasons(dropped);
        if reasons.is_empty() {
            lines.push(format!("- {failed} failed"));
        } else {
            lines.push(format!("- {failed} failed: {}", reasons.join(", ")));
        }
    }

    lines.join("\n")
}

/// The ask lines: one per dropped turn that recorded a prompt, capped.
///
/// Consecutive repeats collapse into a single line with a count, because a
/// session that repeats one instruction ten times should cost one line, not ten.
/// A turn with no prompt is skipped and does not break a run of repeats.
fn ask_lines(dropped: &[&TurnRow]) -> Vec<String> {
    let mut runs: Vec<(String, usize)> = Vec::new();
    for row in dropped {
        let Some(text) = ask_text(&row.prompt) else {
            continue;
        };
        match runs.last_mut() {
            Some((last, count)) if *last == text => *count = count.saturating_add(1),
            _ => runs.push((text, 1)),
        }
    }

    let mut lines: Vec<String> = Vec::new();
    let mut remaining = 0usize;
    for (index, (text, count)) in runs.iter().enumerate() {
        if index >= MAX_ASK_LINES {
            remaining = remaining.saturating_add(*count);
            continue;
        }
        if *count > 1 {
            lines.push(format!("- {text} (x{count})"));
        } else {
            lines.push(format!("- {text}"));
        }
    }
    if remaining > 0 {
        lines.push(format!("- ... and {remaining} more turns"));
    }
    lines
}

/// The first non-blank line of a prompt, trimmed and capped.
fn ask_text(prompt: &str) -> Option<String> {
    let line = prompt.lines().find(|line| !line.trim().is_empty())?;
    Some(cap(line.trim(), ASK_CHARS))
}

/// The distinct failure classes of the dropped turns, in the order they appear.
///
/// A failed turn's reason lives in `stop_reason` (`DESIGN.md` §11), so that is
/// what is read. Distinct-and-ordered rather than sorted: the first failure is
/// the one the reader needs, and the order is deterministic either way.
fn failure_reasons(dropped: &[&TurnRow]) -> Vec<String> {
    let mut reasons: Vec<String> = Vec::new();
    for row in dropped.iter().filter(|row| !row.ok) {
        let raw = row.stop_reason.trim();
        let text = if raw.is_empty() {
            UNRECORDED_REASON.to_string()
        } else {
            cap(raw, REASON_CHARS)
        };
        if !reasons.contains(&text) {
            reasons.push(text);
        }
    }
    reasons
}

/// Truncate to `limit` characters, marking the cut.
///
/// Char-wise rather than byte-wise so a multi-byte prompt cannot be split into
/// invalid UTF-8, and counted in characters rather than bytes so the bound means
/// the same thing for every alphabet.
fn cap(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let kept: String = text.chars().take(limit).collect();
    format!("{kept}…")
}

/// What the ledger says when a failed turn recorded no reason at all.
const UNRECORDED_REASON: &str = "unrecorded";

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn row(prompt: &str, response: &str, ok: bool, reason: &str) -> TurnRow {
        TurnRow {
            prompt: prompt.to_string(),
            response: response.to_string(),
            ok,
            stop_reason: reason.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn the_block_has_the_documented_shape() {
        let rows: Vec<TurnRow> = (0..5)
            .map(|i| row(&format!("ask {i}"), &format!("answer {i}"), true, "SUCCESS"))
            .collect();
        let compacted = compact(&rows, Budget::new(2, 2));

        assert_eq!(
            compacted.block,
            "## Compacted history (1 of 5 turns elided)\n\
             ### Asked\n\
             - ask 2\n\
             ### Changed\n\
             - 1 succeeded"
        );
        assert_eq!(compacted.elided(), 1);
        assert_eq!(compacted.kept(), 4);
        assert!(compacted.head.starts_with("user: ask 0"));
        assert!(compacted.tail.ends_with("assistant: answer 4"));
    }

    #[test]
    fn a_turn_with_nothing_recorded_renders_as_nothing() {
        assert_eq!(render_turn(&row("", "", true, "")), "");
        assert_eq!(render_turn(&row("  \n ", "\t", true, "")), "");
        assert_eq!(
            render_turn(&row("ask", "", true, "")),
            "user: ask",
            "a prompt with no answer must not leave a dangling assistant label"
        );
        assert_eq!(
            render_turn(&row("", "answer", true, "")),
            "assistant: answer"
        );
        assert_eq!(
            render_turn(&row(" ask ", " answer ", true, "")),
            "user: ask\nassistant: answer"
        );
    }

    #[test]
    fn the_cap_counts_characters_not_bytes() {
        assert_eq!(cap("short", 10), "short");
        let capped = cap(&"é".repeat(50), 10);
        assert_eq!(capped.chars().count(), 11, "10 kept plus the ellipsis");
        assert!(capped.ends_with('…'));
    }

    #[test]
    fn a_short_transcript_is_split_at_the_head_and_keeps_everything() {
        let rows: Vec<TurnRow> = (0..3)
            .map(|i| row(&format!("ask {i}"), "answer", true, "SUCCESS"))
            .collect();
        let compacted = compact(&rows, Budget::new(2, 8));

        assert_eq!(compacted.elided(), 0);
        assert!(compacted.block.is_empty());
        assert_eq!(compacted.kept(), 3);
        assert!(
            compacted.tail.contains("ask 2"),
            "the rest follows the head"
        );
    }

    #[test]
    fn an_empty_middle_produces_no_block_lines() {
        assert_eq!(render_block(0, &[]), "");
        let rows: Vec<TurnRow> = Vec::new();
        assert!(compact(&rows, Budget::default()).is_empty());
    }

    #[test]
    fn the_ask_list_collapses_repeats_and_reports_the_remainder() {
        let parked = row("keep going", "ok", true, "SUCCESS");
        let other = row("then stop", "ok", true, "SUCCESS");
        let dropped: Vec<&TurnRow> = vec![&parked, &parked, &parked, &other];

        assert_eq!(
            ask_lines(&dropped),
            vec!["- keep going (x3)", "- then stop"]
        );
    }

    #[test]
    fn a_blank_prompt_neither_breaks_a_run_nor_becomes_a_line() {
        let asked = row("keep going", "ok", true, "SUCCESS");
        let blank = row("  \n", "ok", true, "SUCCESS");
        let dropped: Vec<&TurnRow> = vec![&asked, &blank, &asked];

        assert_eq!(ask_lines(&dropped), vec!["- keep going (x2)"]);
    }

    #[test]
    fn failure_classes_are_distinct_and_ordered_by_first_appearance() {
        let timeout = row("a", "", false, "timeout");
        let detail = row("b", "", false, "PROTOCOL_ERROR");
        let silent = row("c", "", false, "");
        let dropped: Vec<&TurnRow> = vec![&timeout, &detail, &silent, &timeout];

        assert_eq!(
            failure_reasons(&dropped),
            vec!["timeout", "PROTOCOL_ERROR", "unrecorded"]
        );
    }
}
