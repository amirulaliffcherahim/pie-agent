//! Gate G4 for M5 stage C: extractive, quota-free transcript compaction.
//!
//! `planning/ACCEPTANCE.md` §8, row C: *"extractive: head + state block + tail;
//! spawns nothing (quota-free); stored rows untouched; the block sits after the
//! persona"*. `planning/DESIGN.md` §11 is the contract, and this file is where it
//! is enforced.
//!
//! Nothing here spawns a vendor CLI, touches the network or reads `~/.pie`:
//! `src/compact.rs` is a pure function over rows, and the first test below proves
//! *that* — the rest prove the contract on top of it. The whole suite therefore
//! costs zero quota, which is the property stage C exists to have.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use pie::compact::{
    self, Budget, ASKED_HEADING, ASK_CHARS, ASSISTANT_LABEL, BLOCK_HEADING, CHANGED_HEADING,
    DEFAULT_COMPACT_AFTER_TURNS, MAX_ASK_LINES, USER_LABEL,
};
use pie::profile;
use pie::provider::Usage;
use pie::session::TurnRow;

/// Epoch-millisecond base for every fixture row: 13 digits, so a timestamp in the
/// block would be unmistakable.
const TS_BASE: u64 = 1_776_000_000_000;

/// A model id, which is one of the values the block must never carry.
const MODEL: &str = "gemini-3.8-flash";

/// Input-token count for the first row, so a quota figure in the block is
/// unmistakable too.
const INPUT_TOKENS: u64 = 20_704;

/// One deterministic row. Every volatile field carries a value distinctive enough
/// that finding it later proves a leak, and the prose carries no digits.
fn row(index: usize) -> TurnRow {
    TurnRow {
        ts: TS_BASE + index as u64,
        provider: "agy".to_string(),
        model: MODEL.to_string(),
        prompt: format!("task-{index} alpha"),
        response: format!("answer-{index} omega"),
        conversation_id: Some(format!("conv-{index:02}")),
        usage: Usage {
            input_tokens: INPUT_TOKENS + index as u64,
            output_tokens: 11,
            thinking_tokens: 3,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            total_tokens: INPUT_TOKENS + index as u64 + 14,
        },
        ok: true,
        stop_reason: "SUCCESS".to_string(),
    }
}

/// `count` rows, in order.
fn rows(count: usize) -> Vec<TurnRow> {
    (0..count).map(row).collect()
}

fn repo_root() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Where `needle` starts, or a failure that prints the payload it looked in.
fn position(haystack: &str, needle: &str) -> usize {
    haystack
        .find(needle)
        .unwrap_or_else(|| panic!("`{needle}` is not in the payload:\n{haystack}"))
}

/// The ask bullets of a block, in order — the `- ` lines under `### Asked`,
/// excluding the outcome ledger under `### Changed`.
fn ask_bullets(block: &str) -> Vec<&str> {
    let mut bullets = Vec::new();
    let mut in_asked = false;

    for line in block.lines() {
        if line.starts_with(ASKED_HEADING) {
            in_asked = true;
        } else if line.starts_with(CHANGED_HEADING) {
            in_asked = false;
        } else if in_asked && line.starts_with("- ") {
            bullets.push(line);
        }
    }
    bullets
}

/// Longest run of ASCII digits in `text`.
fn longest_digit_run(text: &str) -> usize {
    let mut longest = 0;
    let mut current = 0;
    for c in text.chars() {
        if c.is_ascii_digit() {
            current += 1;
            longest = longest.max(current);
        } else {
            current = 0;
        }
    }
    longest
}

/// Every value the block must not carry: the per-row volatile fields, plus the
/// ambient values a careless implementation reaches for.
fn volatile_values(input: &[TurnRow]) -> Vec<(String, &'static str)> {
    let mut out: Vec<(String, &'static str)> = Vec::new();

    for r in input {
        out.push((r.ts.to_string(), "a timestamp"));
        out.push((r.model.clone(), "a model id"));
        out.push((r.usage.input_tokens.to_string(), "a token/quota figure"));
        if let Some(id) = &r.conversation_id {
            out.push((id.clone(), "a session/conversation id"));
        }
    }

    if let Ok(cwd) = std::env::current_dir() {
        out.push((cwd.display().to_string(), "a working directory"));
    }
    for key in ["HOME", "USERPROFILE", "PIE_HOME"] {
        if let Ok(value) = std::env::var(key) {
            if !value.trim().is_empty() {
                out.push((value, "an ambient path"));
            }
        }
    }
    out.push((
        std::env::temp_dir().display().to_string(),
        "an ambient path",
    ));

    out
}

/// The `usage.jsonl` keys a row dump would carry, and the vocabulary of a quota
/// figure. The block is prose, not a serialised row.
const NOT_A_ROW_DUMP: &[&str] = &[
    "conversation_id",
    "\"ts\"",
    "\"usage\"",
    "\"",
    "quota",
    "credits",
];

/// Reaching for any of these in the module would break the quota-free guarantee.
///
/// The unit tests inside `src/compact.rs` are excluded from the scan: they may
/// use anything, because they are not the shipped path.
const FORBIDDEN_IN_MODULE: &[(&str, &str)] = &[
    ("std::fs", "the filesystem"),
    ("std::process", "a child process"),
    ("Command", "a child process"),
    ("std::time", "the clock"),
    ("SystemTime", "the clock"),
    ("Instant", "the clock"),
    ("now_millis", "the clock"),
    ("rand::", "randomness"),
    ("reqwest", "the network"),
    ("tokio", "the async runtime"),
    ("crate::paths", "pie's on-disk layout"),
    ("crate::config", "configuration"),
];

#[test]
fn the_module_is_pure_nothing_here_can_spawn_read_or_look_at_the_clock() {
    let source = std::fs::read_to_string(repo_root().join("src").join("compact.rs")).unwrap();
    let code = source.split("#[cfg(test)]").next().unwrap();

    assert!(
        code.lines().count() > 150,
        "the scan must cover a real module, not a stub — {} lines",
        code.lines().count()
    );

    for (needle, what) in FORBIDDEN_IN_MODULE {
        assert!(
            !code.contains(needle),
            "src/compact.rs reaches for {what} (`{needle}`) — compaction must stay a pure \
             function over rows: no subprocess, no I/O, no clock, no randomness (DESIGN §11, \
             RISKS §13)"
        );
    }
}

#[test]
fn the_same_rows_compact_byte_identically() {
    let input = rows(24);
    let budget = Budget::new(2, 4);

    let first = compact::compact(&input, budget);
    let second = compact::compact(&input, budget);

    assert_eq!(first.head, second.head);
    assert_eq!(first.block, second.block);
    assert_eq!(first.tail, second.tail);
    assert_eq!(first.render(), second.render());
    assert_eq!(first.dropped.len(), second.dropped.len());

    // A second, equal transcript must produce the same bytes: nothing may leak in
    // from the clock, the process, an address or a hash iteration order.
    let twin = rows(24);
    assert_eq!(compact::compact(&twin, budget).render(), first.render());
}

#[test]
fn compacted_text_carries_no_volatile_value() {
    let input = rows(24);
    let compacted = compact::compact(&input, Budget::new(2, 4));
    assert!(
        !compacted.block.is_empty(),
        "the fixture must actually elide something, or this proves nothing"
    );

    for (value, what) in volatile_values(&input) {
        assert!(
            !compacted.block.contains(&value),
            "the state block carries {what} ({value:?}):\n{}",
            compacted.block
        );
        assert!(
            !compacted.render().contains(&value),
            "the compacted text carries {what} ({value:?})"
        );
    }

    for needle in NOT_A_ROW_DUMP {
        assert!(
            !compacted.block.contains(needle),
            "the block is prose, not a serialised row — found {needle:?}:\n{}",
            compacted.block
        );
    }

    assert!(
        longest_digit_run(&compacted.block) < 5,
        "an epoch-millisecond timestamp cannot hide in the block (longest digit run {}):\n{}",
        longest_digit_run(&compacted.block),
        compacted.block
    );
}

#[test]
fn the_head_and_tail_survive_and_the_middle_is_elided() {
    let input = rows(20);
    let compacted = compact::compact(&input, Budget::new(3, 5));

    assert_eq!(compacted.turns(), 20);
    assert_eq!(compacted.elided(), 12);
    assert_eq!(compacted.kept(), 8);

    // Head: rows 0..3, verbatim, in order.
    for index in 0..3 {
        assert!(compacted.head.contains(&format!("task-{index} alpha")));
        assert!(compacted.head.contains(&format!("answer-{index} omega")));
    }
    // Tail: rows 15..20, verbatim, in order.
    for index in 15..20 {
        assert!(compacted.tail.contains(&format!("task-{index} alpha")));
    }
    assert!(
        position(&compacted.head, "task-0") < position(&compacted.head, "task-1"),
        "a window keeps the transcript's own order"
    );
    assert!(position(&compacted.tail, "task-15") < position(&compacted.tail, "task-19"));

    // Middle: rows 3..15, in neither window, listed as asked in the block.
    for index in 3..15 {
        assert!(
            !compacted.head.contains(&format!("task-{index} alpha")),
            "row {index} is in the middle and must not be in the head"
        );
        assert!(!compacted.tail.contains(&format!("task-{index} alpha")));
        assert!(
            compacted.block.contains(&format!("- task-{index} alpha")),
            "row {index} was asked and the block must say so"
        );
    }

    // The block is a state block, not a transcript: the answers stay out of it.
    assert!(!compacted.block.contains("answer-3 omega"));

    // The elided turn is still whole in the caller's hands.
    assert_eq!(compacted.dropped[0].prompt, "task-3 alpha");
    assert_eq!(compacted.dropped[0].response, "answer-3 omega");

    assert_eq!(
        compacted.block.lines().next().unwrap(),
        format!("{BLOCK_HEADING} (12 of 20 turns elided)")
    );
}

#[test]
fn the_elided_rows_are_handed_back_rather_than_lost() {
    let input = rows(20);
    let compacted = compact::compact(&input, Budget::new(3, 5));

    for (offset, dropped) in compacted.dropped.iter().enumerate() {
        assert!(
            std::ptr::eq(*dropped, &input[3 + offset]),
            "row {} was handed back as a copy, not as the caller's own row",
            3 + offset
        );
    }

    let prompts: Vec<&str> = compacted
        .dropped
        .iter()
        .map(|r| r.prompt.as_str())
        .collect();
    let expected: Vec<String> = (3..15).map(|i| format!("task-{i} alpha")).collect();
    assert_eq!(prompts.join(","), expected.join(","));
}

#[test]
fn the_input_rows_are_not_modified() {
    let input = rows(24);
    let before = input.clone();

    let compacted = compact::compact(&input, Budget::new(2, 4));

    assert_eq!(
        input, before,
        "compaction changes what is sent, never what is kept"
    );
    assert_eq!(input.len(), 24, "no row was removed");
    assert_eq!(compacted.dropped.len(), 18);
    assert_eq!(compacted.dropped[0].ts, input[2].ts);
}

#[test]
fn nothing_is_elided_at_the_threshold_and_one_turn_past_it_elides_exactly_one() {
    let budget = Budget::new(3, 5);
    assert_eq!(budget.keeps(), 8, "the threshold is head + tail turns");

    let filled = rows(8);
    let at = compact::compact(&filled, budget);
    assert_eq!(at.elided(), 0);
    assert_eq!(at.kept(), 8);
    assert!(at.block.is_empty(), "no block below the threshold");
    assert!(at.dropped.is_empty());
    assert!(
        !at.is_empty(),
        "eight kept turns are still eight turns of text"
    );
    assert!(at.head.contains("task-0 alpha") && at.tail.contains("task-7 alpha"));

    let overflowing = rows(9);
    let over = compact::compact(&overflowing, budget);
    assert_eq!(over.elided(), 1);
    assert_eq!(over.kept(), 8);
    assert_eq!(over.dropped.len(), 1);
    assert_eq!(over.dropped[0].prompt, "task-3 alpha");
    assert!(over.block.contains("(1 of 9 turns elided)"));
}

#[test]
fn zero_turns_compact_to_nothing() {
    let compacted = compact::compact(&[], Budget::default());

    assert!(compacted.is_empty());
    assert_eq!(compacted.render(), "");
    assert_eq!(compacted.turns(), 0);
    assert_eq!(compacted.kept(), 0);
    assert_eq!(compacted.elided(), 0);
    assert!(compacted.dropped.is_empty());
}

#[test]
fn one_turn_is_kept_whole() {
    let input = rows(1);
    let compacted = compact::compact(&input, Budget::default());

    assert_eq!(compacted.elided(), 0);
    assert_eq!(
        compacted.head,
        format!("{USER_LABEL}task-0 alpha\n{ASSISTANT_LABEL}answer-0 omega")
    );
    assert!(compacted.tail.is_empty());
    assert!(compacted.block.is_empty());
}

#[test]
fn a_blank_turn_contributes_nothing_rather_than_a_blank_line() {
    let mut input = rows(12);
    input[0].response = "   \n".to_string(); // head: a whitespace-only answer
    input[2].prompt = "\n  \n".to_string(); // middle: a whitespace-only ask
    input[3].response = "  ".to_string(); // middle: a whitespace-only answer
    input[4].prompt = String::new(); // tail: nothing recorded at all
    input[4].response = String::new();

    let compacted = compact::compact(&input, Budget::new(2, 8));

    assert_eq!(
        compacted.head.lines().collect::<Vec<&str>>(),
        vec![
            "user: task-0 alpha",
            "",
            "user: task-1 alpha",
            "assistant: answer-1 omega",
        ],
        "the blank answer must leave no assistant label behind — the one blank line is the \
         separator between turns, not a turn"
    );
    let tail_prompts = compacted
        .tail
        .lines()
        .filter(|line| line.starts_with(USER_LABEL))
        .count();
    let tail_answers = compacted
        .tail
        .lines()
        .filter(|line| line.starts_with(ASSISTANT_LABEL))
        .count();
    assert_eq!(
        (tail_prompts, tail_answers),
        (7, 7),
        "the blank tail turn added no line"
    );
    assert!(!compacted.tail.starts_with('\n'));
    assert_eq!(compacted.dropped.len(), 2, "a blank turn is still a turn");
    assert_eq!(ask_bullets(&compacted.block), vec!["- task-3 alpha"]);
    assert!(
        !compacted.block.contains("\n\n"),
        "a blank ask must not leave a gap in the block:\n{}",
        compacted.block
    );
    assert!(!compacted.render().contains("\n\n\n"));
}

#[test]
fn repeated_asks_collapse_and_the_ask_list_is_capped() {
    // A session that repeats one instruction costs one line, not fifty.
    let mut repeated = rows(60);
    for r in repeated.iter_mut() {
        r.prompt = "keep going".to_string();
    }
    repeated[7].prompt = "and now stop".to_string();

    let collapsed = compact::compact(&repeated, Budget::new(2, 8));
    assert_eq!(
        ask_bullets(&collapsed.block),
        vec!["- keep going (x5)", "- and now stop", "- keep going (x44)"]
    );

    // Fifty distinct asks are capped, with the remainder reported.
    let unique = rows(60);
    let capped = compact::compact(&unique, Budget::new(2, 8));
    assert!(capped.block.contains("(50 of 60 turns elided)"));
    let asks = ask_bullets(&capped.block);
    assert_eq!(
        asks.len(),
        MAX_ASK_LINES + 1,
        "the cap plus the remainder line:\n{}",
        capped.block
    );
    assert_eq!(
        asks[MAX_ASK_LINES],
        format!("- ... and {} more turns", 50 - MAX_ASK_LINES)
    );
}

#[test]
fn a_long_ask_is_capped_character_wise_in_the_block() {
    let mut input = rows(12);
    input[3].prompt = format!("{} and more", "é".repeat(400));

    let compacted = compact::compact(&input, Budget::new(2, 8));
    let asks = ask_bullets(&compacted.block);
    let long = asks[1];

    assert_eq!(
        long.chars().count(),
        "- ".chars().count() + ASK_CHARS + 1,
        "multi-byte input is cut by character, not by byte"
    );
    assert!(long.ends_with('…'));
}

#[test]
fn the_ledger_reports_the_outcomes_of_the_elided_turns_only() {
    let mut input = rows(12);
    input[2].ok = false;
    input[2].stop_reason = "timeout".to_string();
    input[3].ok = false;
    input[3].stop_reason = "timeout".to_string();
    input[5].ok = false; // kept in the tail: the ledger describes the middle only
    input[5].stop_reason = "ERROR".to_string();

    let compacted = compact::compact(&input, Budget::new(2, 8));
    assert!(compacted.block.contains("- 2 failed: timeout"));
    assert!(
        !compacted.block.contains("succeeded"),
        "nothing in the middle succeeded:\n{}",
        compacted.block
    );
    assert!(
        !compacted.block.contains("ERROR"),
        "the ledger describes the elided middle, not the kept tail"
    );

    let mut silent = rows(12);
    silent[2].ok = false;
    silent[2].stop_reason = String::new();

    let quiet = compact::compact(&silent, Budget::new(2, 8));
    assert!(quiet.block.contains("- 1 failed: unrecorded"));
    assert!(quiet.block.contains("- 1 succeeded"));
}

#[test]
fn the_assembled_prefix_places_the_persona_then_the_block_then_the_task() {
    let input = rows(20);
    let compacted = compact::compact(&input, Budget::new(3, 5));
    let persona = "# Persona\nname: pie\ntone: terse";
    let task = "please continue with the next step";

    let payload = compact::assemble(Some(persona), &compacted, task);

    assert!(payload.starts_with(persona), "the static persona is first");
    assert!(payload.ends_with(task), "the volatile task text is last");

    assert!(
        position(&payload, persona) < position(&payload, "task-0 alpha"),
        "the head follows the persona"
    );
    assert!(
        position(&payload, "task-0 alpha") < position(&payload, &compacted.block),
        "the head precedes the state block"
    );
    assert!(
        position(&payload, &compacted.block) < position(&payload, "task-19 alpha"),
        "the state block precedes the tail window"
    );

    // The named property, asserted on the composed string: block after persona,
    // task after block. Nothing stable may follow something volatile.
    let persona_at = position(&payload, persona);
    let block_at = position(&payload, &compacted.block);
    let task_at = position(&payload, task);
    assert!(persona_at < block_at, "persona -> block");
    assert!(block_at < task_at, "block -> task");

    // The provider folds the persona for agy and codex, so that caller passes
    // `None`; the two paths must compose to the same bytes.
    let folded_by_provider = compact::assemble(None, &compacted, task);
    assert!(folded_by_provider.starts_with(USER_LABEL));
    assert_eq!(
        payload,
        profile::fold(Some(persona), &folded_by_provider),
        "assemble(Some(p)) is the provider's own fold over assemble(None, ..)"
    );
}

#[test]
fn compaction_below_the_threshold_leaves_the_payload_unchanged() {
    let empty = compact::compact(&[], Budget::default());

    assert_eq!(
        compact::assemble(None, &empty, "do the thing"),
        "do the thing"
    );
    let persona = "# Persona\ntone: terse";
    assert_eq!(
        compact::assemble(Some(persona), &empty, "do the thing"),
        profile::fold(Some(persona), "do the thing"),
        "a session that never crossed the threshold gets the bytes it always got"
    );

    assert!(!compact::is_due(0, DEFAULT_COMPACT_AFTER_TURNS));
    assert!(!compact::is_due(
        DEFAULT_COMPACT_AFTER_TURNS,
        DEFAULT_COMPACT_AFTER_TURNS
    ));
    assert!(compact::is_due(
        DEFAULT_COMPACT_AFTER_TURNS + 1,
        DEFAULT_COMPACT_AFTER_TURNS
    ));
}
