//! Gate for M7 stage B: what consolidation writes, and what it undoes
//! (`planning/ACCEPTANCE.md` §15, `planning/DESIGN.md` §16).
//!
//! Everything here runs offline and spawns nothing. Each test builds its own home in a
//! `tempfile::tempdir()` and passes it explicitly to `apply`/`revert`, so no test reads
//! or writes the real `~/.pie`, and none of them touches a vendor store — the default
//! path reads pie's own files only (`RISKS.md` §15).
//!
//! The three properties stage B exists for each have a test that would go red if they
//! regressed: `..._is_still_there_when_the_document_is_read_back` (decay demotes rather
//! than deletes), `apply_then_revert_restores_the_previous_bytes_exactly` (byte
//! comparison, not "looks right"), and
//! `consolidation_does_not_touch_the_persona_or_reorder_the_head_of_the_projection`
//! (`TOKEN-ECONOMICS.md` §3).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;

use pie::learn::{Candidate, CandidateKind, Provenance, Source};
use pie::memory::{
    self, Journal, DECAY_WINDOW_MS, DEFAULT_HEAD, LEARNED_FILE, MAX_ENTRY_CHARS, TEMP_FILE,
};
use pie::profile::Profile;

/// 2023-11-14T22:13:20Z.
///
/// The document records the *day* a fact was last seen, so an entry's `last_seen_ms`
/// after a write-then-read is midnight of that day. Every boundary below is measured
/// from [`SEEN_DAY`] for exactly that reason.
const SEEN_AT: u64 = 1_700_000_000_000;
const SEEN_DAY: u64 = 1_699_920_000_000;
const A_DAY: u64 = 24 * 60 * 60 * 1_000;

const PROFILE_TOML: &str = r#"
name = "pie"
tone = "terse"
rules = ["Answer with the smallest correct change."]
"#;

const AGENTS_MD: &str = "AGENTS-MARKER: prefer an existing helper over a new abstraction.\n";

/// A preamble written by the user, not by pie. Consolidation may read it and must
/// re-emit it byte for byte — a head that is re-rendered is a prefix the vendor has to
/// re-prefill.
const CUSTOM_HEAD: &str = "# Learned\n\nmy own notes, keep diffs small.\n\n";

/// One entry, in the format `pie memory show` prints.
const CUSTOM_ENTRY: &str =
    "- [preference] keep diffs small — hits 2; last 2023-01-01; pie-session s1 turn 0; confidence demoted\n";

fn candidate(kind: CandidateKind, text: &str, hits: u32) -> Candidate {
    candidate_at(kind, text, hits, SEEN_AT, "s7f2", 3)
}

fn candidate_at(
    kind: CandidateKind,
    text: &str,
    hits: u32,
    last_seen_ms: u64,
    session_id: &str,
    turn_index: usize,
) -> Candidate {
    Candidate {
        kind,
        text: text.to_string(),
        provenance: Provenance {
            session_id: session_id.to_string(),
            turn_index,
            source: Source::PieSession,
        },
        hits,
        last_seen_ms,
    }
}

fn write(root: &Path, name: &str, text: &str) {
    let path = root.join(name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, text).unwrap();
}

fn write_persona(home: &Path) {
    write(home, "profile.toml", PROFILE_TOML);
    write(home, "AGENTS.md", AGENTS_MD);
}

/// The document as bytes on disk, so a comparison is a byte comparison and an absent file
/// is distinguishable from an empty one.
fn doc_bytes(home: &Path) -> Option<Vec<u8>> {
    std::fs::read(memory::doc_path(home)).ok()
}

fn doc_text(home: &Path) -> String {
    std::fs::read_to_string(memory::doc_path(home)).unwrap_or_default()
}

fn count(haystack: &str, needle: &str) -> usize {
    haystack.matches(needle).count()
}

/// A fact the document does not hold is written, and the line names the session and turn
/// it came from, because a fact you cannot trace is one you cannot judge (`DESIGN.md` §16).
#[test]
fn a_new_candidate_is_written_with_the_session_and_turn_it_came_from() {
    let home = tempfile::tempdir().unwrap();
    write_persona(home.path());

    let fact = "always run the offline gates before committing";
    let candidates = [candidate_at(
        CandidateKind::Preference,
        fact,
        2,
        SEEN_AT,
        "s7f2",
        3,
    )];
    let existing = memory::read_doc(home.path()).unwrap();
    let decision = memory::consolidate(&existing, &candidates, SEEN_AT);

    assert_eq!(decision.added.len(), 1, "one new fact, one new line");
    assert!(decision.reinforced.is_empty(), "{decision:#?}");
    assert!(decision.demoted.is_empty(), "{decision:#?}");
    let line = decision.added[0].clone();
    assert!(line.contains("[preference]"), "{line}");
    assert!(line.contains("pie-session s7f2 turn 3"), "{line}");
    assert!(line.contains("last 2023-11-14"), "{line}");

    let journal = memory::apply(&decision, home.path()).unwrap();
    assert_eq!(journal.added, vec![line.clone()], "the journal reports it");

    let written = doc_text(home.path());
    assert!(
        written.starts_with(DEFAULT_HEAD),
        "a document that did not exist is created with the default head\n{written}"
    );
    assert!(
        written.lines().any(|l| l == line.as_str()),
        "the entry is a line of its own, not glued to the preamble\n{written}"
    );

    // Written *and* part of the persona: a fact in a file the projection never reads is
    // not learned.
    let profile = Profile::load(home.path()).unwrap();
    assert!(
        profile.projection().contains(fact),
        "{}",
        profile.projection()
    );
}

/// The duplicating implementation passes "the document contains it". This one counts.
#[test]
fn a_candidate_the_document_already_holds_is_reinforced_and_stored_exactly_once() {
    let home = tempfile::tempdir().unwrap();
    write_persona(home.path());
    let fact = "prefer an existing helper to a new abstraction";

    let first = memory::consolidate(
        &memory::read_doc(home.path()).unwrap(),
        &[candidate(CandidateKind::Convention, fact, 2)],
        SEEN_AT,
    );
    memory::apply(&first, home.path()).unwrap();
    let after_first = doc_text(home.path());

    let second = memory::consolidate(
        &memory::read_doc(home.path()).unwrap(),
        &[candidate_at(
            CandidateKind::Convention,
            fact,
            5,
            SEEN_AT + A_DAY,
            "s8",
            1,
        )],
        SEEN_AT + A_DAY,
    );
    assert_eq!(second.reinforced.len(), 1, "an already-known fact");
    assert!(
        second.added.is_empty(),
        "is never added a second time: {:#?}",
        second.added
    );
    memory::apply(&second, home.path()).unwrap();

    let after_second = doc_text(home.path());
    assert_eq!(
        count(&after_second, fact),
        1,
        "it appears exactly once\n{after_second}"
    );
    assert_eq!(
        after_second.lines().count(),
        after_first.lines().count(),
        "and takes no extra line"
    );
    assert!(after_second.contains("hits 5"), "{after_second}");
    assert!(
        after_second.contains("pie-session s8 turn 1"),
        "the trace follows the newest evidence\n{after_second}"
    );

    // Case and a trailing full stop are the same fact, not a second line.
    let third = memory::consolidate(
        &memory::read_doc(home.path()).unwrap(),
        &[candidate_at(
            CandidateKind::Convention,
            "Prefer an existing helper to a new abstraction.",
            6,
            SEEN_AT + 2 * A_DAY,
            "s9",
            0,
        )],
        SEEN_AT + 2 * A_DAY,
    );
    assert_eq!(third.reinforced.len(), 1, "one fact, however it is spelled");
    assert!(third.added.is_empty());
    memory::apply(&third, home.path()).unwrap();
    assert_eq!(count(&doc_text(home.path()), fact), 1, "still exactly once");
}

/// Reinforcement rewrites the fact's own line in place. Nothing moves, and the head is
/// not re-rendered: that is the difference between an update and a reorder.
#[test]
fn re_observing_a_fact_rewrites_its_own_line_and_moves_nothing_else() {
    let home = tempfile::tempdir().unwrap();
    write_persona(home.path());
    write(
        home.path(),
        "memory/learned.md",
        &format!("{CUSTOM_HEAD}{CUSTOM_ENTRY}"),
    );

    let decision = memory::consolidate(
        &memory::read_doc(home.path()).unwrap(),
        &[candidate_at(
            CandidateKind::Preference,
            "keep diffs small",
            4,
            SEEN_AT,
            "s9",
            2,
        )],
        SEEN_AT,
    );
    assert_eq!(decision.reinforced.len(), 1, "{decision:#?}");
    assert!(decision.added.is_empty() && decision.demoted.is_empty());
    memory::apply(&decision, home.path()).unwrap();

    let after = doc_text(home.path());
    assert!(
        after.starts_with(CUSTOM_HEAD),
        "the head is re-emitted, not re-rendered\n{after}"
    );
    assert_eq!(
        after.lines().count(),
        CUSTOM_HEAD.lines().count() + 1,
        "one entry line in, one entry line out\n{after}"
    );
    let line = decision.reinforced[0].clone();
    assert!(line.contains("hits 4"), "{line}");
    assert!(line.contains("confidence full"), "{line}");
    assert!(line.contains("pie-session s9 turn 2"), "{line}");
    assert!(
        after.lines().any(|l| l == line.as_str()),
        "the reinforced line is where the old one was\n{after}"
    );
}

/// `RISKS.md` §16: memory that rots invisibly. Decay lowers confidence; it never removes
/// the fact, and the fact is still in the projection afterwards.
#[test]
fn a_fact_outside_the_decay_window_is_demoted_and_is_still_there_when_the_document_is_read_back() {
    let home = tempfile::tempdir().unwrap();
    write_persona(home.path());
    let fact = "keep diffs small";

    let first = memory::consolidate(
        &memory::read_doc(home.path()).unwrap(),
        &[candidate_at(
            CandidateKind::Preference,
            fact,
            2,
            SEEN_AT,
            "s1",
            0,
        )],
        SEEN_AT,
    );
    memory::apply(&first, home.path()).unwrap();

    let decision = memory::consolidate(
        &memory::read_doc(home.path()).unwrap(),
        &[],
        SEEN_DAY + DECAY_WINDOW_MS + 1,
    );
    assert_eq!(decision.demoted.len(), 1, "{decision:#?}");
    assert!(decision.added.is_empty() && decision.reinforced.is_empty());
    assert!(decision.skipped.is_empty(), "nothing was decided about it");
    assert!(
        decision.demoted[0].contains("confidence demoted"),
        "{}",
        decision.demoted[0]
    );
    memory::apply(&decision, home.path()).unwrap();

    let after = memory::read_doc(home.path()).unwrap();
    assert!(
        after.learned.contains(fact),
        "demoted, not deleted — it is still present when the document is read back\n{}",
        after.learned
    );
    assert!(after.learned.contains("confidence demoted"));
    assert_eq!(count(&after.learned, fact), 1, "and still once");
    assert!(
        Profile::load(home.path())
            .unwrap()
            .projection()
            .contains(fact),
        "the demoted fact is still part of the projection"
    );

    // A demoted fact is not a dead fact: being seen again restores full confidence rather
    // than adding a second line.
    let back = memory::consolidate(
        &after,
        &[candidate_at(
            CandidateKind::Preference,
            fact,
            3,
            SEEN_DAY + DECAY_WINDOW_MS + 2 * A_DAY,
            "s2",
            4,
        )],
        SEEN_DAY + DECAY_WINDOW_MS + 2 * A_DAY,
    );
    assert_eq!(back.reinforced.len(), 1, "{back:#?}");
    assert!(back.added.is_empty());
    assert!(back.reinforced[0].contains("confidence full"));
    memory::apply(&back, home.path()).unwrap();
    assert_eq!(count(&doc_text(home.path()), fact), 1);
}

/// The classic off-by-one: at exactly the window a fact is still inside it.
#[test]
fn a_fact_exactly_at_the_decay_boundary_is_not_demoted_yet() {
    let home = tempfile::tempdir().unwrap();
    write_persona(home.path());
    let fact = "keep diffs small";

    let first = memory::consolidate(
        &memory::read_doc(home.path()).unwrap(),
        &[candidate_at(
            CandidateKind::Preference,
            fact,
            2,
            SEEN_AT,
            "s1",
            0,
        )],
        SEEN_AT,
    );
    memory::apply(&first, home.path()).unwrap();
    let existing = memory::read_doc(home.path()).unwrap();

    assert!(
        memory::consolidate(&existing, &[], SEEN_DAY + DECAY_WINDOW_MS)
            .demoted
            .is_empty(),
        "at the window's edge the fact is still inside it"
    );
    assert_eq!(
        memory::consolidate(&existing, &[], SEEN_DAY + DECAY_WINDOW_MS + 1)
            .demoted
            .len(),
        1,
        "one millisecond past it, it is demoted"
    );
    assert_eq!(
        memory::consolidate(&existing, &[], SEEN_DAY + DECAY_WINDOW_MS - 1)
            .demoted
            .len(),
        0,
        "and one millisecond before it, it is not"
    );
}

/// The revert handle is only worth anything if it restores *bytes*. A comparison against
/// a re-rendered document passes for an implementation that drops a trailing blank line,
/// so this fixture is hand-written prose with no trailing newline — bytes pie did not
/// produce, and therefore cannot reconstruct.
#[test]
fn apply_then_revert_restores_the_previous_bytes_exactly() {
    let home = tempfile::tempdir().unwrap();
    write_persona(home.path());
    let before = format!(
        "{CUSTOM_HEAD}{CUSTOM_ENTRY}- [stack] rg over grep — hits 9; last 2025-12-25; pie-session s2 turn 1; confidence full\n\nprose written by hand, and no trailing newline"
    );
    write(home.path(), "memory/learned.md", &before);

    let decision = memory::consolidate(
        &memory::read_doc(home.path()).unwrap(),
        &[
            candidate_at(CandidateKind::Stack, "rg over grep", 11, SEEN_AT, "s3", 2),
            candidate(
                CandidateKind::Terminology,
                "a \"gate\" is one check in qc.ps1",
                2,
            ),
        ],
        SEEN_DAY,
    );
    assert_eq!(
        (
            decision.added.len(),
            decision.reinforced.len(),
            decision.demoted.len()
        ),
        (1, 1, 1),
        "{decision:#?}"
    );

    let journal = memory::apply(&decision, home.path()).unwrap();
    assert_ne!(
        doc_bytes(home.path()).unwrap(),
        before.as_bytes(),
        "the apply has to have written something, or the revert proves nothing"
    );
    assert_eq!(journal.previous, before, "the journal holds the old bytes");

    memory::revert(&journal, home.path()).unwrap();
    assert_eq!(
        doc_bytes(home.path()).unwrap(),
        before.as_bytes(),
        "byte for byte: {}",
        doc_text(home.path())
    );
    assert_eq!(
        memory::read_journal(home.path()).unwrap(),
        None,
        "the undo has been performed, so the journal is consumed"
    );
}

/// A journal whose `previous` is what is already on disk must not corrupt it. This is the
/// shape a re-run of `pie memory revert` takes.
#[test]
fn reverting_a_journal_that_already_matches_the_document_changes_nothing() {
    let home = tempfile::tempdir().unwrap();
    write_persona(home.path());
    let text = format!("{CUSTOM_HEAD}{CUSTOM_ENTRY}");
    write(home.path(), "memory/learned.md", &text);

    let journal = Journal {
        previous: text.clone(),
        added: vec!["- [stack] rg over grep".to_string()],
    };
    memory::revert(&journal, home.path()).unwrap();
    assert_eq!(doc_bytes(home.path()).unwrap(), text.as_bytes());

    // Twice, because a second revert finds no journal and must still be harmless.
    memory::revert(&journal, home.path()).unwrap();
    assert_eq!(doc_bytes(home.path()).unwrap(), text.as_bytes());
}

/// The first run creates the document. Undoing it means absent, not empty: an empty
/// contribution and a missing one are the same zero bytes in the projection, and the
/// state before the apply was "no file".
#[test]
fn reverting_an_apply_that_created_the_document_removes_it() {
    let home = tempfile::tempdir().unwrap();
    write_persona(home.path());
    assert!(!memory::doc_path(home.path()).exists());

    let persona_only = Profile::load(home.path()).unwrap().projection().to_string();
    assert!(!persona_only.is_empty(), "the fixture persona is set");

    let decision = memory::consolidate(
        &memory::read_doc(home.path()).unwrap(),
        &[candidate(CandidateKind::Stack, "ripgrep for searching", 3)],
        SEEN_AT,
    );
    let journal = memory::apply(&decision, home.path()).unwrap();
    assert!(memory::doc_path(home.path()).is_file());
    assert_ne!(
        Profile::load(home.path()).unwrap().projection(),
        persona_only
    );

    memory::revert(&journal, home.path()).unwrap();
    assert!(
        !memory::doc_path(home.path()).exists(),
        "restored to what it was: no document"
    );
    assert_eq!(
        Profile::load(home.path()).unwrap().projection(),
        persona_only,
        "and the projection is back to the persona alone"
    );
}

/// A daily job re-derives the same facts every day. Applying the same decision twice must
/// be a no-op, and so must a second day's *re-derived* decision over the same sessions.
#[test]
fn applying_the_same_consolidation_twice_does_not_double_the_document() {
    let home = tempfile::tempdir().unwrap();
    write_persona(home.path());
    let fact = "ripgrep for searching";
    let candidates = [candidate(CandidateKind::Stack, fact, 3)];

    let first = memory::consolidate(
        &memory::read_doc(home.path()).unwrap(),
        &candidates,
        SEEN_AT,
    );
    memory::apply(&first, home.path()).unwrap();
    let once = doc_bytes(home.path()).unwrap();

    let second = memory::apply(&first, home.path()).unwrap();
    assert!(second.added.is_empty(), "the second apply added nothing");
    assert_eq!(
        doc_bytes(home.path()).unwrap(),
        once,
        "{}",
        doc_text(home.path())
    );
    assert_eq!(count(&doc_text(home.path()), fact), 1);

    // And the same thing arrived at the way a daily run arrives at it: re-mine the same
    // sessions, re-consolidate, re-apply. Nothing churns.
    let again = memory::consolidate(
        &memory::read_doc(home.path()).unwrap(),
        &candidates,
        SEEN_AT,
    );
    assert_eq!(again.reinforced.len(), 1, "{again:#?}");
    let third = memory::apply(&again, home.path()).unwrap();
    assert!(third.added.is_empty());
    assert_eq!(
        doc_bytes(home.path()).unwrap(),
        once,
        "a second run over the same sessions produces the same bytes\n{}",
        doc_text(home.path())
    );
}

/// "Pure" is a claim about the filesystem, so it gets a filesystem test.
#[test]
fn consolidating_writes_nothing_even_when_it_decides_to_add() {
    let home = tempfile::tempdir().unwrap();
    write_persona(home.path());
    write(
        home.path(),
        "memory/learned.md",
        &format!("{CUSTOM_HEAD}{CUSTOM_ENTRY}"),
    );
    let before = doc_bytes(home.path()).unwrap();

    let decision = memory::consolidate(
        &memory::read_doc(home.path()).unwrap(),
        &[
            candidate(CandidateKind::Stack, "ripgrep for searching", 3),
            candidate(CandidateKind::Preference, "small diffs", 1),
        ],
        SEEN_DAY + DECAY_WINDOW_MS + 1,
    );
    assert!(!decision.added.is_empty());
    assert!(!decision.demoted.is_empty());

    assert_eq!(doc_bytes(home.path()).unwrap(), before, "nothing written");
    assert!(
        !memory::journal_path(home.path()).exists(),
        "and no journal, because there is nothing to undo"
    );
    assert!(
        !memory::memory_dir(home.path()).join(TEMP_FILE).exists(),
        "and no scratch file left behind"
    );
}

/// The load-bearing prefix rule (`TOKEN-ECONOMICS.md` §3, `DESIGN.md` §16): a
/// consolidation appends. The projected head is the bytes before the *first* learned
/// entry — persona, `AGENTS.md`, the user's own memory files, and the learned document's
/// own preamble. All of it must be identical, or the vendor re-prefills the whole
/// projection once a day.
#[test]
fn consolidation_does_not_touch_the_persona_or_reorder_the_head_of_the_projection() {
    let home = tempfile::tempdir().unwrap();
    write_persona(home.path());
    write(
        home.path(),
        "memory/01-notes.md",
        "MEMORY-ONE: the release gate is `scripts/qc.sh`.\n",
    );
    write(
        home.path(),
        "memory/learned.md",
        &format!("{CUSTOM_HEAD}{CUSTOM_ENTRY}"),
    );

    let profile_before = std::fs::read(home.path().join("profile.toml")).unwrap();
    let agents_before = std::fs::read(home.path().join("AGENTS.md")).unwrap();

    let before = Profile::load(home.path()).unwrap().projection().to_string();
    let old_learned = doc_text(home.path());
    let learned_at = before
        .find(old_learned.trim_end())
        .expect("the learned file is a contribution to the projection");
    let head_len = learned_at + old_learned.find("- [").expect("the fixture has one entry");

    let decision = memory::consolidate(
        &memory::read_doc(home.path()).unwrap(),
        &[candidate(CandidateKind::Stack, "ripgrep for searching", 3)],
        SEEN_AT,
    );
    assert_eq!(decision.added.len(), 1);
    memory::apply(&decision, home.path()).unwrap();

    let after = Profile::load(home.path()).unwrap().projection().to_string();
    assert_eq!(
        &after.as_bytes()[..head_len],
        &before.as_bytes()[..head_len],
        "the projection's first {head_len} bytes must be unchanged; rewriting them \
         invalidates the vendor's cached prefix once a day\nbefore: {before}\nafter: {after}"
    );
    assert_eq!(
        std::fs::read(home.path().join("profile.toml")).unwrap(),
        profile_before,
        "consolidation never rewrites profile.toml"
    );
    assert_eq!(
        std::fs::read(home.path().join("AGENTS.md")).unwrap(),
        agents_before,
        "consolidation never rewrites AGENTS.md"
    );

    let new_learned = doc_text(home.path());
    assert!(
        new_learned.starts_with(old_learned.trim_end()),
        "the run appends: everything the document already held is still its prefix\n\
         before: {old_learned}\nafter: {new_learned}"
    );
    assert!(after.contains("ripgrep for searching"));
}

/// "Never silent" (`PLAN §11`): a candidate that is not written says why, and two
/// different reasons are not the same string.
#[test]
fn every_skipped_candidate_says_why() {
    let home = tempfile::tempdir().unwrap();
    write(
        home.path(),
        "memory/learned.md",
        &format!("# Learned\n\nmy own notes, tabs over two spaces.\n\n{CUSTOM_ENTRY}"),
    );
    let existing = memory::read_doc(home.path()).unwrap();

    let too_long = "x".repeat(MAX_ENTRY_CHARS + 1);
    let candidates = [
        candidate(CandidateKind::Preference, "  \n\t ", 3),
        candidate(CandidateKind::Stack, &too_long, 3),
        candidate(CandidateKind::Habit, "run tests before committing", 1),
        candidate(CandidateKind::Preference, "tabs over two spaces", 2),
        candidate(CandidateKind::Stack, "ripgrep for searching", 3),
        candidate(CandidateKind::Stack, "Ripgrep for searching.", 3),
    ];
    let decision = memory::consolidate(&existing, &candidates, SEEN_AT);

    assert_eq!(decision.added.len(), 1, "{decision:#?}");
    assert_eq!(decision.skipped.len(), 5, "{decision:#?}");
    for entry in &decision.skipped {
        let (subject, reason) = entry
            .split_once(" — ")
            .unwrap_or_else(|| panic!("a skipped candidate names its reason: {entry}"));
        assert!(!reason.trim().is_empty(), "{entry}");
        assert!(!subject.trim().is_empty(), "{entry}");
    }
    let reasons: Vec<&str> = decision
        .skipped
        .iter()
        .map(|entry| entry.split_once(" — ").unwrap().1)
        .collect();
    let distinct: std::collections::BTreeSet<&str> = reasons.iter().copied().collect();
    assert_eq!(
        distinct.len(),
        reasons.len(),
        "five different skips, five different reasons: {reasons:#?}"
    );

    memory::apply(&decision, home.path()).unwrap();
    let written = doc_text(home.path());
    assert_eq!(
        written.lines().filter(|l| l.starts_with("- [")).count(),
        2,
        "only the one fact was written, on top of the entry already there\n{written}"
    );
    assert!(!written.contains(&too_long));
}

/// The stub returned `Ok(Journal::default())` for every input, including one whose write
/// failed. A learned persona that is reported as applied and is not is the failure this
/// guards, and the journal has to exist before the document is touched for a failure to
/// be recoverable.
#[test]
fn a_failed_document_write_is_an_error_and_leaves_the_journal_behind() {
    let home = tempfile::tempdir().unwrap();
    write_persona(home.path());
    let before = "hand written prose\n";
    write(home.path(), "memory/learned.md", before);
    // Occupy the scratch path the document is written through, so the write fails at the
    // same point a full disk or a permissions change would.
    std::fs::create_dir_all(memory::memory_dir(home.path()).join(TEMP_FILE)).unwrap();

    let decision = memory::consolidate(
        &memory::read_doc(home.path()).unwrap(),
        &[candidate(CandidateKind::Stack, "ripgrep for searching", 3)],
        SEEN_AT,
    );
    let error = memory::apply(&decision, home.path())
        .expect_err("a write that failed must not report success");
    let _ = error.to_string();

    assert_eq!(
        doc_bytes(home.path()).unwrap(),
        before.as_bytes(),
        "the document is untouched"
    );
    assert!(
        memory::journal_path(home.path()).is_file(),
        "the journal is written before the document, so a crash between the two is \
         recoverable by revert"
    );
    let journal = memory::read_journal(home.path()).unwrap().unwrap();
    assert_eq!(journal.previous, before);

    // The undo writes through the same scratch name, so it is blocked by the same
    // obstacle — and reports rather than pretending, which is how a recoverable failure
    // stays recoverable.
    assert!(memory::revert(&journal, home.path()).is_err());
    assert_eq!(doc_bytes(home.path()).unwrap(), before.as_bytes());

    std::fs::remove_dir(memory::memory_dir(home.path()).join(TEMP_FILE)).unwrap();
    memory::revert(&journal, home.path()).unwrap();
    assert_eq!(doc_bytes(home.path()).unwrap(), before.as_bytes());
    assert_eq!(memory::read_journal(home.path()).unwrap(), None);
}

/// The document must stay a `memory/*.md` contribution, and the journal must not be one:
/// an undo handle inside the projection would put JSON in the prompt, and a source
/// `profile.rs` cannot see would mean consolidation wrote nothing anyone reads.
#[test]
fn the_learned_document_is_a_memory_contribution_and_the_journal_is_not() {
    let home = tempfile::tempdir().unwrap();
    write_persona(home.path());
    let decision = memory::consolidate(
        &memory::read_doc(home.path()).unwrap(),
        &[candidate(CandidateKind::Stack, "ripgrep for searching", 3)],
        SEEN_AT,
    );
    memory::apply(&decision, home.path()).unwrap();
    assert!(memory::journal_path(home.path()).is_file());

    let profile = Profile::load(home.path()).unwrap();
    let sources: Vec<String> = profile
        .contributions()
        .iter()
        .map(|c| c.source.to_string_lossy().to_string())
        .collect();
    assert!(
        sources.iter().any(|s| s.ends_with(LEARNED_FILE)),
        "the learned document is a source of the projection: {sources:?}"
    );
    assert!(
        !sources.iter().any(|s| s.contains("journal")),
        "the journal is not: {sources:?}"
    );
    assert!(
        profile.projection().contains("ripgrep for searching"),
        "and what it holds reaches the persona"
    );
}
