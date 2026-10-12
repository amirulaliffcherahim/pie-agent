//! The session store: pie's own transcripts, beside the frozen log.
//!
//! `session_log.rs` records *accounting*; this module records the
//! *conversation*. They are separate files on purpose (`DESIGN.md` §11):
//! `usage.jsonl` is frozen and shared with the earlier JS implementation, so a
//! transcript could never be added to it without breaking the one file two
//! implementations already read.
//!
//! ```text
//! $PIE_HOME/sessions/
//! ├── <id>.jsonl      the transcript: one row per turn, append-only
//! └── index.jsonl     one summary frame per session, for lookup without a scan
//! ```
//!
//! Four decisions are load-bearing, so each is stated rather than implied:
//!
//! * **pie mints its own id** (`s-<epoch-ms>-<pid>`). A vendor id differs per
//!   backend and is lost the moment a conversation switches backend, so it
//!   cannot identify a session; it is recorded per row as `conversation_id`
//!   instead, where a resume can hand it back to the backend that issued it.
//! * **Both files are append-only.** `index.jsonl` is a log of summary frames
//!   and [`list`] folds it last-wins by id, so there is no rewrite, no lock, and
//!   no half-written file — the same discipline the frozen log already uses.
//! * **The transcript is written before the index.** A failure between the two
//!   leaves a transcript with no summary, which is exactly what "if the two ever
//!   disagree, the index is the thing to rebuild" asks for. The reverse order
//!   would leave a summary claiming a turn that does not exist.
//! * **A write failure never fails a turn** (`DESIGN.md` §11). A turn is what the
//!   user paid subscription quota for; the recording path therefore goes through
//!   [`Session::record_best_effort`], which logs and returns `()`.
//!
//! `prompt` is the user's own text, never the folded payload: the profile
//! projection is assembled fresh from `profile.toml` on every turn
//! (`DESIGN.md` §10), so copying it into every row would double the store and buy
//! nothing a resume can use.
//!
//! Nothing here spawns a process, so every path in this module is quota-free.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{PieError, Result};
use crate::paths;
use crate::provider::{TurnOutcome, Usage};
use crate::session_log::now_millis;

/// Characters of the first prompt kept as a listing preview.
pub const PREVIEW_CHARS: usize = 80;

/// Longest session id accepted on the read path.
///
/// Ids are minted by pie (see [`mint_id`]) and are 8–24 characters. The bound
/// exists so a hand-edited index or a user-supplied id cannot make pie build a
/// path of arbitrary length.
const MAX_ID_CHARS: usize = 64;

/// One turn of a transcript. Field names are fixed by `DESIGN.md` §11.
///
/// `#[serde(default)]` on every read: a row written by an older pie must not
/// make the whole transcript unreadable (§11 fixes the keys, not their
/// optionality, and a partial row is still history).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TurnRow {
    /// Epoch milliseconds.
    pub ts: u64,
    /// Provider id.
    pub provider: String,
    /// Model id; empty when the backend picked its own.
    pub model: String,
    /// The user's prompt, unfolded.
    pub prompt: String,
    /// The answer text; empty for a turn that failed before producing one.
    pub response: String,
    /// The vendor's conversation id, when it issued one.
    ///
    /// Per row, not per session: it is the only thing that lets a resume hand the
    /// id back to the backend that minted it.
    pub conversation_id: Option<String>,
    /// Token accounting for this turn.
    pub usage: Usage,
    /// Whether the turn succeeded.
    pub ok: bool,
    /// The vendor's status on success; the error class on failure.
    pub stop_reason: String,
}

impl TurnRow {
    /// A row for a turn that produced an answer.
    pub fn from_turn(
        provider: impl Into<String>,
        model: impl Into<String>,
        prompt: impl Into<String>,
        turn: &TurnOutcome,
    ) -> Self {
        Self {
            ts: now_millis(),
            provider: provider.into(),
            model: model.into(),
            prompt: prompt.into(),
            response: turn.text.clone(),
            conversation_id: turn.conversation_id.clone(),
            usage: turn.usage.clone(),
            ok: true,
            stop_reason: turn.status.clone(),
        }
    }

    /// A row for a turn that failed.
    ///
    /// A failure is part of the session: it is the half of the history that says
    /// what was *asked* when nothing came back. The row schema has no
    /// `error_kind` key (`DESIGN.md` §11 fixes nine of them and this is not one),
    /// so the error class goes in `stop_reason` — the same slot the frozen log
    /// fills with the vendor's status.
    pub fn from_error(
        provider: impl Into<String>,
        model: impl Into<String>,
        prompt: impl Into<String>,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            ts: now_millis(),
            provider: provider.into(),
            model: model.into(),
            prompt: prompt.into(),
            response: String::new(),
            conversation_id: None,
            usage: Usage::default(),
            ok: false,
            stop_reason: reason.into(),
        }
    }
}

/// One summary frame of `index.jsonl`. Field names are fixed by `DESIGN.md` §11.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionSummary {
    /// pie's own session id.
    pub id: String,
    /// Provider id the turn ran on.
    pub provider: String,
    /// Working directory the session ran in.
    pub cwd: String,
    /// Epoch milliseconds when the session was minted.
    pub started_at: u64,
    /// Epoch milliseconds of the newest recorded turn.
    pub updated_at: u64,
    /// Turns recorded so far.
    pub turns: u64,
    /// One line of the first prompt, for a listing.
    pub preview: String,
}

/// A handle on one session's identity and progress.
///
/// It holds no open file: every write is a short append, so a session cannot leak
/// a handle and a crashed turn can leave at most one truncated line. The struct
/// carries the running state (`turns`, `preview`, `updated_at`) that the next
/// summary frame needs, which is why recording takes `&mut self`.
#[derive(Debug, Clone)]
pub struct Session {
    root: PathBuf,
    id: String,
    provider: String,
    cwd: String,
    started_at: u64,
    updated_at: u64,
    turns: u64,
    preview: String,
}

impl Session {
    /// Mint a session against the default store (`$PIE_HOME/sessions/`).
    ///
    /// Mints nothing else: the id exists only in memory until the first
    /// [`Self::record`], so a command that never reaches a turn leaves no trace.
    pub fn start(provider: impl Into<String>, cwd: &Path) -> Self {
        Self::start_in(paths::sessions_dir(), provider, cwd)
    }

    /// Mint a session against an explicit store root.
    ///
    /// The read paths take an explicit root too, so a test can exercise the whole
    /// store without touching `PIE_HOME`.
    pub fn start_in(root: impl Into<PathBuf>, provider: impl Into<String>, cwd: &Path) -> Self {
        let root = root.into();
        let started_at = now_millis();
        Self {
            id: mint_id(&root, started_at),
            root,
            provider: provider.into(),
            cwd: cwd.display().to_string(),
            started_at,
            updated_at: started_at,
            turns: 0,
            preview: String::new(),
        }
    }

    /// pie's own session id.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Re-open a recorded session so the next turn appends to its transcript.
    ///
    /// [`Self::start_in`] cannot be used for this: it mints a new id, and the
    /// turn would then land in a second session beside the one the user asked to
    /// continue. The running state the next summary frame needs (`turns`,
    /// `preview`, `updated_at`) is taken from the index frame, because the index —
    /// not this process — is what knows how many turns came before.
    ///
    /// The id is not re-validated here; every write path validates it
    /// (`append_row` → `validate_id`), so a hand-edited index cannot talk this
    /// into writing outside the store.
    pub fn reopen_in(root: impl Into<PathBuf>, summary: SessionSummary) -> Self {
        Self {
            root: root.into(),
            id: summary.id,
            provider: summary.provider,
            cwd: summary.cwd,
            started_at: summary.started_at,
            updated_at: summary.updated_at,
            turns: summary.turns,
            preview: summary.preview,
        }
    }

    /// Re-open a recorded session whose next turn runs on a **different** backend.
    ///
    /// `--switch <backend>` (`DESIGN.md` §11) continues one of pie's sessions on
    /// another backend, so the id and the transcript are kept — that is what makes
    /// a switch a continuation rather than a second conversation — while
    /// `provider` becomes the backend the next turn runs on.
    ///
    /// It has to change, and for a reason that is not cosmetic: the index frame
    /// describes the **newest** turn, so a frame still naming the backend that was
    /// left would disagree with the row written beneath it — the two are asserted
    /// to agree — and `--continue` on the new backend could never find a
    /// conversation it now holds, because the id it would hand back was the one
    /// that backend issued.
    pub fn reopen_on_in(
        root: impl Into<PathBuf>,
        summary: SessionSummary,
        provider: impl Into<String>,
    ) -> Self {
        Self {
            provider: provider.into(),
            ..Self::reopen_in(root, summary)
        }
    }

    /// `$PIE_HOME/sessions/<id>.jsonl`
    pub fn transcript_path(&self) -> PathBuf {
        transcript_path(&self.root, &self.id)
    }

    /// Turns recorded by this handle.
    pub fn turns(&self) -> u64 {
        self.turns
    }

    /// The index frame this session currently deserves.
    pub fn summary(&self) -> SessionSummary {
        SessionSummary {
            id: self.id.clone(),
            provider: self.provider.clone(),
            cwd: self.cwd.clone(),
            started_at: self.started_at,
            updated_at: self.updated_at,
            turns: self.turns,
            preview: self.preview.clone(),
        }
    }

    /// Append one turn to the transcript, then fold a summary onto the index.
    ///
    /// Order matters: the transcript is authoritative and the index is derived,
    /// so a failure here can leave the index behind — never ahead of — the
    /// transcript.
    pub fn record(&mut self, row: &TurnRow) -> Result<()> {
        append_row(&self.root, &self.id, row)?;

        self.turns = self.turns.saturating_add(1);
        // The newest row's own timestamp, not `now_millis()`: the index then
        // describes the transcript exactly, so a rebuild from `sessions/*.jsonl`
        // reproduces it.
        self.updated_at = self.updated_at.max(row.ts);
        if self.preview.is_empty() {
            self.preview = preview_of(&row.prompt);
        }

        append_summary(&self.root, &self.summary())
    }

    /// Record a turn, logging (never propagating) any failure.
    ///
    /// This is the only entry point the turn path uses: a chat answer that
    /// arrived must never be reported as a failed turn because the disk was
    /// full, and an error the user cannot act on mid-turn would only be noise.
    pub fn record_best_effort(&mut self, row: &TurnRow) {
        if let Err(error) = self.record(row) {
            tracing::warn!(
                session = %self.id,
                path = %self.transcript_path().display(),
                error = %error,
                "could not record the turn in the session store"
            );
        }
    }
}

/// Every recorded session, newest first.
///
/// Reads `index.jsonl` only — that is the whole point of the index — and is
/// tolerant in the way `session_log::read_all` is: an unparseable frame is
/// skipped, never fatal.
pub fn list() -> Result<Vec<SessionSummary>> {
    list_from(&paths::sessions_dir())
}

/// Every recorded session under an explicit store root, newest first.
///
/// A summary whose transcript is missing is dropped with a warning: the index is
/// derived state, and an entry that points at nothing would make a later
/// `load` fail for a reason the user cannot see.
pub fn list_from(root: &Path) -> Result<Vec<SessionSummary>> {
    let mut summaries: Vec<SessionSummary> = Vec::new();

    for frame in read_summaries(root)? {
        if !is_valid_id(&frame.id) {
            tracing::warn!(
                id = %frame.id,
                "ignoring an index frame whose id is not a pie session id"
            );
            continue;
        }
        if !transcript_path(root, &frame.id).is_file() {
            tracing::warn!(
                session = %frame.id,
                "index frame has no transcript; skipping it — the index is rebuildable \
                 from `sessions/*.jsonl`"
            );
            summaries.retain(|s| s.id != frame.id);
            continue;
        }
        // Append-only log: the newest frame for an id wins.
        match summaries.iter_mut().find(|s| s.id == frame.id) {
            Some(slot) => *slot = frame,
            None => summaries.push(frame),
        }
    }

    summaries.sort_by(|a, b| {
        b.updated_at
            .cmp(&a.updated_at)
            .then_with(|| b.id.cmp(&a.id))
    });
    Ok(summaries)
}

/// Read one session's transcript. Empty transcript files read as empty.
pub fn load(id: &str) -> Result<Vec<TurnRow>> {
    load_from(&paths::sessions_dir(), id)
}

/// Read one session's transcript from an explicit store root.
///
/// A missing transcript is an error rather than an empty list: the caller named a
/// session, and answering "no turns" for a typo would be a silent lie.
pub fn load_from(root: &Path, id: &str) -> Result<Vec<TurnRow>> {
    validate_id(id)?;
    let path = transcript_path(root, id);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(PieError::MissingFile(path))
        }
        Err(e) => return Err(e.into()),
    };

    let mut rows = Vec::new();
    let mut skipped = 0usize;
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<TurnRow>(line) {
            Ok(row) => rows.push(row),
            Err(_) => skipped += 1,
        }
    }
    if skipped > 0 {
        tracing::warn!(skipped, path = %path.display(), "skipped unreadable transcript rows");
    }
    Ok(rows)
}

/// `$PIE_HOME/sessions/<id>.jsonl`, or `<root>/<id>.jsonl`.
pub fn transcript_path(root: &Path, id: &str) -> PathBuf {
    root.join(format!("{id}.jsonl"))
}

/// `<root>/index.jsonl`.
pub fn index_path(root: &Path) -> PathBuf {
    root.join("index.jsonl")
}

/// Mint an id that is unique on this machine and sorts by time.
///
/// The timestamp keeps a listing readable and the pid makes a collision between
/// two pie processes in the same millisecond impossible in practice; the counter
/// covers the case where it is not (a reused pid on a coarse clock).
fn mint_id(root: &Path, ts: u64) -> String {
    let base = format!("s-{ts}-{}", std::process::id());
    let mut candidate = base.clone();
    let mut suffix = 2u64;
    while root.join(format!("{candidate}.jsonl")).exists() {
        candidate = format!("{base}-{suffix}");
        suffix = suffix.saturating_add(1);
    }
    candidate
}

/// Whether `id` is one pie could have minted.
///
/// Ids are used to build a path, so the accepted set is deliberately narrow:
/// ASCII alphanumerics, `-` and `_`. That rules out separators, `..`, drive
/// letters and a trailing dot — all of which would let a caller (or a hand-edited
/// index) name a file outside the store.
fn is_valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.chars().count() <= MAX_ID_CHARS
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Reject an id that could not name a transcript in the store.
fn validate_id(id: &str) -> Result<()> {
    if is_valid_id(id) {
        return Ok(());
    }
    Err(PieError::Config(format!(
        "invalid session id `{}` — pie session ids look like `s-1776000000000-4821`",
        crate::error::truncate(id, 40)
    )))
}

/// The first non-blank line of a prompt, truncated for a listing.
fn preview_of(prompt: &str) -> String {
    let line = prompt
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or_default()
        .trim();

    if line.chars().count() <= PREVIEW_CHARS {
        return line.to_string();
    }
    let kept: String = line.chars().take(PREVIEW_CHARS).collect();
    format!("{kept}…")
}

/// Append one row to `<root>/<id>.jsonl`, creating the store directory.
fn append_row(root: &Path, id: &str, row: &TurnRow) -> Result<()> {
    validate_id(id)?;
    let mut line = serde_json::to_string(row)?;
    line.push('\n');
    append_line(&transcript_path(root, id), &line)
}

/// Append one frame to `<root>/index.jsonl`.
fn append_summary(root: &Path, summary: &SessionSummary) -> Result<()> {
    let mut line = serde_json::to_string(summary)?;
    line.push('\n');
    append_line(&index_path(root), &line)
}

/// The shared append: create the parent, open for append, write, flush.
///
/// Flushed before returning because the next thing that happens may be a process
/// exit — and a transcript that only reaches the page cache is not a transcript.
fn append_line(path: &Path, line: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }

    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    file.write_all(line.as_bytes())?;
    file.flush()?;
    Ok(())
}

/// Every parseable frame of `index.jsonl`; unparseable frames are skipped.
fn read_summaries(root: &Path) -> Result<Vec<SessionSummary>> {
    let path = index_path(root);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };

    let mut frames = Vec::new();
    let mut skipped = 0usize;
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<SessionSummary>(line) {
            Ok(frame) => frames.push(frame),
            Err(_) => skipped += 1,
        }
    }
    if skipped > 0 {
        tracing::warn!(skipped, path = %path.display(), "skipped unreadable index frames");
    }
    Ok(frames)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn outcome(text: &str, conversation_id: Option<&str>, status: &str) -> TurnOutcome {
        TurnOutcome {
            text: text.to_string(),
            usage: Usage {
                input_tokens: 20_704,
                output_tokens: 20,
                thinking_tokens: 19,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                total_tokens: 20_724,
            },
            status: status.to_string(),
            conversation_id: conversation_id.map(str::to_string),
            duration: std::time::Duration::from_millis(35_900),
        }
    }

    fn store() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn a_minted_id_is_a_valid_one_and_sorts_by_time() {
        let dir = store();
        let session = Session::start_in(dir.path(), "agy", Path::new("/work"));
        assert!(is_valid_id(session.id()), "minted {id}", id = session.id());
        assert!(
            session
                .id()
                .starts_with(&format!("s-{}-", session.summary().started_at)),
            "the id carries its mint time: {}",
            session.id()
        );
        assert!(
            !session.transcript_path().exists(),
            "minting must not touch the disk"
        );
    }

    #[test]
    fn a_colliding_id_gets_a_suffix_rather_than_overwriting() {
        let dir = store();
        let first = Session::start_in(dir.path(), "agy", Path::new("/work"));
        // Force the collision the counter exists for: put a transcript where the
        // next mint at the same clock reading would land.
        append_row(dir.path(), first.id(), &TurnRow::default()).unwrap();
        let second = mint_id(dir.path(), first.summary().started_at);
        assert_ne!(second, first.id());
        assert!(second.starts_with(first.id()), "{second}");
    }

    #[test]
    fn a_row_carries_exactly_the_nine_fixed_keys() {
        let row = TurnRow::from_turn(
            "agy",
            "gemini-3.8-flash",
            "audit this repo",
            &outcome("OK\n", Some("conv-1"), "SUCCESS"),
        );
        let line = serde_json::to_string(&row).unwrap();
        // The on-disk line keeps the struct's own order (`to_value` would sort it
        // through a map), and the leading field is the one a reader looks for.
        assert!(line.starts_with("{\"ts\":"), "{line}");

        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        let mut keys: Vec<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec![
                "conversation_id",
                "model",
                "ok",
                "prompt",
                "provider",
                "response",
                "stop_reason",
                "ts",
                "usage"
            ],
            "DESIGN §11 fixes this row: exactly these nine keys"
        );
        assert_eq!(value["ok"], true);
        assert_eq!(value["stop_reason"], "SUCCESS");
        assert_eq!(value["usage"]["input_tokens"], 20_704);
    }

    #[test]
    fn a_failure_row_records_the_reason_in_stop_reason() {
        let row = TurnRow::from_error("codex", "gpt-6-luna", "hi", "timeout");
        assert!(!row.ok);
        assert_eq!(row.stop_reason, "timeout");
        assert_eq!(row.response, "");
        assert!(row.conversation_id.is_none());
        assert_eq!(row.usage, Usage::default());
    }

    #[test]
    fn recording_appends_a_row_and_a_summary_that_agree() {
        let dir = store();
        let mut session = Session::start_in(dir.path(), "agy", Path::new("/work/pie"));
        session
            .record(&TurnRow::from_turn(
                "agy",
                "gemini-3.8-flash",
                "audit this repo",
                &outcome("OK\n", Some("conv-1"), "SUCCESS"),
            ))
            .unwrap();

        let rows = load_from(dir.path(), session.id()).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].prompt, "audit this repo");

        let summaries = list_from(dir.path()).unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].turns, rows.len() as u64);
        assert_eq!(summaries[0].provider, rows[0].provider);
        assert_eq!(summaries[0].cwd, "/work/pie");
        assert_eq!(summaries[0].preview, "audit this repo");
    }

    #[test]
    fn a_second_turn_folds_onto_the_same_summary() {
        let dir = store();
        let mut session = Session::start_in(dir.path(), "agy", Path::new("/work"));
        for prompt in ["first", "second"] {
            session
                .record(&TurnRow::from_turn(
                    "agy",
                    "m",
                    prompt,
                    &outcome("OK", None, "SUCCESS"),
                ))
                .unwrap();
        }

        assert_eq!(load_from(dir.path(), session.id()).unwrap().len(), 2);
        let summaries = list_from(dir.path()).unwrap();
        assert_eq!(summaries.len(), 1, "one session, not two");
        assert_eq!(summaries[0].turns, 2);
        assert_eq!(
            summaries[0].preview, "first",
            "the preview describes the session, so the first prompt wins"
        );
    }

    #[test]
    fn the_index_is_read_last_wins_and_tolerates_junk() {
        let dir = store();
        let mut session = Session::start_in(dir.path(), "agy", Path::new("/work"));
        session
            .record(&TurnRow::from_turn(
                "agy",
                "m",
                "one",
                &outcome("OK", None, "SUCCESS"),
            ))
            .unwrap();

        // A hand-edited index, in the shape a crashed writer would leave.
        let index = index_path(dir.path());
        let mut text = std::fs::read_to_string(&index).unwrap();
        text.push_str("not json\n\n");
        std::fs::write(&index, text).unwrap();

        let summaries = list_from(dir.path()).unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].turns, 1);
    }

    #[test]
    fn a_summary_without_a_transcript_is_dropped() {
        let dir = store();
        let orphan = SessionSummary {
            id: "s-1-2".to_string(),
            provider: "agy".to_string(),
            turns: 3,
            ..Default::default()
        };
        append_summary(dir.path(), &orphan).unwrap();
        assert!(
            list_from(dir.path()).unwrap().is_empty(),
            "the index is derived: an entry pointing at nothing is not a session"
        );
    }

    #[test]
    fn sessions_are_listed_newest_first() {
        let dir = store();
        for (i, provider) in ["agy", "codex"].iter().enumerate() {
            let mut session = Session::start_in(dir.path(), *provider, Path::new("/work"));
            let mut row = TurnRow::from_turn(*provider, "m", "hi", &outcome("OK", None, "SUCCESS"));
            row.ts = 1_000 + i as u64;
            session.record(&row).unwrap();
        }
        let ids: Vec<String> = list_from(dir.path())
            .unwrap()
            .into_iter()
            .map(|s| s.provider)
            .collect();
        assert_eq!(ids, vec!["codex", "agy"]);
    }

    #[test]
    fn a_missing_transcript_is_a_stated_error_not_an_empty_list() {
        let dir = store();
        let error = load_from(dir.path(), "s-1-2").unwrap_err();
        assert!(matches!(error, PieError::MissingFile(_)), "got {error:?}");
    }

    #[test]
    fn an_id_that_could_escape_the_store_is_refused() {
        let dir = store();
        for bad in ["../usage", "..", "a/b", "", "id.jsonl", "C:\\x"] {
            assert!(!is_valid_id(bad), "`{bad}` must not be a session id");
            assert!(
                matches!(load_from(dir.path(), bad), Err(PieError::Config(_))),
                "`{bad}` must be refused on the read path too"
            );
        }
    }

    #[test]
    fn a_write_failure_is_reported_but_never_propagated_by_the_best_effort_path() {
        let dir = store();
        // A *file* where the store directory belongs: `create_dir_all` fails on
        // both Windows and Unix, so the append cannot succeed.
        let blocked = dir.path().join("sessions");
        std::fs::write(&blocked, "not a directory").unwrap();

        let mut session = Session::start_in(&blocked, "agy", Path::new("/work"));
        let row = TurnRow::from_turn("agy", "m", "hi", &outcome("OK", None, "SUCCESS"));
        assert!(
            session.record(&row).is_err(),
            "the fallible path must report the failure"
        );
        // …and the infallible one must swallow it, leaving the turn's result as
        // the only thing the caller sees.
        session.record_best_effort(&row);
        assert_eq!(session.turns(), 0, "nothing was recorded");
        assert!(blocked.is_file(), "the obstruction is untouched");
    }

    #[test]
    fn a_reopened_session_continues_the_transcript_it_names() {
        let dir = store();
        let mut first = Session::start_in(dir.path(), "agy", Path::new("/work"));
        first
            .record(&TurnRow::from_turn(
                "agy",
                "m",
                "one",
                &outcome("OK", None, "SUCCESS"),
            ))
            .unwrap();

        let summary = list_from(dir.path()).unwrap().remove(0);
        let mut reopened = Session::reopen_in(dir.path(), summary);
        assert_eq!(
            reopened.id(),
            first.id(),
            "re-opening must not mint a second id"
        );
        assert_eq!(reopened.turns(), 1, "the index knows what came before");
        assert_eq!(reopened.transcript_path(), first.transcript_path());

        reopened
            .record(&TurnRow::from_turn(
                "agy",
                "m",
                "two",
                &outcome("OK", None, "SUCCESS"),
            ))
            .unwrap();

        assert_eq!(
            load_from(dir.path(), first.id()).unwrap().len(),
            2,
            "the second turn joined the first conversation"
        );
        let summaries = list_from(dir.path()).unwrap();
        assert_eq!(summaries.len(), 1, "one session, folded, not two");
        assert_eq!(summaries[0].turns, 2);
        assert_eq!(
            summaries[0].preview, "one",
            "the preview still describes the first turn"
        );
    }

    #[test]
    fn the_preview_is_one_line_and_bounded() {
        assert_eq!(
            preview_of("\n\n  audit this repo  \nmore"),
            "audit this repo"
        );
        assert_eq!(preview_of(""), "");
        let long = preview_of(&"x".repeat(PREVIEW_CHARS + 10));
        assert_eq!(long.chars().count(), PREVIEW_CHARS + 1);
        assert!(long.ends_with('…'));
        // Char-safe on multi-byte input.
        let unicode = preview_of(&"é".repeat(PREVIEW_CHARS + 5));
        assert_eq!(unicode.chars().count(), PREVIEW_CHARS + 1);
    }
}
