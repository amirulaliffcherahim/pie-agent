//! M7 stage A gate: mining pie's own sessions, and the rule-based extractor.
//!
//! Contract: `planning/DESIGN.md` §16, criteria: `planning/ACCEPTANCE.md` §15 (row
//! "A extract"). The gate is `cargo test --test learn`.
//!
//! Three properties are the whole point of this stage, and each is asserted rather
//! than described:
//!
//! * **Deterministic** — two runs over the same turns are byte-identical, so a report
//!   can be diffed. A report you cannot diff is one you cannot review.
//! * **Pure** — `extract` spawns nothing, reads nothing, and never reads a clock; the
//!   `last_seen_ms` of a candidate is the store's own timestamp.
//! * **Quota-free** — nothing here runs a vendor CLI, and there is no `PIE_LIVE` and
//!   no network anywhere in this file. `RISKS.md` §13 is the governing risk.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::sync::{Mutex, MutexGuard, OnceLock};

use pie::learn::{extract, mine_sessions, Candidate, CandidateKind, MinedTurn, Source};
use pie::session::{Session, TurnRow};

/// A fixed clock reading for hand-built turns. It is decades in the past on purpose:
/// a `last_seen_ms` near *now* would mean `extract` read a clock instead of the turn.
const BASE_MS: u64 = 1_700_000_000_000;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// One successful or failed turn, timestamped from its index.
fn turn(session: &str, index: usize, prompt: &str, ok: bool) -> MinedTurn {
    MinedTurn {
        session_id: session.to_string(),
        turn_index: index,
        source: Source::PieSession,
        prompt: prompt.to_string(),
        answer: String::new(),
        ok,
        ts: BASE_MS + index as u64,
    }
}

/// A turn whose *answer* contains text, to prove the answer is not evidence.
fn turn_with_answer(session: &str, index: usize, prompt: &str, answer: &str) -> MinedTurn {
    MinedTurn {
        answer: answer.to_string(),
        ..turn(session, index, prompt, true)
    }
}

fn candidate(
    kind: CandidateKind,
    text: &str,
    hits: u32,
    session: &str,
    turn_index: usize,
    last_seen_ms: u64,
) -> Candidate {
    Candidate {
        kind,
        text: text.to_string(),
        provenance: pie::learn::Provenance {
            session_id: session.to_string(),
            turn_index,
            source: Source::PieSession,
        },
        hits,
        last_seen_ms,
    }
}

fn has(candidates: &[Candidate], kind: CandidateKind, text: &str) -> bool {
    candidates.iter().any(|c| c.kind == kind && c.text == text)
}

/// The position of a fact in the report, which is the order a reader sees.
fn position(candidates: &[Candidate], kind: CandidateKind, text: &str) -> usize {
    candidates
        .iter()
        .position(|c| c.kind == kind && c.text == text)
        .unwrap_or_else(|| panic!("no {kind:?} candidate {text:?} in {candidates:#?}"))
}

/// A fixture with exactly the shapes the rules name, small enough to pin by hand.
///
/// `always run cargo fmt …` twice (a stated preference, and a `Stack` term inside it),
/// and `audit <thing>` three times (a habit, and three one-off directives that are
/// *not* a convention). Nothing here repeats inside a single turn, so every `hits`
/// below is a count of turns.
fn sample_turns() -> Vec<MinedTurn> {
    vec![
        turn("s-1-1", 0, "always run cargo fmt before committing.", true),
        turn("s-1-2", 1, "always run cargo fmt before committing.", true),
        turn("s-1-2", 2, "audit the renderer.", true),
        turn("s-2-1", 3, "audit the picker.", true),
        turn("s-2-1", 4, "audit the reducer.", true),
    ]
}

/// Points `PIE_HOME` at a directory for the duration of the block.
///
/// `src/testing.rs`'s `EnvGuard` is the house guard, but it is `pub(crate)` and
/// therefore unavailable from an integration test; this is the same shape as
/// `tests/parse_codex.rs`'s `HomeGuard`, with its own lock so the two never contend.
/// Held for the shortest possible scope — the lock serialises every env-touching test
/// in this binary, and a guard taken twice on one thread would deadlock it (B24).
#[derive(Debug)]
struct EnvPin {
    _guard: MutexGuard<'static, ()>,
    saved: Option<String>,
}

impl EnvPin {
    fn new(pie_home: &Path) -> Self {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        let guard = LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        let saved = std::env::var("PIE_HOME").ok();
        std::env::set_var("PIE_HOME", pie_home);
        Self {
            _guard: guard,
            saved,
        }
    }
}

impl Drop for EnvPin {
    fn drop(&mut self) {
        match &self.saved {
            Some(value) => std::env::set_var("PIE_HOME", value),
            None => std::env::remove_var("PIE_HOME"),
        }
    }
}

/// A recorded row with an explicit timestamp, so the mining test can name the value
/// that must survive into `last_seen_ms`.
fn row(ts: u64, prompt: &str, ok: bool) -> TurnRow {
    TurnRow {
        ts,
        provider: "agy".to_string(),
        model: "gemini-3.8-flash".to_string(),
        prompt: prompt.to_string(),
        ok,
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// Determinism and purity
// ---------------------------------------------------------------------------

/// A report you cannot diff is one you cannot review. This asserts on the whole
/// vector, and it also asserts the report does not depend on the order the caller
/// listed the turns in — otherwise a miner that returns sessions in a different order
/// would move facts around in the output for no reason.
#[test]
fn extracting_the_same_turns_twice_is_byte_identical() {
    let turns = sample_turns();

    let first = extract(&turns);
    let second = extract(&turns);
    assert_eq!(
        first, second,
        "two runs over the same turns must be identical"
    );
    assert!(
        !first.is_empty(),
        "the fixture must produce candidates, or this test proves nothing"
    );

    let mut reversed = turns.clone();
    reversed.reverse();
    assert_eq!(
        extract(&reversed),
        first,
        "the order the caller hands turns in must not change the report"
    );
}

/// The whole report, pinned by hand: kind, text, recurrence, provenance and timestamp.
///
/// This is the strongest form of the determinism claim — a fact appearing in a
/// different position, or with a `hits` count that came from the wrong place, fails
/// here even when two calls in one process happen to agree.
#[test]
fn the_report_for_a_fixture_is_exactly_the_expected_list_of_facts() {
    let expected = vec![
        // Three turns open with `audit`, and the verb was never stated as a rule, so
        // the habit carries the most observations and leads the report.
        candidate(
            CandidateKind::Habit,
            "habitually asks to audit",
            3,
            "s-2-1",
            4,
            BASE_MS + 4,
        ),
        candidate(
            CandidateKind::Preference,
            "always run cargo fmt before committing",
            2,
            "s-1-2",
            1,
            BASE_MS + 1,
        ),
        candidate(
            CandidateKind::Stack,
            "uses cargo",
            2,
            "s-1-2",
            1,
            BASE_MS + 1,
        ),
    ];

    assert_eq!(extract(&sample_turns()), expected);
}

/// `extract` is pure: it reads nothing, so where `PIE_HOME` points cannot matter, and
/// it must not create the directory it was pointed at either.
#[test]
fn extract_reads_nothing_even_when_pie_home_points_at_a_missing_directory() {
    let turns = sample_turns();
    let expected = extract(&turns);

    let scratch = tempfile::tempdir().unwrap();
    let missing = scratch.path().join("a-pie-home-that-does-not-exist");

    {
        let _env = EnvPin::new(&missing);
        assert_eq!(
            extract(&turns),
            expected,
            "extract must not consult PIE_HOME: it takes its input as an argument"
        );
    }

    assert!(
        !missing.exists(),
        "extract must not create anything under PIE_HOME: {}",
        missing.display()
    );
}

// ---------------------------------------------------------------------------
// Recurrence, kind and provenance
// ---------------------------------------------------------------------------

/// A habit the user has outranks a one-off — and it is recurrence, not the number of
/// times a fact appeared inside one prompt, that decides: all three observations of
/// the first fact are separate turns.
#[test]
fn a_candidate_seen_more_turns_reports_more_hits_and_outranks_one_seen_once() {
    let turns = vec![
        turn("s-1-1", 0, "always run cargo fmt", true),
        turn("s-1-1", 1, "always run cargo fmt", false),
        turn("s-1-1", 2, "always run cargo fmt", false),
        turn("s-1-2", 0, "never touch the vendor store", true),
    ];

    let found = extract(&turns);
    let repeated = position(&found, CandidateKind::Preference, "always run cargo fmt");
    let once = position(
        &found,
        CandidateKind::Preference,
        "never touch the vendor store",
    );

    let hits_of = |kind: CandidateKind, text: &str| {
        found
            .iter()
            .find(|c| c.kind == kind && c.text == text)
            .unwrap_or_else(|| panic!("no {kind:?} {text:?}"))
            .hits
    };
    assert_eq!(
        hits_of(CandidateKind::Preference, "always run cargo fmt"),
        3
    );
    assert_eq!(
        hits_of(CandidateKind::Preference, "never touch the vendor store"),
        1
    );
    assert!(
        repeated < once,
        "three observations must outrank one on equal confirmations: {found:#?}"
    );
}

/// The one signal the user said out loud is its own kind, and it is never *also*
/// reported as a habit: a habit is something the user does **without saying so**.
#[test]
fn an_explicit_preference_is_a_preference_and_never_a_habit() {
    let turns = vec![
        turn("s-1-1", 0, "always run the tests before committing", true),
        turn("s-1-1", 1, "always run the tests before committing", true),
        turn("s-1-1", 2, "always run the tests before committing", true),
    ];

    let found = extract(&turns);
    assert!(
        has(
            &found,
            CandidateKind::Preference,
            "always run the tests before committing"
        ),
        "a stated preference is a Preference: {found:#?}"
    );
    assert!(
        !found.iter().any(|c| c.kind == CandidateKind::Habit),
        "a rule the user stated is not a habit: {found:#?}"
    );
}

/// Every candidate can be traced to the turn it was last seen in, which is what makes
/// a learned fact one the user can judge (and revert).
#[test]
fn a_candidate_carries_the_session_and_turn_it_was_last_seen_in() {
    let turns = vec![
        turn("s-1-1", 0, "always review the diff before committing", true),
        turn("s-2-2", 4, "always review the diff before committing", true),
    ];

    let found = extract(&turns);
    let fact = found
        .iter()
        .find(|c| c.kind == CandidateKind::Preference)
        .unwrap_or_else(|| panic!("no preference in {found:#?}"));

    assert_eq!(
        fact.hits, 2,
        "the same fact from two sessions is one candidate"
    );
    assert_eq!(fact.provenance.session_id, "s-2-2");
    assert_eq!(fact.provenance.turn_index, 4);
    assert_eq!(fact.provenance.source, Source::PieSession);
    assert_eq!(
        fact.last_seen_ms,
        BASE_MS + 4,
        "the timestamp is the newest turn's own, never a clock read"
    );
}

/// A failed turn is still evidence the user asked — dropping it would lose the half of
/// the history that says what was wanted — but a fact drawn only from failures must
/// not outrank an identical fact drawn from a success.
#[test]
fn a_failed_turn_is_evidence_but_never_outranks_a_successful_one() {
    let turns = vec![
        turn("s-1-1", 0, "always run cargo fmt", false),
        turn("s-1-2", 0, "never touch the vendor store", true),
    ];

    let found = extract(&turns);
    let failed = position(&found, CandidateKind::Preference, "always run cargo fmt");
    let succeeded = position(
        &found,
        CandidateKind::Preference,
        "never touch the vendor store",
    );

    let hits_of = |text: &str| {
        found
            .iter()
            .find(|c| c.kind == CandidateKind::Preference && c.text == text)
            .unwrap_or_else(|| panic!("no preference {text:?}"))
            .hits
    };
    assert_eq!(
        hits_of("always run cargo fmt"),
        1,
        "the failure still contributed"
    );
    assert_eq!(hits_of("never touch the vendor store"), 1);
    assert!(
        succeeded < failed,
        "with the same recurrence, the success must lead: {found:#?}"
    );
}

/// An empty fact is not a fact: a blank prompt, or one that is only punctuation, must
/// leave no line in the memory document that `pie memory revert` cannot explain.
#[test]
fn empty_and_whitespace_prompts_produce_no_candidates() {
    assert!(extract(&[]).is_empty());
    assert!(extract(&[turn("s-1-1", 0, "", true)]).is_empty());
    assert!(extract(&[turn("s-1-1", 0, "   \n\t  \n ", true)]).is_empty());

    let punctuation = extract(&[turn("s-1-1", 0, "!!! ... ;;;", true)]);
    assert!(
        punctuation.iter().all(|c| !c.text.trim().is_empty()),
        "no candidate may carry an empty fact: {punctuation:#?}"
    );
}

// ---------------------------------------------------------------------------
// Stack, terminology, convention, habit
// ---------------------------------------------------------------------------

#[test]
fn stack_and_terminology_are_derived_from_the_prompt_text() {
    let turns = vec![turn(
        "s-1-1",
        0,
        "Rust and cargo please, and we call it \"the cut\"",
        true,
    )];

    let found = extract(&turns);
    assert!(has(&found, CandidateKind::Stack, "uses Rust"), "{found:#?}");
    assert!(
        has(&found, CandidateKind::Stack, "uses cargo"),
        "{found:#?}"
    );
    assert!(
        has(&found, CandidateKind::Terminology, "calls it \"the cut\""),
        "the user's own word for a thing is terminology: {found:#?}"
    );
}

/// The one contamination this stage can commit: reporting the vendor model's
/// vocabulary back to the user as their own. `RISKS.md` §16 is memory that rots, and
/// this is how it would start.
#[test]
fn the_models_answer_is_never_evidence_about_the_user() {
    let turns = vec![turn_with_answer(
        "s-1-1",
        0,
        "summarise the plan",
        "You should use Docker and note that we always prefer tabs.",
    )];

    let found = extract(&turns);
    assert!(
        !has(&found, CandidateKind::Stack, "uses Docker"),
        "the answer's technology is the model's, not the user's: {found:#?}"
    );
    assert!(
        !found
            .iter()
            .any(|c| c.kind == CandidateKind::Preference && c.text.contains("prefer tabs")),
        "the answer's preference is not the user's: {found:#?}"
    );
}

/// A directive the user repeats is a house rule; the same directive asked once is a
/// one-off and must not be written down as a convention.
#[test]
fn a_directive_the_user_repeats_becomes_a_convention() {
    let once = extract(&[turn("s-1-1", 0, "run the tests before committing", true)]);
    assert!(
        !has(
            &once,
            CandidateKind::Convention,
            "run the tests before committing"
        ),
        "one occurrence is not a convention: {once:#?}"
    );

    let twice = extract(&[
        turn("s-1-1", 0, "run the tests before committing", true),
        turn("s-1-2", 0, "run the tests before committing", true),
    ]);
    assert!(
        has(
            &twice,
            CandidateKind::Convention,
            "run the tests before committing"
        ),
        "a directive asked twice is a house rule: {twice:#?}"
    );
    assert!(
        !has(
            &twice,
            CandidateKind::Preference,
            "run the tests before committing"
        ),
        "the user never said `always` or `never`, so this is not a stated preference"
    );
}

/// A habit is behaviour, not a sentence: three different requests that open the same
/// way are one habit, and two are not enough to claim one.
#[test]
fn a_verb_the_user_opens_with_becomes_a_habit_only_after_enough_turns() {
    let two = extract(&[
        turn("s-1-1", 0, "audit the renderer", true),
        turn("s-1-1", 1, "audit the picker", true),
    ]);
    assert!(
        !has(&two, CandidateKind::Habit, "habitually asks to audit"),
        "two turns do not yet show a habit: {two:#?}"
    );

    let three = extract(&[
        turn("s-1-1", 0, "audit the renderer", true),
        turn("s-1-1", 1, "audit the picker", true),
        turn("s-1-1", 2, "audit the reducer", true),
    ]);
    assert!(
        has(&three, CandidateKind::Habit, "habitually asks to audit"),
        "three turns that open the same way are a habit: {three:#?}"
    );
    assert!(
        !has(&three, CandidateKind::Convention, "audit the renderer"),
        "each request was made once, so none is a repeated house rule"
    );
}

// ---------------------------------------------------------------------------
// Mining pie's own store
// ---------------------------------------------------------------------------

/// A fresh install has no store, and neither does a path that names a file. Both are
/// "nothing to learn", never a failure a scheduled job has to be rescued from.
#[test]
fn mining_a_store_that_does_not_exist_is_empty_and_not_an_error() {
    let scratch = tempfile::tempdir().unwrap();
    let missing = scratch.path().join("sessions");
    assert!(mine_sessions(&missing).unwrap().is_empty());

    let file = scratch.path().join("not-a-directory");
    std::fs::write(&file, "x").unwrap();
    assert!(mine_sessions(&file).unwrap().is_empty());
}

/// The tolerant-parse convention (I5) applied to the store: one torn line — what a
/// crashed writer leaves — must not cost the user every other turn in that session.
#[test]
fn mining_skips_a_row_it_cannot_parse_instead_of_failing() {
    let root = tempfile::tempdir().unwrap();
    let mut session = Session::start_in(root.path(), "agy", Path::new("/work"));
    session
        .record(&row(1_000, "always run cargo fmt", true))
        .unwrap();

    let transcript = session.transcript_path();
    let existing = std::fs::read_to_string(&transcript).unwrap();
    std::fs::write(&transcript, format!("{existing}not json at all\n")).unwrap();

    session
        .record(&row(2_000, "audit the renderer", true))
        .unwrap();

    let turns = mine_sessions(root.path()).unwrap();
    assert_eq!(turns.len(), 2, "the torn row is skipped: {turns:#?}");
    assert_eq!(turns[0].prompt, "always run cargo fmt");
    assert_eq!(turns[1].prompt, "audit the renderer");
    assert!(
        turns.iter().all(|t| !t.prompt.is_empty()),
        "`index.jsonl` is a summary log, not a transcript, and must not be mined: {turns:#?}"
    );
}

/// Mining names the session, the turn's position and the store's own timestamp — and
/// the timestamp is the store's, not the clock's: these rows were written long before
/// this test ran, and `extract` must report the stored value.
#[test]
fn mined_turns_keep_the_session_id_turn_index_and_stored_timestamp() {
    let root = tempfile::tempdir().unwrap();
    let mut session = Session::start_in(root.path(), "agy", Path::new("/work"));
    session
        .record(&row(1_000, "always run cargo fmt", true))
        .unwrap();
    session
        .record(&row(2_000, "always run cargo fmt", false))
        .unwrap();

    let turns = mine_sessions(root.path()).unwrap();
    assert_eq!(turns.len(), 2);
    assert_eq!(turns[0].session_id, session.id());
    assert_eq!(turns[0].turn_index, 0);
    assert_eq!(turns[1].turn_index, 1);
    assert_eq!(turns[0].source, Source::PieSession);
    assert_eq!(
        turns[1].ts, 2_000,
        "the store's own `ts`, not a clock reading"
    );

    let found = extract(&turns);
    let fact = found
        .iter()
        .find(|c| c.kind == CandidateKind::Preference)
        .unwrap_or_else(|| panic!("no preference in {found:#?}"));
    assert_eq!(fact.hits, 2);
    assert_eq!(
        fact.last_seen_ms, 2_000,
        "a learned fact's timestamp is the turn's, never `now` — `now` would make the \
         report differ on every run and decay impossible to test"
    );
    assert_eq!(fact.provenance.session_id, session.id());
    assert_eq!(fact.provenance.turn_index, 1);
}
