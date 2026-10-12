//! Consolidating what was learned into the files the persona is built from
//! (`DESIGN.md` §16).
//!
//! `learn` extracts candidates; this module is where they meet the memory document
//! that already exists, and where the result can be **undone**. Three properties are
//! the whole point, and each is a test rather than a hope:
//!
//! * **Revertible.** [`apply`] writes a journal *before* it touches the document and
//!   hands it back, and [`revert`] restores the pre-apply bytes exactly. A crash
//!   mid-apply leaves the journal on disk, so the document is recoverable rather than
//!   half-written.
//! * **Decaying, never deleting.** A candidate not re-observed within the window loses
//!   confidence and is **demoted**, not removed (`RISKS.md` §16: stale memory that
//!   rots invisibly). Re-reading the document still shows it.
//! * **Prefix-stable.** Consolidation **appends** to a later section and never reorders
//!   the head of the projection, because rewriting the head invalidates the vendor's
//!   cached prefix once a day (`TOKEN-ECONOMICS.md` §3).
//!
//! The clock is a parameter (`now_ms`), never read: a decay rule that reads the system
//! clock can only be tested today.
//!
//! # Where the learned material lives
//!
//! `$PIE_HOME/memory/learned.md`, with the undo handle beside it in
//! `$PIE_HOME/memory/learned.journal`. Three reasons, in order of weight:
//!
//! * `profile.rs` reads `memory/*.md` — non-recursively — and joins it **after** the
//!   persona and `AGENTS.md`, sorted by file name. A learned fact therefore cannot move
//!   the head of the projection, however many runs write it, which is exactly
//!   `TOKEN-ECONOMICS.md` §3's rule and `PLAN.md` §11b's model. Material outside
//!   `memory/` would not reach the projection at all; material inside `profile.toml` or
//!   `AGENTS.md` could only be written by *rewriting* those files on every run, which is
//!   the one thing consolidation must not do.
//! * The journal is `.journal`, not `.md`, so `profile.rs`'s extension filter skips it.
//!   An undo handle must never become part of the persona it undoes — and a journal that
//!   *is* a `memory/*.md` contribution would put a JSON blob in the prompt.
//! * `learned.md` sorts after the numeric names a user is likely to give their own notes
//!   (`01-notes.md` < `learned.md`), so an append lands at the end of the projection
//!   instead of in the middle of it. Where a user's own file sorts later, the append
//!   still cannot move the head — it only shifts bytes after the learned section.
//!
//! # What [`MemoryDoc::head`] is
//!
//! The learned document's own preamble. [`consolidate`] reads it and [`apply`] writes it
//! back byte for byte; neither re-orders nor re-renders it, and that is what makes the
//! projection's leading bytes stable across a run. `DESIGN.md` §16 describes it as "the
//! head of the projection" — it is the part of the projection this stage must not
//! disturb. `apply` is handed a `home`, not a projection, because it may not write
//! `profile.toml` or `AGENTS.md` at all; the head it can preserve is the document's.
//!
//! # The line format
//!
//! One line per fact, so a human can read the file and so `apply` can find the line a
//! decision refers to:
//!
//! ```text
//! - [preference] always run the offline gates — hits 4; last 2026-10-12; pie-session s7 turn 3; confidence full
//! ```
//!
//! The line is the *only* thing `apply` receives per decision (`Consolidation`'s vectors
//! are `Vec<String>` and `apply` is handed no document), so `reinforced` and `demoted`
//! carry rendered lines, not bare candidate texts: a bare text could not say what the
//! new hit count or the restored confidence is, and a decision that cannot be written is
//! a decision that was not really made.
//!
//! Dates are `YYYY-MM-DD`, in UTC, computed with integer arithmetic rather than a
//! locale-dependent formatter: the document records the day a fact was last seen, and a
//! 90-day window does not turn on a millisecond. The projection is normalised line ending
//! by line ending by `profile.rs`, so the stored date is the same bytes on every machine
//! (`TOKEN-ECONOMICS.md` §4).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::learn::{Candidate, CandidateKind, Provenance, Source};
use crate::paths;

/// How long a candidate stays fully confident without being re-observed.
pub const DECAY_WINDOW_MS: u64 = 90 * 24 * 60 * 60 * 1_000;

/// The learned document, under `$PIE_HOME/memory/`.
pub const LEARNED_FILE: &str = "learned.md";

/// The undo handle, under `$PIE_HOME/memory/`.
pub const JOURNAL_FILE: &str = "learned.journal";

/// The scratch name the document is written through before it is renamed into place.
pub const TEMP_FILE: &str = "learned.md.tmp";

/// The longest fact pie will write.
///
/// Not a token budget — a bound on how much of the projection one candidate can take.
/// A "fact" longer than this is a paragraph, and a paragraph belongs in `AGENTS.md`,
/// where the user put it deliberately.
pub const MAX_ENTRY_CHARS: usize = 240;

/// The preamble of a document that does not exist yet.
///
/// Kept to two lines because every byte here is paid on every turn and sits inside the
/// stable prefix (`TOKEN-ECONOMICS.md` §4).
pub const DEFAULT_HEAD: &str = "# Learned\n\nFacts pie observed in your own sessions, each naming its session and turn. Appended, never reordered.\n\n";

/// Milliseconds in a day, for the date the document stores.
const MS_PER_DAY: u64 = 24 * 60 * 60 * 1_000;

/// The prefix that marks a line as an entry rather than prose.
const ENTRY_PREFIX: &str = "- [";

/// Separator between an entry's text and its attributes.
const ATTRS_START: &str = " — ";

/// The memory document as it exists on disk.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MemoryDoc {
    /// The head of the projection: static persona text, never reordered.
    pub head: String,
    /// The learned section, which consolidation appends to.
    pub learned: String,
}

/// What consolidation decided to do, before anything is written.
///
/// Every vector holds **rendered lines** (see the module docs): `added` lines are
/// appended, `reinforced` and `demoted` lines replace the line with the same fact, and
/// `skipped` entries are `"<fact> — <reason>"`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Consolidation {
    /// Lines to append to the learned section, already formatted with provenance.
    pub added: Vec<String>,
    /// Candidates already present, whose confidence was raised.
    pub reinforced: Vec<String>,
    /// Candidates present but no longer re-observed, demoted rather than removed.
    pub demoted: Vec<String>,
    /// Candidates deliberately not written, with the reason.
    pub skipped: Vec<String>,
}

/// The handle that makes an apply undoable.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Journal {
    /// The document's exact bytes before the apply.
    pub previous: String,
    /// What the apply added, for the report.
    pub added: Vec<String>,
}

/// One fact as the document holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    kind: CandidateKind,
    text: String,
    hits: u32,
    /// Truncated to the day the document records.
    last_seen_ms: u64,
    provenance: Provenance,
    demoted: bool,
}

/// What `learned.journal` holds. Deliberately not `Journal` itself: the frozen type
/// carries no serialisation derives, and the on-disk shape is this module's business.
#[derive(Debug, Serialize, Deserialize)]
struct JournalFile {
    previous: String,
    added: Vec<String>,
}

/// `$PIE_HOME/memory` for a given home.
pub fn memory_dir(home: &Path) -> PathBuf {
    home.join(paths::MEMORY_DIR)
}

/// `$PIE_HOME/memory/learned.md` for a given home.
pub fn doc_path(home: &Path) -> PathBuf {
    memory_dir(home).join(LEARNED_FILE)
}

/// `$PIE_HOME/memory/learned.journal` for a given home.
pub fn journal_path(home: &Path) -> PathBuf {
    memory_dir(home).join(JOURNAL_FILE)
}

/// Read the learned document. A missing file is an empty document, not an error: a
/// fresh install has nothing learned yet, which is the normal first run.
pub fn read_doc(home: &Path) -> Result<MemoryDoc> {
    Ok(parse_doc(&read_document(&doc_path(home))?))
}

/// Split a document's text into the part consolidation must not touch and the part it
/// appends to. The split is lossless: `render_doc(parse_doc(text)) == text` for a
/// document whose entries start on their own line.
pub fn parse_doc(text: &str) -> MemoryDoc {
    let at = first_entry_offset(text);
    MemoryDoc {
        head: text[..at].to_string(),
        learned: text[at..].to_string(),
    }
}

/// Re-join a document. A learned section on a head that does not end in a newline gets
/// one, or the first appended entry would be glued onto the preamble's last line and
/// stop parsing as an entry.
pub fn render_doc(doc: &MemoryDoc) -> String {
    if doc.head.is_empty() {
        return doc.learned.clone();
    }
    if doc.learned.is_empty() {
        return doc.head.clone();
    }
    if doc.head.ends_with('\n') {
        format!("{}{}", doc.head, doc.learned)
    } else {
        format!("{}\n{}", doc.head, doc.learned)
    }
}

/// The journal on disk, if an apply left one behind. `pie memory revert` reads this.
pub fn read_journal(home: &Path) -> Result<Option<Journal>> {
    let path = journal_path(home);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let file: JournalFile = serde_json::from_slice(&bytes)?;
    Ok(Some(Journal {
        previous: file.previous,
        added: file.added,
    }))
}

/// Merge candidates into the existing document. Pure; nothing is written.
///
/// The decisions, in the order they are taken:
///
/// * a fact the document already holds is **reinforced** — its evidence is merged into
///   the existing line rather than a second line added;
/// * a fact the document does not hold is **added**, unless it is skipped for a reason
///   (`skipped` is never empty-and-silent: every unwritten candidate names why);
/// * an entry the batch did not re-observe, and which has not been seen inside
///   [`DECAY_WINDOW_MS`], is **demoted** — still present, lower confidence, never
///   removed (`RISKS.md` §16).
///
/// A candidate reinforces an entry whatever kind it claims: the fact is one fact, and the
/// kind pie wrote first is the one it keeps, so a phrase that later looks like a
/// convention cannot fork into two lines.
///
/// `hits` is the number of times a fact was observed *in the source material*, taken as
/// the maximum seen rather than summed. The same sessions are re-mined every day, so a
/// sum would inflate forever and a stale fact would be kept alive by its own history.
pub fn consolidate(existing: &MemoryDoc, candidates: &[Candidate], now_ms: u64) -> Consolidation {
    let mut decision = Consolidation::default();

    // What the document holds, in document order. Order is kept because the projection's
    // head is precisely what must not be reordered.
    let mut present: Vec<Entry> = Vec::new();
    for line in existing.learned.split_inclusive('\n') {
        if let Some(entry) = parse_entry(line.trim_end_matches('\n')) {
            // A line the user duplicated by hand is one fact, not two: the duplicate is
            // left where it is rather than deleted, because consolidation never deletes.
            if !present
                .iter()
                .any(|held| identity(&held.text) == identity(&entry.text))
            {
                present.push(entry);
            }
        }
    }

    let mut matched: Vec<String> = Vec::new();
    let mut decided: Vec<String> = Vec::new();

    for candidate in candidates {
        let text = sanitize(&candidate.text);

        if text.is_empty() {
            decision.skipped.push(
                "(empty text) — there is nothing to write once whitespace is collapsed".to_string(),
            );
            continue;
        }
        if text.chars().count() > MAX_ENTRY_CHARS {
            decision.skipped.push(format!(
                "{text} — {} chars is past the {MAX_ENTRY_CHARS}-char entry budget, and one fact \
                 may not take over the projection",
                text.chars().count()
            ));
            continue;
        }
        let needed = minimum_hits(candidate.kind);
        if candidate.hits < needed {
            decision.skipped.push(format!(
                "{text} — seen {}×; a {} needs {needed} observations before pie writes it down",
                candidate.hits,
                kind_token(candidate.kind)
            ));
            continue;
        }

        let key = identity(&text);
        if decided.contains(&key) {
            decision.skipped.push(format!(
                "{text} — the same fact appears more than once in this batch; it is written once"
            ));
            continue;
        }
        match present.iter_mut().find(|held| identity(&held.text) == key) {
            Some(held) => {
                held.hits = held.hits.max(candidate.hits);
                // The trace follows the newest evidence, and a tie keeps the trace the
                // document already has, so re-mining the same sessions reproduces the
                // same line instead of churning the file.
                if candidate.last_seen_ms > held.last_seen_ms {
                    held.last_seen_ms = candidate.last_seen_ms;
                    held.provenance = candidate.provenance.clone();
                }
                // A demoted fact that has been observed again is fully confident again.
                held.demoted = false;
                decision.reinforced.push(render_entry(held));
                matched.push(key.clone());
            }
            None => {
                // Checked only for a fact the document does not already hold as an entry:
                // an entry is the stronger statement, and a rule that shadowed it would
                // make a fact already learned impossible to reinforce.
                if head_states(&existing.head, &text) {
                    decision.skipped.push(format!(
                        "{text} — already stated in the document's own head, so writing it \
                         again would only lengthen the prefix"
                    ));
                    continue;
                }
                let entry = entry_from(candidate, &text);
                decision.added.push(render_entry(&entry));
            }
        }
        decided.push(key);
    }

    for held in &present {
        if matched.contains(&identity(&held.text)) {
            continue;
        }
        if now_ms.saturating_sub(held.last_seen_ms) > DECAY_WINDOW_MS {
            let mut demoted = held.clone();
            demoted.demoted = true;
            decision.demoted.push(render_entry(&demoted));
        }
    }

    decision
}

/// Write the consolidation, returning the journal needed to undo it.
///
/// The journal is written **first**, so a failure between the two writes leaves
/// something `revert` can use.
///
/// Three things this refuses to do:
///
/// * it never writes `profile.toml`, `AGENTS.md`, or the head of the document it is
///   handed — the head is read back and re-emitted byte for byte, and only a document
///   that does not exist yet gets [`DEFAULT_HEAD`];
/// * it never reports success for a write that failed: a failed write is an error, so a
///   half-learned persona cannot look like a good run;
/// * it never writes anything at all when nothing changed, so a second identical run
///   cannot double the document.
///
/// The document is read at the start, so the bytes in the journal — and therefore what
/// [`revert`] restores — are what `apply` actually saw, even if something else wrote the
/// file in between.
///
/// A `reinforced` or `demoted` line whose fact is no longer in the document is dropped
/// rather than appended. If the user deleted that fact between the read and the write,
/// re-adding it would silently undo their edit.
pub fn apply(consolidation: &Consolidation, home: &Path) -> Result<Journal> {
    let path = doc_path(home);
    let previous = read_document(&path)?;
    let doc = parse_doc(&previous);

    let mut learned = String::with_capacity(doc.learned.len() + 256);
    let mut held: Vec<String> = Vec::new();

    for line in doc.learned.split_inclusive('\n') {
        let body = line.trim_end_matches('\n');
        let terminator = &line[body.len()..];
        match parse_entry(body) {
            Some(entry) => {
                let key = identity(&entry.text);
                match replacement_for(consolidation, &key) {
                    Some(replacement) => learned.push_str(replacement),
                    None => learned.push_str(body),
                }
                held.push(key);
            }
            // Prose, a comment, a blank line: kept exactly as it is.
            None => learned.push_str(body),
        }
        learned.push_str(terminator);
    }

    let mut appended: Vec<String> = Vec::new();
    for line in &consolidation.added {
        // A line already in the document — a fact the previous apply appended — is not
        // appended again: that is what makes applying the same consolidation twice a
        // no-op rather than a doubled document.
        if let Some(key) = line_identity(line) {
            if held.contains(&key) {
                continue;
            }
            held.push(key);
        }
        if !learned.is_empty() && !learned.ends_with('\n') {
            learned.push('\n');
        }
        learned.push_str(line);
        learned.push('\n');
        appended.push(line.clone());
    }

    let next = render_doc(&MemoryDoc {
        head: head_for(&doc),
        learned,
    });

    // Compared before anything is written, so an empty or already-applied consolidation
    // leaves no journal behind for `revert` to act on.
    if next == previous {
        return Ok(Journal {
            previous,
            added: Vec::new(),
        });
    }

    let journal = Journal {
        previous,
        added: appended,
    };
    write_journal(home, &journal)?;
    write_atomically(&path, &next)?;
    Ok(journal)
}

/// Restore the document to exactly the bytes the journal recorded.
///
/// An empty `previous` is read as "there was no document": an empty file and an absent
/// one contribute the same zero bytes to the projection (`profile.rs` drops a source
/// that is blank), so restoring the indistinguishable one is safe — and `Journal` cannot
/// record the difference, which is a limitation of the frozen type rather than a choice
/// made here.
///
/// The journal is consumed on success. A journal that outlived its undo would make
/// `pie memory revert` offer an undo that has already been performed.
pub fn revert(journal: &Journal, home: &Path) -> Result<()> {
    let path = doc_path(home);
    if journal.previous.is_empty() {
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    } else {
        write_atomically(&path, &journal.previous)?;
    }

    match std::fs::remove_file(journal_path(home)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// The head to write: the document's own, or the default when there is no document yet.
///
/// The `is_empty` arm is the whole prefix-stability rule in one line. A document that
/// exists keeps its preamble byte for byte, because re-rendering it would move bytes the
/// vendor has already cached (`TOKEN-ECONOMICS.md` §3).
fn head_for(doc: &MemoryDoc) -> String {
    if doc.head.is_empty() {
        DEFAULT_HEAD.to_string()
    } else {
        doc.head.clone()
    }
}

/// The line a decision wants in place of the one the document holds, if any.
fn replacement_for<'a>(consolidation: &'a Consolidation, key: &str) -> Option<&'a str> {
    consolidation
        .reinforced
        .iter()
        .chain(consolidation.demoted.iter())
        .find(|line| line_identity(line).as_deref() == Some(key))
        .map(String::as_str)
}

/// Whether the document's head already states this fact.
fn head_states(head: &str, text: &str) -> bool {
    let needle = identity(text);
    !needle.is_empty()
        && head
            .split_inclusive('\n')
            .any(|line| identity(line).contains(&needle))
}

/// How many observations a kind needs before pie writes it down.
///
/// A preference or a tool name is a fact about one moment; a habit or a convention is a
/// claim about a pattern, and a pattern seen once is not a pattern.
fn minimum_hits(kind: CandidateKind) -> u32 {
    match kind {
        CandidateKind::Preference | CandidateKind::Stack | CandidateKind::Terminology => 1,
        CandidateKind::Convention | CandidateKind::Habit => 2,
    }
}

fn entry_from(candidate: &Candidate, text: &str) -> Entry {
    Entry {
        kind: candidate.kind,
        text: text.to_string(),
        hits: candidate.hits,
        last_seen_ms: candidate.last_seen_ms,
        provenance: candidate.provenance.clone(),
        demoted: false,
    }
}

/// The form a fact is compared by: whitespace collapsed, case folded, and a trailing
/// full stop ignored, so "Always run the gates." and "always run the gates" are one
/// fact rather than two lines that disagree.
fn identity(text: &str) -> String {
    sanitize(text).trim_end_matches('.').to_lowercase()
}

/// Collapse a candidate's text into something that can live on one line.
///
/// The em dash is replaced because it is the entry's own field separator: without this,
/// a candidate could contain `" — hits "` and a line pie wrote would parse as two facts,
/// which is a document that corrupts itself from the inside.
fn sanitize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut space = false;
    for ch in text.chars() {
        if ch.is_whitespace() || ch.is_control() {
            space = !out.is_empty();
            continue;
        }
        if space {
            out.push(' ');
            space = false;
        }
        out.push(if ch == '—' { '-' } else { ch });
    }
    out
}

/// A session id, reduced to characters that cannot break the line it sits on.
fn sanitize_ident(id: &str) -> String {
    id.chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .collect()
}

fn render_entry(entry: &Entry) -> String {
    let mut line = String::with_capacity(160);
    line.push_str(ENTRY_PREFIX);
    line.push_str(kind_token(entry.kind));
    line.push_str("] ");
    line.push_str(entry.text.as_str());
    line.push_str(ATTRS_START);
    line.push_str("hits ");
    line.push_str(&entry.hits.to_string());
    line.push_str("; last ");
    line.push_str(&date_of(entry.last_seen_ms));
    line.push_str("; ");
    line.push_str(source_token(entry.provenance.source));
    line.push(' ');
    line.push_str(&sanitize_ident(&entry.provenance.session_id));
    line.push_str(" turn ");
    line.push_str(&entry.provenance.turn_index.to_string());
    line.push_str("; confidence ");
    line.push_str(if entry.demoted { "demoted" } else { "full" });
    line
}

/// Parse one line, or `None` when it is prose.
///
/// Tolerant by design: a line pie cannot fully account for is left exactly as it is,
/// because the alternative is treating a hand-edited note as a fact and rewriting it.
fn parse_entry(line: &str) -> Option<Entry> {
    let rest = line.strip_prefix(ENTRY_PREFIX)?;
    let (kind, rest) = rest.split_once("] ")?;
    let kind = kind_from_token(kind)?;
    let (text, attrs) = rest.split_once(ATTRS_START)?;
    if text.is_empty() {
        return None;
    }

    let mut hits: Option<u32> = None;
    let mut last_seen_ms: Option<u64> = None;
    let mut provenance: Option<Provenance> = None;
    let mut demoted: Option<bool> = None;

    for field in attrs.split("; ") {
        if let Some(value) = field.strip_prefix("hits ") {
            hits = value.parse().ok();
        } else if let Some(value) = field.strip_prefix("last ") {
            last_seen_ms = parse_date(value);
        } else if let Some(value) = field.strip_prefix("confidence ") {
            demoted = match value {
                "full" => Some(false),
                "demoted" => Some(true),
                _ => None,
            };
        } else {
            let mut parts = field.split(' ');
            let source = source_from_token(parts.next()?)?;
            let session_id = parts.next()?.to_string();
            if parts.next()? != "turn" {
                return None;
            }
            let turn_index = parts.next()?.parse().ok()?;
            if parts.next().is_some() {
                return None;
            }
            provenance = Some(Provenance {
                session_id,
                turn_index,
                source,
            });
        }
    }

    Some(Entry {
        kind,
        text: text.to_string(),
        hits: hits?,
        last_seen_ms: last_seen_ms?,
        provenance: provenance?,
        demoted: demoted?,
    })
}

/// The identity of a rendered line, or `None` when the line is not an entry.
fn line_identity(line: &str) -> Option<String> {
    parse_entry(line).map(|entry| identity(&entry.text))
}

fn kind_token(kind: CandidateKind) -> &'static str {
    match kind {
        CandidateKind::Preference => "preference",
        CandidateKind::Stack => "stack",
        CandidateKind::Convention => "convention",
        CandidateKind::Terminology => "terminology",
        CandidateKind::Habit => "habit",
    }
}

fn kind_from_token(token: &str) -> Option<CandidateKind> {
    match token {
        "preference" => Some(CandidateKind::Preference),
        "stack" => Some(CandidateKind::Stack),
        "convention" => Some(CandidateKind::Convention),
        "terminology" => Some(CandidateKind::Terminology),
        "habit" => Some(CandidateKind::Habit),
        _ => None,
    }
}

fn source_token(source: Source) -> &'static str {
    match source {
        Source::PieSession => "pie-session",
        Source::AgyHistory => "agy-history",
        Source::CodexHistory => "codex-history",
        Source::ClaudeHistory => "claude-history",
    }
}

fn source_from_token(token: &str) -> Option<Source> {
    match token {
        "pie-session" => Some(Source::PieSession),
        "agy-history" => Some(Source::AgyHistory),
        "codex-history" => Some(Source::CodexHistory),
        "claude-history" => Some(Source::ClaudeHistory),
        _ => None,
    }
}

/// `YYYY-MM-DD` in UTC, with no formatter and no timezone database.
///
/// Integer arithmetic rather than a date library: a locale-dependent rendering would
/// make the document — and therefore the projection's bytes — differ between machines
/// (`TOKEN-ECONOMICS.md` §4). Civil-from-days is Howard Hinnant's algorithm.
fn date_of(ms: u64) -> String {
    let (year, month, day) = civil_from_days((ms / MS_PER_DAY) as i64);
    format!("{year:04}-{month:02}-{day:02}")
}

/// The inverse, as midnight UTC. A document records a day, so this is what a re-read
/// entry's `last_seen_ms` becomes.
fn parse_date(value: &str) -> Option<u64> {
    let mut parts = value.split('-');
    let year: i64 = parts.next()?.parse().ok()?;
    let month: u32 = parts.next()?.parse().ok()?;
    let day: u32 = parts.next()?.parse().ok()?;
    if parts.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let days = days_from_civil(year, month, day);
    if days < 0 {
        return None;
    }
    Some(days as u64 * MS_PER_DAY)
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month: u32 = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = if month <= 2 { year + 1 } else { year };
    (year, month, day)
}

fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = (year - era * 400) as u64;
    let mp: u64 = if month > 2 { month - 3 } else { month + 9 } as u64;
    let doy = (153 * mp + 2) / 5 + day as u64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe as i64 - 719_468
}

/// Where the learned section starts: the first line that is an entry.
fn first_entry_offset(text: &str) -> usize {
    let mut at = 0;
    for line in text.split_inclusive('\n') {
        if line.starts_with(ENTRY_PREFIX) {
            return at;
        }
        at += line.len();
    }
    text.len()
}

/// Read a document that is allowed not to exist.
fn read_document(path: &Path) -> Result<String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e.into()),
    }
}

/// Write a file by writing a scratch file and renaming it into place.
///
/// The rename is the point: a plain overwrite that dies half way leaves a *readable but
/// wrong* document — a persona that is truncated to a line boundary and reported as a
/// successful run. The scratch name ([`TEMP_FILE`]) does not end in `.md`, so a scratch
/// file left by a crash is invisible to the projection instead of being read as memory.
fn write_atomically(path: &Path, text: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let scratch = path.with_file_name(TEMP_FILE);
    if let Err(e) = std::fs::write(&scratch, text) {
        let _ = std::fs::remove_file(&scratch);
        return Err(e.into());
    }
    if let Err(e) = std::fs::rename(&scratch, path) {
        let _ = std::fs::remove_file(&scratch);
        return Err(e.into());
    }
    Ok(())
}

fn write_journal(home: &Path, journal: &Journal) -> Result<()> {
    let dir = memory_dir(home);
    std::fs::create_dir_all(&dir)?;
    let file = JournalFile {
        previous: journal.previous.clone(),
        added: journal.added.clone(),
    };
    let bytes = serde_json::to_vec(&file)?;
    // Same scratch-and-rename as the document: a truncated journal is an undo that
    // silently restores the wrong bytes.
    let path = journal_path(home);
    let scratch = dir.join(format!("{JOURNAL_FILE}.tmp"));
    if let Err(e) = std::fs::write(&scratch, bytes) {
        let _ = std::fs::remove_file(&scratch);
        return Err(e.into());
    }
    if let Err(e) = std::fs::rename(&scratch, &path) {
        let _ = std::fs::remove_file(&scratch);
        return Err(e.into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    /// The calendar has to be right for a decay window measured in days to mean
    /// anything, and it has to be UTC-only: a local-time date would differ per machine
    /// and take the projection's bytes with it.
    #[test]
    fn dates_are_utc_and_round_trip_through_the_document() {
        assert_eq!(date_of(0), "1970-01-01");
        assert_eq!(date_of(MS_PER_DAY - 1), "1970-01-01");
        assert_eq!(date_of(MS_PER_DAY), "1970-01-02");
        assert_eq!(date_of(1_700_000_000_000), "2023-11-14");
        assert_eq!(date_of(1_699_920_000_000), "2023-11-14");
        assert_eq!(date_of(2_000_000_000_000), "2033-05-18");

        for ms in [
            0,
            1_000,
            1_699_920_000_000,
            2_000_000_000_000,
            4_102_444_800_000,
        ] {
            let day = (ms / MS_PER_DAY) * MS_PER_DAY;
            assert_eq!(parse_date(&date_of(ms)), Some(day), "round trip at {ms}");
        }
        assert_eq!(parse_date("2023-13-01"), None);
        assert_eq!(parse_date("2023-11"), None);
        assert_eq!(parse_date("nonsense"), None);
    }

    /// A line pie writes must parse back into what it meant, or decay has no memory to
    /// measure against and a re-observation becomes a duplicate line.
    #[test]
    fn an_entry_line_round_trips_through_the_text_it_writes() {
        let entry = Entry {
            kind: CandidateKind::Preference,
            text: "always run the offline gates".to_string(),
            hits: 4,
            last_seen_ms: 1_699_920_000_000,
            provenance: Provenance {
                session_id: "s7".to_string(),
                turn_index: 3,
                source: Source::PieSession,
            },
            demoted: false,
        };
        let line = render_entry(&entry);
        assert_eq!(parse_entry(&line), Some(entry));
        assert!(line.contains("last 2023-11-14"));
    }

    /// The em dash is the entry's own field separator, so a candidate may not carry one
    /// in: a text that could split its own line is a document that corrupts itself.
    #[test]
    fn a_candidate_cannot_smuggle_a_field_separator_into_its_own_line() {
        assert_eq!(sanitize("a\n b\tc"), "a b c");
        assert_eq!(sanitize("x — hits 9"), "x - hits 9");
        assert!(parse_entry(
            "- [stack] x - hits 9 — hits 2; last 2023-11-14; pie-session s turn 0; confidence full"
        )
        .is_some());
    }

    #[test]
    fn a_line_that_is_not_an_entry_is_prose_not_a_guessed_fact() {
        assert_eq!(parse_entry("# my notes"), None);
        assert_eq!(
            parse_entry(
                "- [nonsense] x — hits 1; last 2023-11-14; pie-session s turn 0; confidence full"
            ),
            None
        );
        assert_eq!(parse_entry("- [stack] x — hits 1; last 2023-11-14"), None);
    }

    #[test]
    fn parsing_a_document_is_lossless_so_a_head_can_be_rewritten_byte_for_byte() {
        let text = "# Learned\n\nmine.\n\n- [stack] ripgrep — hits 2; last 2023-11-14; pie-session s turn 1; confidence full";
        assert_eq!(render_doc(&parse_doc(text)), text);
        assert_eq!(
            parse_doc("prose and nothing else\n").learned,
            "",
            "a document with no entries has no learned section"
        );
    }
}
