//! Learning about the user from their own history (`DESIGN.md` §16).
//!
//! **The default path spends nothing.** `RISKS.md` §13 is the governing risk — an
//! LLM-summarising job can silently spend premium subscription quota — and this
//! machine has no cheap backend: every backend pie drives is subscription-locked, so
//! "default to a cheap model" has nothing to name. The honest reading is stronger,
//! not weaker: [`extract`] makes **no model call at all**. It counts recurrences,
//! recognises explicit preference statements, and derives stack and terminology from
//! prompt text and tool names. That mirrors M5's compaction decision (`DESIGN.md`
//! §11: "extractive: head + state block + tail; spawns nothing").
//!
//! The second reason matters more: a rule-based extractor has no failure mode that can
//! spend money silently. A model-based one has exactly one, and it is the risk
//! `RISKS.md` §13 already names. A model may still consolidate — as an **opt-in
//! extra** behind [`crate::schedule::check_budget`].
//!
//! Two rules carried over from M6, because they are what made M6 testable:
//! **the clock is a parameter, never read**, and **nothing here spawns** except behind
//! the budget check.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::error::Result;
use crate::session;

/// What kind of thing was learned. Drives how it is rendered and how it decays.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateKind {
    /// An explicit preference the user stated (`always`, `never`, `I prefer`).
    Preference,
    /// A tool, language or framework the user actually uses.
    Stack,
    /// A convention or house rule the user asks for repeatedly.
    Convention,
    /// A word the user uses for a thing — feeds the domain glossary.
    Terminology,
    /// Something the user does habitually without saying so.
    Habit,
}

/// Where a turn came from, for provenance.
///
/// Vendor stores are **opt-in and read-only** (`RISKS.md` §15): the default path reads
/// pie's own sessions only, and nothing in M7 sends anything off-machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// pie's own session store under `$PIE_HOME/sessions`.
    PieSession,
    /// agy's own history (`~/.gemini/antigravity-cli`). Opt-in.
    AgyHistory,
    /// codex's own history (`~/.codex`). Opt-in.
    CodexHistory,
    /// claude's own store (`~/.claude`). Opt-in, and empty on this machine.
    ClaudeHistory,
}

/// Which session and turn a candidate was observed in.
///
/// A learned fact you cannot trace is one you cannot judge, so every candidate
/// carries this and `pie memory show` prints it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provenance {
    /// pie's session id.
    pub session_id: String,
    /// Index of the turn within that session.
    pub turn_index: usize,
    /// Which store it came from.
    pub source: Source,
}

/// One thing that might be worth remembering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// What kind of thing it is.
    pub kind: CandidateKind,
    /// The fact, as the user would read it.
    pub text: String,
    /// Where it was seen.
    pub provenance: Provenance,
    /// How many times it has been observed.
    pub hits: u32,
    /// When it was last observed, in epoch milliseconds. Passed in, never read.
    pub last_seen_ms: u64,
}

/// One turn, as `learn` sees it.
///
/// Deliberately not `session::TurnRow`: mining also reads vendor stores, whose rows
/// have a different shape, and normalising them once here is what stops every
/// extractor from growing a second branch per source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MinedTurn {
    /// The session it belongs to.
    pub session_id: String,
    /// Index within the session.
    pub turn_index: usize,
    /// Which store it came from.
    pub source: Source,
    /// What the user asked.
    pub prompt: String,
    /// What came back.
    pub answer: String,
    /// Whether the turn succeeded — a failed turn's text is still evidence, but a
    /// candidate drawn only from failures should not outrank one drawn from success.
    pub ok: bool,
    /// When the turn was recorded, in epoch milliseconds, taken from the store's own
    /// `ts`.
    ///
    /// Added by stage A, and the only field here that is not part of the shape the
    /// extractor reads. It exists because [`Candidate::last_seen_ms`] has to come
    /// *from the turn*: the alternative sources are a clock read inside [`extract`]
    /// (forbidden — it must be pure and diffable) or a constant (which would make
    /// every candidate look ancient to stage B's decay). Stage A reports this as an
    /// addition to a scaffold type rather than a silent change.
    pub ts: u64,
}

/// Read pie's own sessions into `MinedTurn`s. Quota-free; reads pie's files only.
///
/// Vendor stores are a separate, **opt-in** reader: a fresh install must never read a
/// vendor's private history just because it happens to exist on disk.
pub fn mine_sessions(sessions_root: &Path) -> Result<Vec<MinedTurn>> {
    // A missing store is not an error. A fresh install has never recorded a session,
    // and a background job that failed because there was nothing to learn from would
    // be a job the user has to babysit for no reason. A *file* where the store belongs
    // reads the same way: there is no store, so there is nothing to mine.
    if !sessions_root.is_dir() {
        return Ok(Vec::new());
    }

    let mut ids: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(sessions_root)? {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let stem = match path.file_stem().and_then(|s| s.to_str()) {
            Some(stem) => stem,
            // A file name that is not UTF-8 cannot be a pie session id, so it is
            // neither mined nor an error.
            None => continue,
        };
        // `index.jsonl` is the derived summary log (`session.rs`), not a transcript.
        // Every field of a summary frame is optional on the read path, so it would
        // parse as a turn with an empty prompt and land in the store twice.
        if stem == "index" {
            continue;
        }
        ids.push(stem.to_string());
    }
    // Sorted, so two runs over an unchanged store mine the same turns in the same
    // order whatever `read_dir` felt like returning.
    ids.sort_unstable();

    let mut turns: Vec<MinedTurn> = Vec::new();
    for id in ids {
        // pie's own reader, not a second parser: it validates the id, skips a row it
        // cannot parse (the tolerant-parse convention, I5) and warns about it. A
        // session that cannot be read at all is skipped the same way — a hand-edited
        // id or an I/O error in one file must not cost the user every other session.
        let rows = match session::load_from(sessions_root, &id) {
            Ok(rows) => rows,
            Err(error) => {
                tracing::warn!(
                    session = %id,
                    error = %error,
                    "skipping a session store entry that could not be read"
                );
                continue;
            }
        };
        for (turn_index, row) in rows.into_iter().enumerate() {
            turns.push(MinedTurn {
                session_id: id.clone(),
                turn_index,
                source: Source::PieSession,
                prompt: row.prompt,
                answer: row.response,
                ok: row.ok,
                ts: row.ts,
            });
        }
    }
    Ok(turns)
}

/// Extract candidates from turns. **Pure**: no spawn, no clock, no I/O.
///
/// Two calls over the same input must produce byte-identical output, so a report can
/// be diffed. That determinism is asserted, not assumed: a report you cannot diff is
/// one you cannot review.
pub fn extract(turns: &[MinedTurn]) -> Vec<Candidate> {
    // Only `prompt` is read, never `answer`: what the vendor's model wrote is not
    // evidence about the user, and learning its vocabulary as the user's own is the
    // drift `RISKS.md` §16 names. `answer` stays on the turn for a later opt-in model
    // consolidation, which is allowed a judgement this rule-based pass is not.

    // Keyed by (rank, text), so the map's own order is already stable; the sort below
    // decides ranking, never identity.
    let mut facts: BTreeMap<(u8, String), Acc> = BTreeMap::new();
    // Verbs are keyed by the verb, not by a sentence: a habit is behaviour that
    // recurs across prompts, not a line the user happened to type twice.
    let mut verbs: BTreeMap<String, Acc> = BTreeMap::new();
    let mut stated: BTreeSet<String> = BTreeSet::new();

    for turn in turns {
        // Dedupe inside the turn: a fact repeated in one prompt is one observation,
        // so `hits` counts *turns*. Without this, one long paste would outrank a habit
        // the user actually has.
        let mut claimed: BTreeMap<(u8, String), CandidateKind> = BTreeMap::new();
        let mut opened: BTreeSet<String> = BTreeSet::new();

        for sentence in sentences(&turn.prompt) {
            let chars: Vec<char> = sentence.chars().collect();
            let lower = lowered(&chars);
            let marked = is_preference(&lower);

            if marked {
                claimed.insert(
                    (rank(CandidateKind::Preference), sentence.clone()),
                    CandidateKind::Preference,
                );
            }

            if let Some(verb) = directive_verb(&lower) {
                if marked {
                    // The user *said* it, so it is a stated rule and must not also be
                    // reported as a habit — "something the user does habitually
                    // **without saying so**".
                    stated.insert(verb);
                } else {
                    opened.insert(verb);
                    // A directive the user repeats is a house rule rather than a
                    // one-off; the count is applied when emitting.
                    claimed.insert(
                        (rank(CandidateKind::Convention), sentence.clone()),
                        CandidateKind::Convention,
                    );
                }
            }

            for term in stack_terms(&lower) {
                claimed.insert(
                    (rank(CandidateKind::Stack), format!("uses {term}")),
                    CandidateKind::Stack,
                );
            }

            if let Some(term) = named_term(&lower, &chars) {
                claimed.insert(
                    (
                        rank(CandidateKind::Terminology),
                        format!("calls it \"{term}\""),
                    ),
                    CandidateKind::Terminology,
                );
            }
        }

        for ((_, text), kind) in claimed {
            see(&mut facts, kind, text, turn);
        }
        for verb in opened {
            see_verb(&mut verbs, &verb, turn);
        }
    }

    let mut ranked: Vec<(u32, Candidate)> = Vec::new();
    for acc in facts.into_values() {
        if acc.kind == CandidateKind::Convention && acc.hits < CONVENTION_MIN_TURNS {
            continue;
        }
        ranked.push((acc.confirmed, acc.into_candidate()));
    }
    for (verb, acc) in verbs {
        if acc.hits >= HABIT_MIN_TURNS && !stated.contains(&verb) {
            ranked.push((acc.confirmed, acc.into_candidate()));
        }
    }

    // A total order, because a report you cannot diff is one you cannot review.
    // Confirmations lead (a fact seen only in failures must not outrank one seen in a
    // success), then recurrence, then the kind's own confidence order, then the text —
    // and every pair of candidates differs in (kind, text), so the order is total and
    // independent of the order the caller listed the turns in.
    ranked.sort_by(|(a_confirmed, a), (b_confirmed, b)| {
        b_confirmed
            .cmp(a_confirmed)
            .then_with(|| b.hits.cmp(&a.hits))
            .then_with(|| rank(a.kind).cmp(&rank(b.kind)))
            .then_with(|| a.text.cmp(&b.text))
    });

    ranked.into_iter().map(|(_, candidate)| candidate).collect()
}

/// Turns a directive must recur in before it is a house rule rather than a one-off.
const CONVENTION_MIN_TURNS: u32 = 2;

/// Turns an opening verb must recur in before it is a habit.
const HABIT_MIN_TURNS: u32 = 3;

/// Longest fact kept. One pasted prompt must not put a wall of text into the memory
/// document, and a fact nobody can read is one nobody can revert with confidence.
const FACT_MAX_CHARS: usize = 240;

/// Longest `call it X` term accepted. A longer run of words is a sentence about a
/// thing, not the word for it.
const TERM_MAX_CHARS: usize = 64;

/// Phrases that state a preference. The user said them, which is why they are the
/// highest-confidence signal and their own kind.
const PREFERENCE_MARKERS: &[&str] = &[
    "always", "never", "prefer", "prefers", "don't", "dont", "do not",
];

/// Phrases that name a thing. `call it X` is the only construction recognised — a
/// guess at which noun is a term of art would invent facts.
const NAMING_PHRASES: &[&str] = &["call it", "calls it", "call this", "call that"];

/// Verbs that open a directive.
///
/// A table, not a parser. Its failure mode is *under*-detection — a directive phrased
/// with an unlisted verb produces no `Convention` and no `Habit`, which is the honest
/// failure for a rule-based extractor; inventing a rule out of prose is the one thing
/// this pass must not do.
const DIRECTIVE_VERBS: &[&str] = &[
    "run",
    "use",
    "add",
    "remove",
    "keep",
    "avoid",
    "write",
    "check",
    "ensure",
    "remember",
    "review",
    "test",
    "fix",
    "refactor",
    "audit",
    "update",
    "read",
    "create",
    "delete",
    "commit",
    "implement",
    "document",
    "verify",
    "measure",
    "benchmark",
    "try",
    "start",
    "stop",
    "move",
    "rename",
    "split",
    "merge",
    "extract",
    "apply",
    "attach",
    "install",
    "build",
    "compile",
    "format",
    "lint",
    "pin",
    "revert",
    "make",
    "prefer",
];

/// Words that lead a sentence without naming the action it asks for.
const LEAD_IN_WORDS: &[&str] = &[
    "please", "can", "could", "would", "should", "must", "i", "we", "you", "want", "need", "to",
    "let's", "lets", "just", "then", "also", "and", "so", "now", "first", "next", "always",
    "never", "don't", "dont", "do", "not",
];

/// The vocabulary a `Stack` candidate is drawn from, as `(token, display name)`.
///
/// Closed on purpose: a term the user uses and this table does not name is not
/// reported, while "any capitalised word" would report the user's prose back to them
/// as a technology. Extend the table when a real session shows a miss.
const STACK_TERMS: &[(&str, &str)] = &[
    ("rust", "Rust"),
    ("cargo", "cargo"),
    ("rustfmt", "rustfmt"),
    ("clippy", "clippy"),
    ("tokio", "tokio"),
    ("serde", "serde"),
    ("clap", "clap"),
    ("ratatui", "ratatui"),
    ("crossterm", "crossterm"),
    ("toml", "TOML"),
    ("json", "JSON"),
    ("jsonl", "JSONL"),
    ("yaml", "YAML"),
    ("markdown", "Markdown"),
    ("git", "git"),
    ("github", "GitHub"),
    ("bash", "bash"),
    ("fish", "fish"),
    ("zsh", "zsh"),
    ("powershell", "PowerShell"),
    ("python", "Python"),
    ("node", "Node.js"),
    ("npm", "npm"),
    ("pnpm", "pnpm"),
    ("typescript", "TypeScript"),
    ("javascript", "JavaScript"),
    ("react", "React"),
    ("docker", "Docker"),
    ("sqlite", "SQLite"),
    ("postgres", "Postgres"),
    ("nix", "Nix"),
    ("linux", "Linux"),
    ("windows", "Windows"),
    ("wsl", "WSL"),
    ("systemd", "systemd"),
    ("pacman", "pacman"),
    ("ripgrep", "ripgrep"),
    ("rg", "ripgrep"),
    ("regex", "regex"),
];

/// One fact's observations so far.
///
/// `confirmed` is kept beside `hits` because they answer different questions: how
/// often the user asked (`hits`) and how often it worked (`confirmed`). A failed turn
/// is still evidence the user asked — dropping it would lose the half of the history
/// that says what was *wanted* — but a fact seen only in failures must not outrank one
/// seen in a success.
#[derive(Debug)]
struct Acc {
    kind: CandidateKind,
    text: String,
    hits: u32,
    confirmed: u32,
    last_seen_ms: u64,
    provenance: Provenance,
}

impl Acc {
    fn new(kind: CandidateKind, text: String, turn: &MinedTurn) -> Self {
        Self {
            kind,
            text,
            hits: 1,
            confirmed: if turn.ok { 1 } else { 0 },
            last_seen_ms: turn.ts,
            provenance: provenance_of(turn),
        }
    }

    fn observe(&mut self, turn: &MinedTurn) {
        self.hits = self.hits.saturating_add(1);
        if turn.ok {
            self.confirmed = self.confirmed.saturating_add(1);
        }

        // The newest observation wins, ties broken by (session id, turn index), so the
        // chosen provenance does not depend on the order the caller listed the turns
        // in — the report has to be diffable.
        let newest = (turn.ts, turn.session_id.as_str(), turn.turn_index)
            >= (
                self.last_seen_ms,
                self.provenance.session_id.as_str(),
                self.provenance.turn_index,
            );
        if newest {
            self.last_seen_ms = turn.ts;
            self.provenance = provenance_of(turn);
        }
    }

    fn into_candidate(self) -> Candidate {
        Candidate {
            kind: self.kind,
            text: self.text,
            provenance: self.provenance,
            hits: self.hits,
            last_seen_ms: self.last_seen_ms,
        }
    }
}

fn provenance_of(turn: &MinedTurn) -> Provenance {
    Provenance {
        session_id: turn.session_id.clone(),
        turn_index: turn.turn_index,
        source: turn.source,
    }
}

/// Record one observation of `text` as `kind`.
fn see(
    facts: &mut BTreeMap<(u8, String), Acc>,
    kind: CandidateKind,
    text: String,
    turn: &MinedTurn,
) {
    // An empty fact is not a fact: a whitespace-only or punctuation-only prompt must
    // leave no blank line in the memory document that `pie memory revert` cannot
    // explain.
    if text.is_empty() {
        return;
    }
    let key = (rank(kind), text);
    if let Some(slot) = facts.get_mut(&key) {
        slot.observe(turn);
        return;
    }
    let text = key.1.clone();
    facts.insert(key, Acc::new(kind, text, turn));
}

/// Record one observation of a prompt-opening verb.
fn see_verb(verbs: &mut BTreeMap<String, Acc>, verb: &str, turn: &MinedTurn) {
    if let Some(slot) = verbs.get_mut(verb) {
        slot.observe(turn);
        return;
    }
    verbs.insert(
        verb.to_string(),
        Acc::new(
            CandidateKind::Habit,
            format!("habitually asks to {verb}"),
            turn,
        ),
    );
}

/// The confidence order of the kinds, used to break a tie on equal recurrence.
///
/// `CandidateKind` keeps no `Ord` — it is a frozen interface, and how a report orders
/// two equally-recurring facts is a presentation decision that belongs here. A stated
/// preference leads: it is the one signal the user said out loud.
fn rank(kind: CandidateKind) -> u8 {
    match kind {
        CandidateKind::Preference => 0,
        CandidateKind::Convention => 1,
        CandidateKind::Stack => 2,
        CandidateKind::Terminology => 3,
        CandidateKind::Habit => 4,
    }
}

/// Split a prompt into sentences, whitespace-collapsed and bounded.
///
/// A terminator ends a sentence only when whitespace or the line's end follows it, so
/// `cargo.toml` and `node.js` survive as one word instead of becoming a "sentence"
/// whose text is a fragment of a path.
fn sentences(prompt: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in prompt.lines() {
        let chars: Vec<char> = line.chars().collect();
        let mut start = 0usize;
        for (i, ch) in chars.iter().enumerate() {
            let ends_sentence = matches!(ch, '.' | '!' | '?' | ';')
                && match chars.get(i + 1) {
                    None => true,
                    Some(next) => next.is_whitespace(),
                };
            if ends_sentence {
                push_sentence(&mut out, &chars[start..i]);
                start = i + 1;
            }
        }
        push_sentence(&mut out, &chars[start..]);
    }
    out
}

fn push_sentence(out: &mut Vec<String>, chars: &[char]) {
    let text: String = chars.iter().collect();
    let text = collapse(&text);
    if !text.is_empty() {
        out.push(bounded(text));
    }
}

/// Whitespace-collapsed text, so two turns that differ only in line wrapping produce
/// the same fact and therefore the same merge key.
fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Cap a fact. Char-safe: the ellipsis follows a `char` count, never a byte count, so
/// multi-byte input can neither panic nor be split mid-character.
fn bounded(text: String) -> String {
    if text.chars().count() <= FACT_MAX_CHARS {
        return text;
    }
    let kept: String = text.chars().take(FACT_MAX_CHARS).collect();
    format!("{kept}…")
}

/// Per-char lowercase, one output char per input char.
///
/// `str::to_lowercase` can change byte length (`İ` → `i`+combining dot), so a byte
/// offset found in its output cannot safely index the original — and an index that is
/// not a char boundary panics, which I6 forbids on external input. This mapping is
/// index-preserving by construction.
fn lowered(chars: &[char]) -> Vec<char> {
    chars
        .iter()
        .map(|c| c.to_lowercase().next().unwrap_or(*c))
        .collect()
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '\'' | '-' | '_' | '+')
}

/// The position of `needle` in `hay` as a whole word, or `None`.
///
/// Whole-word, because `not` inside `nothing` and `always` inside `alwayson` are not
/// statements about the user; a substring search would report them as preferences.
fn find_word(hay: &[char], needle: &str) -> Option<usize> {
    let needle = lowered(&needle.chars().collect::<Vec<char>>());
    if needle.is_empty() {
        return None;
    }
    for (start, window) in hay.windows(needle.len()).enumerate() {
        if window != needle.as_slice() {
            continue;
        }
        let before_is_word = match start.checked_sub(1) {
            None => false,
            Some(i) => is_word_char(hay[i]),
        };
        let after_is_word = match hay.get(start + needle.len()) {
            None => false,
            Some(c) => is_word_char(*c),
        };
        if !before_is_word && !after_is_word {
            return Some(start);
        }
    }
    None
}

fn has_word(hay: &[char], needle: &str) -> bool {
    find_word(hay, needle).is_some()
}

/// Whether a sentence states a preference the user said out loud.
fn is_preference(sentence: &[char]) -> bool {
    if PREFERENCE_MARKERS.iter().any(|m| has_word(sentence, m)) {
        return true;
    }
    // `use X not Y` is a preference only with the contrast: `use` alone is not a
    // statement about the user's habits, but the pair always states a choice.
    (has_word(sentence, "use") || has_word(sentence, "using")) && has_word(sentence, "not")
}

/// The verb a sentence opens with, when that verb is a known directive.
fn directive_verb(lower: &[char]) -> Option<String> {
    for raw in lower.split(|c| c.is_whitespace()) {
        let word: String = raw
            .iter()
            .copied()
            .skip_while(|c| !c.is_alphanumeric())
            .take_while(|c| c.is_alphanumeric() || *c == '\'')
            .collect();
        if word.is_empty() || LEAD_IN_WORDS.contains(&word.as_str()) {
            continue;
        }
        return DIRECTIVE_VERBS.contains(&word.as_str()).then_some(word);
    }
    None
}

/// Stack terms referenced in the sentence, in vocabulary order.
fn stack_terms(lower: &[char]) -> Vec<&'static str> {
    let mut found: Vec<&'static str> = Vec::new();
    for (token, display) in STACK_TERMS {
        if has_word(lower, token) && !found.contains(display) {
            found.push(display);
        }
    }
    found
}

/// The word the user gives a thing, from a naming phrase (`call it X`).
///
/// `lower` and `original` must describe the same characters (see [`lowered`]); the
/// returned term keeps the user's own casing.
fn named_term(lower: &[char], original: &[char]) -> Option<String> {
    for phrase in NAMING_PHRASES {
        let Some(pos) = find_word(lower, phrase) else {
            continue;
        };
        let start = pos + phrase.chars().count();
        let raw: String = original
            .get(start..)
            .unwrap_or_default()
            .iter()
            .take_while(|c| !matches!(c, '.' | '!' | '?' | ';' | ','))
            .collect();
        let term = collapse(raw.trim_matches(|c: char| {
            c.is_whitespace() || matches!(c, '"' | '\'' | '`' | '(' | ')')
        }));
        if term.is_empty() || term.chars().count() > TERM_MAX_CHARS {
            continue;
        }
        return Some(term);
    }
    None
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    /// The scaffold's own smoke test, kept: the frozen types exist and the empty case
    /// holds.
    #[test]
    fn extracting_nothing_yields_nothing() {
        assert!(extract(&[]).is_empty());
    }

    /// Mining must not need a real store to be callable, and must not create one.
    #[test]
    fn mining_a_directory_that_does_not_exist_is_empty_not_an_error() {
        let missing = Path::new("/nonexistent-pie-sessions-for-a-unit-test");
        assert!(mine_sessions(missing).unwrap_or_default().is_empty());
    }

    /// The splitter's whole job is to keep a file name out of `sentences`: a `.`
    /// followed by a non-space is a character, not the end of a fact.
    #[test]
    fn a_dot_only_ends_a_sentence_when_whitespace_follows_it() {
        assert_eq!(
            sentences("fix cargo.toml then stop. thanks"),
            vec!["fix cargo.toml then stop", "thanks"]
        );
        assert!(sentences("   \n\t\n").is_empty());
    }

    /// Markers are matched as whole words: `preferred` is not `prefer`, and `nothing`
    /// is not `not`. Without this, ordinary prose reports itself as a preference.
    #[test]
    fn a_marker_inside_a_longer_word_is_not_a_preference() {
        let cases: &[(&str, bool)] = &[
            ("I preferred the old layout", false),
            ("there is nothing to do", false),
            ("I prefer tabs", true),
            ("do not touch the vendor store", true),
            ("use tabs not spaces", true),
        ];
        for (text, expected) in cases {
            let chars: Vec<char> = text.chars().collect();
            assert_eq!(is_preference(&lowered(&chars)), *expected, "{text}");
        }
    }

    /// The index-preserving lowercase is what makes `named_term` safe on non-ASCII
    /// input: `to_lowercase` can change byte length, and an index found in it would
    /// then panic when used on the original (I6).
    #[test]
    fn a_non_ascii_prompt_is_lowercased_without_shifting_indexes() {
        let chars: Vec<char> = "İSTANBUL we call it \"the cut\"".chars().collect();
        let lower = lowered(&chars);
        assert_eq!(lower.len(), chars.len());
        assert_eq!(named_term(&lower, &chars).as_deref(), Some("the cut"));
    }
}
