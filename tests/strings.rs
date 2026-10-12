//! The REA-lite `strings` gate: the extractor behind the drift check's third leg.
//!
//! Contract: `planning/DESIGN.md` §17; the gap it closes is `planning/ACCEPTANCE.md` §13
//! B34. The gate is `cargo test --test strings`.
//!
//! **Every input here is a byte slice built in this file.** Nothing opens a file, spawns a
//! process or consults the machine's installs: the extractor is pure precisely so that it
//! can be tested this way, and a test that read the developer's own `agy.exe` would be green
//! on one machine and red everywhere else — gate **G3d**, after B32.
//!
//! Three properties are the reason the file is shaped this way, and none is visible from a
//! single interesting case:
//!
//! * **A short flag is noise and a long flag is the contract.** The first version kept
//!   `-x`, and against the real `agy.exe` that produced 268 entries that were almost
//!   entirely Go compiler and runtime noise while every real flag was absent. The regression
//!   test below is that measurement, reduced to a byte slice.
//! * **A binary is the least forgiving input in the project.** It is full of bytes that are
//!   not UTF-8, and invariant I6 forbids slicing one at a non-boundary — so the last run in
//!   the buffer and the run interrupted by invalid bytes are both asserted, not assumed.
//! * **Chunking must be invisible.** The driver reads the artefact in bounded chunks and
//!   feeds an incremental scanner; the *same* scanner over one slice must produce the
//!   *same* answer, or the leg would report drift that was really a read-boundary artefact.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;

use pie::error::truncate;
use pie::watch::strings::{interesting_strings, StringScanner, MAX_STRINGS, POLICY_WORDS};

/// A printable run that is neither flag-like nor policy vocabulary.
///
/// Long on purpose: the assertion below is that length alone does not make a string
/// interesting, which a two-word fixture could not show.
const DULL: &str = "the quick brown fox jumps over the lazy dog";

/// Feed `bytes` to a fresh scanner in pieces of `chunk` bytes.
///
/// A helper rather than an inline loop because the chunk-size sweep below is the point: the
/// answer must not depend on any of these numbers.
fn scan_in_chunks(bytes: &[u8], cap: usize, chunk: usize) -> Vec<String> {
    let mut scanner = StringScanner::new(cap);
    for piece in bytes.chunks(chunk) {
        scanner.feed(piece);
    }
    scanner.finish()
}

// ---------------------------------------------------------------------------
// What is kept
// ---------------------------------------------------------------------------

#[test]
fn a_long_flag_is_extracted_and_a_short_flag_is_not() {
    // The regression this fix is about. `--print-timeout` is the shape of a real CLI's
    // contract; `-x`, `-C`, `-1v`, `-h` and `-p` are the shape of everything else that
    // follows a dash in a binary — a format verb, a register name, a getopt letter. Keeping
    // them buried the flags the leg exists to see and churned on every rebuild, which is
    // false drift.
    let found = interesting_strings(
        b"\x00--print-timeout\x00-x\x00-C\x00-1v\x00-h\x00-p\x00",
        MAX_STRINGS,
    );

    assert_eq!(found, vec!["--print-timeout".to_string()], "{found:?}");
}

#[test]
fn a_flag_wrapped_in_the_sources_own_syntax_is_still_a_flag() {
    // `"--json"` is a JavaScript literal, `"%1"=="--version"` is a batch script's argument
    // test, and `[--print <PROMPT>]` is a usage line. All three are flags, and the quotes,
    // equals signs and brackets around them belong to the source.
    let found = interesting_strings(
        b"\x00if (mode === \"--json\")\x00if \"%1\"==\"--version\"\x00usage: agy [--print <PROMPT>]\x00",
        MAX_STRINGS,
    );

    for expected in ["--json", "--version", "--print"] {
        assert!(
            found.iter().any(|s| s == expected),
            "`{expected}` was not extracted from a run that carries it: {found:?}"
        );
    }
}

#[test]
fn a_name_that_does_not_start_with_a_letter_is_not_a_long_flag() {
    // `--`, `---` and `--2fast` are not flags a CLI documents, and counting them would report
    // the same "new flag" on every run — the failure `drift::is_flag` guards against, reached
    // from the extractor's side.
    let found = interesting_strings(b"\x00--\x00---\x00--2fast\x00--1\x00--a\x00", MAX_STRINGS);

    assert_eq!(found, vec!["--a".to_string()], "{found:?}");
}

#[test]
fn every_policy_word_is_extracted_from_the_run_that_carries_it() {
    // The vocabulary that makes this leg early rather than late: a vendor writes `consent`
    // or `opt-out` into its binary long before the sentence reaches `--help`, and
    // `PieMustStop` is what has to see it.
    for word in POLICY_WORDS {
        let input = format!("\x00we log {word} here\x00");
        let found = interesting_strings(input.as_bytes(), MAX_STRINGS);

        assert!(
            found.iter().any(|s| s.to_ascii_lowercase().contains(word)),
            "`{word}` was not extracted from {input:?}: {found:?}"
        );
    }
}

#[test]
fn a_policy_word_beyond_the_retained_head_is_still_found() {
    // A run longer than the extractor keeps can still carry its word far past the bytes it
    // retains. Tracking the word only in the retained head would drop the whole run — the
    // silent hole this leg exists to avoid — so the assertion is simply that a run with no
    // flag in it is kept at all, with its cut marked.
    let run = format!("{}{}", "x".repeat(300), "consent");

    let found = interesting_strings(run.as_bytes(), MAX_STRINGS);

    assert_eq!(found.len(), 1, "{found:?}");
    assert!(
        found[0].ends_with("[truncated]"),
        "a run past the retention must be marked, not whole: {:?}",
        found[0]
    );
}

#[test]
fn a_dull_run_is_not_extracted() {
    // The control for everything above: a long, perfectly printable run that is neither a
    // flag nor policy vocabulary is *not* evidence, and dumping it is how one snapshot
    // becomes megabytes and the diff unreadable.
    let input = format!("\x00{DULL}\x00also nothing here\x00");
    assert!(interesting_strings(input.as_bytes(), MAX_STRINGS).is_empty());
}

#[test]
fn a_single_string_is_cut_and_marked_rather_than_kept_whole() {
    // One printable region can be arbitrarily long, and an uncut one would push the
    // snapshot past the point where anyone reads it. The cut is marked, so a truncated
    // string is never mistaken for the whole one.
    let long = "consent ".repeat(60);

    let found = interesting_strings(long.as_bytes(), MAX_STRINGS);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(
        found[0].ends_with("[truncated]"),
        "a cut string must say so: {:?}",
        found[0]
    );
}

#[test]
fn a_long_flag_cut_by_the_per_string_cap_is_marked() {
    // The per-string cap applies to a flag token too: a pathologically long name is kept,
    // not dropped, and it says that it was cut.
    let token = format!("--{}", "a".repeat(300));

    let found = interesting_strings(token.as_bytes(), MAX_STRINGS);

    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].starts_with("--a"), "{found:?}");
    assert!(found[0].ends_with("[truncated]"), "{found:?}");
}

#[test]
fn the_cut_marker_is_the_one_the_rest_of_the_project_uses() {
    // The incremental extractor builds its truncated sentence from retained bytes rather
    // than from the whole run, so the marker it appends must be `error::truncate`'s — the
    // one a reader of any other snapshot already knows.
    let long = "consent ".repeat(60);

    let found = interesting_strings(long.as_bytes(), MAX_STRINGS);

    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0], truncate(long.trim(), 200), "{found:?}");
}

// ---------------------------------------------------------------------------
// The bounds, and the properties a diff depends on
// ---------------------------------------------------------------------------

#[test]
fn the_cap_is_respected_and_the_result_is_sorted_deduped_and_reproducible() {
    // The runs that are evidence: two long flags repeated, one long flag alone, and a policy
    // sentence. The short flags are here too, to show the new rule discards them rather than
    // letting them into the set.
    let input: &[u8] = b"\x00-x\x00--b\x00--a\x00-x\x00--c\x00consent\x00--a\x00";

    // Sorted exactly, which is the dedup and the ordering in one assertion.
    let expected: Vec<String> = ["--a", "--b", "--c", "consent"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    assert_eq!(interesting_strings(input, MAX_STRINGS), expected);

    // Capped: `--b`, `--a` and `--c` are the first three distinct runs in byte order, and
    // they come back sorted rather than in the order they were met.
    let capped = interesting_strings(input, 3);
    assert_eq!(capped.len(), 3, "{capped:?}");
    assert_eq!(capped, vec!["--a", "--b", "--c"], "{capped:?}");

    // Reproducible: a snapshot that reordered itself between two runs would be diffed as a
    // change that never happened, which is noise the reader has to filter by hand.
    assert_eq!(capped, interesting_strings(input, 3));
}

#[test]
fn a_long_flag_is_kept_even_when_policy_prose_has_filled_the_cap() {
    // Measured against the real `agy.exe`: ~15 000 policy sentences are selected before a
    // contract flag arrives, so a first-come cap would be full of prose before it saw one.
    // A flag evicts a policy sentence instead of being dropped by one — only prose may lose.
    let mut scanner = StringScanner::new(2);
    scanner.feed(b"\x00consent here\x00terms of service\x00--real-flag\x00");
    let found = scanner.finish();

    assert!(
        found.iter().any(|s| s == "--real-flag"),
        "the flag must survive a cap the prose filled: {found:?}"
    );
    assert_eq!(found.len(), 2, "the cap must still hold: {found:?}");
}

#[test]
fn a_megabyte_of_flag_tokens_is_bounded_by_the_cap() {
    // A pathological artefact — every run flag-like and distinct — must not grow the kept
    // set with the input, and must not grow the open token either. The assertion is the cap
    // plus the fact that the scan terminates at all; the memory the set can reach is the cap
    // by construction.
    let mut input: Vec<u8> = Vec::new();
    let mut index: u32 = 0;
    while input.len() < 1024 * 1024 {
        input.extend_from_slice(format!("--flag-{index}\x00").as_bytes());
        index += 1;
    }

    let found = scan_in_chunks(&input, MAX_STRINGS, 64 * 1024);

    assert_eq!(found.len(), MAX_STRINGS, "the cap must bound the set");
    assert!(found.iter().all(|s| s.starts_with("--flag-")), "{found:?}");
}

#[test]
fn an_empty_input_yields_an_empty_list() {
    assert!(interesting_strings(b"", MAX_STRINGS).is_empty());

    // …and so does a buffer of nothing but separators, which is the same claim about a file
    // whose first bytes are not printable — with two of them at or above `0x80`, so the
    // claim covers the invalid-UTF-8 case too.
    assert!(interesting_strings(&[0x00, 0xff, 0x0a, 0x80], MAX_STRINGS).is_empty());
}

// ---------------------------------------------------------------------------
// Chunk boundaries, where the streaming read either works or silently loses a string
// ---------------------------------------------------------------------------

#[test]
fn a_string_straddling_a_chunk_boundary_is_found_whole() {
    // The driver reads the artefact in pieces, so a flag or a policy sentence can be split
    // between two reads. The scanner holds the partial run, so the string arrives whole: it
    // is neither lost (the part before the boundary closed the run early) nor split into two
    // strings (the part after the boundary started a new one).
    let mut scanner = StringScanner::new(MAX_STRINGS);
    scanner.feed(b"\x00--pri");
    scanner.feed(b"nt-timeout we log cons");
    scanner.feed(b"ent here\x00");
    let found = scanner.finish();

    assert!(
        found.iter().any(|s| s == "--print-timeout"),
        "the straddling flag must be whole: {found:?}"
    );
    assert!(
        found
            .iter()
            .any(|s| s == "--print-timeout we log consent here"),
        "the straddling sentence must be whole: {found:?}"
    );
}

#[test]
fn a_run_ending_exactly_on_a_chunk_boundary_is_not_lost() {
    // Two edges at once: a run closed by the byte that ends a chunk, and a run that begins
    // at the first byte of the next one. Both are the case a per-chunk scan gets wrong.
    let input: &[u8] = b"abc\x00--edge-flag";

    let mut scanner = StringScanner::new(MAX_STRINGS);
    scanner.feed(&input[..4]); // `abc\x00` — the first run ends exactly here…
    scanner.feed(&input[4..]); // …and the flagged run begins exactly here.
    let found = scanner.finish();

    assert_eq!(found, vec!["--edge-flag".to_string()], "{found:?}");
}

#[test]
fn a_chunked_read_produces_exactly_the_same_result_as_a_single_pass() {
    // The stronger claim behind the two tests above: the answer is a property of the bytes,
    // not of how they were chunked. The old driver took one prefix and the new one streams,
    // so this is what makes "the strings are the ones the whole file carries" a test rather
    // than a hope.
    let mut input: Vec<u8> = Vec::new();
    input.extend_from_slice(b"\x00--print-timeout\x00--json\x00a-run-without-separators-");
    input.extend_from_slice(b"consent and telemetry in one sentence\x00");
    input.extend_from_slice(&[0xff, 0xfe, 0x80]);
    input.extend_from_slice(b"--add-dir\x00--x\x00");
    input.extend_from_slice(&b"z".repeat(500));
    input.extend_from_slice(b"\x00opt-out\x00--disable-slash-commands\x00privacy policy\x00");
    // Ends mid-run on purpose: the final string has no separator to close it.
    input.extend_from_slice(b"--trailing");

    for cap in [MAX_STRINGS, 3] {
        let expected = interesting_strings(&input, cap);
        // Chunk sizes that straddle every interesting boundary, plus the degenerate single
        // byte and whole buffer.
        for chunk in [1, 2, 3, 5, 7, 13, 64, 777, 4096, input.len()] {
            assert_eq!(
                scan_in_chunks(&input, cap, chunk),
                expected,
                "cap {cap}, chunk {chunk}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// The buffer's edges, where a binary bites
// ---------------------------------------------------------------------------

#[test]
fn a_run_at_the_very_end_of_the_buffer_is_not_lost() {
    // No trailing separator, so the final run has nothing to close it. A scanner that only
    // emits on a separator drops it — every time, on every file that ends in a string, and
    // the hole would be silent rather than an error.
    let found = interesting_strings(b"junk\x00--last-flag", MAX_STRINGS);

    assert_eq!(found, vec!["--last-flag".to_string()]);
}

#[test]
fn invalid_utf8_and_nul_split_runs_do_not_panic() {
    // A binary file is full of bytes that are not UTF-8, and a byte slice taken at a
    // non-boundary is a panic on external input (invariant I6). This input puts invalid
    // bytes immediately before and inside both kinds of run.
    let input: &[u8] = b"\xff\xfe\x00--help\x00\xc3\x28 consent \xf0\x9f\x00\x80--flag\x00";

    let found = interesting_strings(input, MAX_STRINGS);

    assert!(found.iter().any(|s| s == "--help"), "{found:?}");
    assert!(found.iter().any(|s| s == "--flag"), "{found:?}");
    assert!(
        found.iter().any(|s| s.contains("consent")),
        "the run carrying the policy word must survive the bytes around it: {found:?}"
    );

    // The same bytes through the chunked path, split through the invalid ones, must not
    // panic either.
    for chunk in [1, 2, 3, 5] {
        let _ = scan_in_chunks(input, MAX_STRINGS, chunk);
    }
}

#[test]
fn the_same_bytes_always_produce_the_same_list() {
    // Stated separately from the cap test because it is the property the *differ* relies
    // on: `diff` compares two snapshots as sets, so an extractor whose output depended on
    // hash iteration order would report drift on a binary that never changed.
    let input: &[u8] = b"\x00--alpha\x00telemetry\x00--beta\x00policy\x00";

    let first = interesting_strings(input, MAX_STRINGS);
    let second = interesting_strings(input, MAX_STRINGS);
    assert_eq!(first, second);

    let unique: BTreeSet<&String> = first.iter().collect();
    assert_eq!(first.len(), unique.len(), "duplicates survived: {first:?}");
}

/// **A fused flag table yields every flag in it, not one blob.**
///
/// A vendor stores its flags concatenated, so a single token can be five flags with no
/// separators — measured on the real `codex.exe`, whose table reads
/// `--sandbox-policy-cwd--permission-profile--use-legacy-landlock--managed-networkcommand`.
/// Keeping that as one string did two things wrong: the report showed a blob instead of naming
/// a flag, and because a kept string is truncated at the per-string cap, a change *late in the
/// blob* was invisible. The second is the serious one — it is this leg's primary signal, lost
/// to a bound meant for prose.
#[test]
fn a_fused_flag_table_yields_every_flag_in_it() {
    let fused = b"--sandbox-policy-cwd--permission-profile--use-legacy-landlock\x00";
    let found = interesting_strings(fused, 64);

    for expected in [
        "--sandbox-policy-cwd",
        "--permission-profile",
        "--use-legacy-landlock",
    ] {
        assert!(
            found.iter().any(|s| s == expected),
            "`{expected}` must be found as its own entry, not fused into a blob: {found:?}"
        );
    }
    assert!(
        found.iter().any(|s| s == "--use-legacy-landlock"),
        "{found:?}"
    );
    // The material claim is that **every flag is its own entry**, because that is what removes
    // the truncation blind spot: a 200-character cap over the whole blob would hide a change
    // late in it. A fused blob may *also* appear, and that is a second, deliberate path rather
    // than a failure of this one — see `a_policy_word_keeps_its_sentence_whole` for that rule,
    // and `ACCEPTANCE.md` §14's S17 for the duplication it creates.
    for expected in [
        "--sandbox-policy-cwd",
        "--permission-profile",
        "--use-legacy-landlock",
    ] {
        assert!(
            found.iter().any(|s| s == expected),
            "`{expected}` must survive the split as its own entry: {found:?}"
        );
    }
}

/// A run carrying a policy word is kept **whole**, by design.
///
/// That is the other path, and it is why a fused flag table can appear twice: once split into
/// flags, and once as the sentence around `permission`. The sentence is the evidence a human
/// judges, so keeping it is deliberate — the duplication is recorded rather than papered over.
#[test]
fn a_policy_word_keeps_its_sentence_whole() {
    let found = interesting_strings(b"ask for permission before writing files\x00", 64);
    assert!(
        found
            .iter()
            .any(|s| s == "ask for permission before writing files"),
        "the sentence around the policy word is the evidence: {found:?}"
    );
}

/// The unfused case is unchanged, and a double hyphen still ends a body.
#[test]
fn a_single_flag_is_one_entry_and_a_double_hyphen_still_ends_a_name() {
    let found = interesting_strings(b"--print-timeout\x00--add-dir\x00", 16);
    assert_eq!(
        found,
        vec!["--add-dir".to_string(), "--print-timeout".to_string()],
        "two standalone flags stay two entries, sorted"
    );

    // `--approve-for-me` must not absorb a following flag with no separator — the boundary is
    // the double hyphen, and a real long flag never contains one.
    let found = interesting_strings(b"x--approve-for-me--sandbox\x00", 16);
    assert!(found.iter().any(|s| s == "--approve-for-me"), "{found:?}");
    assert!(found.iter().any(|s| s == "--sandbox"), "{found:?}");
}
