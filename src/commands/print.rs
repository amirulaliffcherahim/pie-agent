//! `pie -p/--print` — one delegate turn.
//!
//! Delegate mode means the whole task goes to the vendor CLI, which runs its
//! own tools and returns a final answer. pie injects no tools of its own
//! (`RISKS.md` §1), so there are no pie tool cards here — only the CLI's own
//! progress steps, which we surface on stderr.
//!
//! Every turn is recorded in the session log, success or failure, so `pie
//! usage` and the future "know me" job (M7) have real data to work from. The
//! same turn is also written to the session *store* (`DESIGN.md` §11) so the
//! conversation — not just its cost — survives the turn; that write is
//! best-effort, because a turn the user paid quota for must never be reported as
//! failed because a disk write did not land.
//!
//! This is also where a turn decides **which conversation it continues**.
//! `--continue` and `--session <id>` name one of pie's own sessions (the vendor
//! id that has to be handed back is read from that session's transcript);
//! `--resume <id>` hands a vendor's id straight back; and doing nothing starts a
//! fresh conversation, deliberately, because auto-resume would silently answer in
//! the wrong context (`DESIGN.md` §11). The three are mutually exclusive, and the
//! selection is resolved *before* anything is spawned, so a contradictory or
//! impossible selection costs no quota and leaves no trace on disk.
//!
//! A resumed turn whose vendor id no longer exists re-bootstraps from pie's own
//! transcript exactly once (`RISKS.md` §6). The prompt for that rebuild is one
//! function — [`seed_prompt`] — because stage C replaces raw seeding with a
//! compacted state block, and nothing else needs to move.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::cli::Cli;
use crate::config::Config;
use crate::error::{PieError, Result};
use crate::paths;
use crate::profile::Profile;
use crate::provider::{self, ChatRequest, StreamEvent};
use crate::session::{self, Session, SessionSummary, TurnRow};
use crate::session_log::{self, SessionLogRow};

/// `pie -p <PROMPT> [--model provider/model]`
pub async fn run(cfg: &Config, cli: &Cli, prompt: &str) -> Result<()> {
    // The projection is loaded once, before anything is spawned, and handed to
    // whichever backend runs. Every backend receives the identical bytes (M3's
    // Done condition, DESIGN §10); only the mechanism differs. A malformed
    // `profile.toml` therefore fails before a turn is paid for, not after.
    let profile = Profile::from_home()?;

    let (backend, model) = provider::resolve_target(cfg, cli.model.as_deref())?;

    let provider = provider::by_id(&backend).ok_or_else(|| PieError::UnknownBackend {
        backend: backend.clone(),
        known: super::models::known_backends(),
    })?;

    let mut request = build_request(cli, prompt, model.as_deref(), &profile);

    // Which conversation this turn continues. Resolved before anything is
    // spawned: a contradictory selection, an unknown session id or a `--continue`
    // with nothing to continue must cost nothing (`ACCEPTANCE.md` §8, stage B).
    let store = paths::sessions_dir();
    let selection = select(store.as_path(), cli, &backend, &request.cwd)?;

    // The session the turn is recorded in: the one it continues, or a new one.
    // Re-opening keeps the id *and* the transcript it names, so a continued turn
    // lands *in* that conversation instead of beside it.
    let mut session = match selection.continuation() {
        Some(continuation) => Session::reopen_in(&store, continuation.summary.clone()),
        None => Session::start_in(&store, &backend, &request.cwd),
    };

    apply_selection(&selection, &mut request, prompt);

    let started = Instant::now();
    let streamed = Arc::new(AtomicBool::new(false));

    let mut outcome = {
        let mut sink = make_sink(cli.json, Arc::clone(&streamed), session.id());
        provider.stream(cfg, &request, &mut sink).await
    };

    // `RISKS.md` §6: a vendor conversation id can be purged or rotated under us
    // (codex archives threads, agy keeps its own database, claude has retention),
    // and a resume that fails because the conversation is gone must re-bootstrap
    // from pie's own transcript rather than fail the turn.
    //
    // The guard against a *retry loop* is structural — there is exactly one `if`,
    // so a second failure is reported whatever it says.
    let rebuilt = match &outcome {
        Err(error) => {
            rebootstrap_prompt(&selection, error, streamed.load(Ordering::Relaxed), prompt)
        }
        Ok(_) => None,
    };

    if let Some(seeded) = rebuilt {
        tracing::warn!(
            session = %session.id(),
            conversation = %request.resume.as_deref().unwrap_or_default(),
            "the backend no longer has this conversation — re-bootstrapping the turn from pie's \
             own transcript"
        );

        // The retry is a *fresh* turn: no vendor id to hand back, and the prompt
        // carries the history pie still has.
        request.resume = None;
        request.prompt = seeded;
        let mut sink = make_sink(cli.json, Arc::clone(&streamed), session.id());
        outcome = provider.stream(cfg, &request, &mut sink).await;
    }

    // Both attempts, for the same reason the frozen log counts the wall clock the
    // user actually waited.
    let elapsed_ms = started.elapsed().as_millis() as u64;

    match outcome {
        Ok(turn) => {
            if !cli.json {
                if streamed.load(Ordering::Relaxed) {
                    // Deltas already reached stdout; just close the line.
                    println!();
                } else {
                    println!("{}", turn.text);
                }
            }

            session_log::append_best_effort(&SessionLogRow::from_turn(
                &backend,
                // `general.default_backend` can select a backend without a
                // model, and then the vendor CLI picks its own — which pie
                // cannot name. The frozen row schema has no null, so the field
                // is empty rather than an invented id.
                model.as_deref().unwrap_or_default(),
                &turn.usage,
                elapsed_ms,
                &turn.status,
            ));

            // Then the conversation itself, in its own file: the frozen log
            // records what the turn cost, the transcript records what was said.
            //
            // `prompt` is the user's own text, never `request.prompt` — which may
            // hold the seeded transcript of a re-bootstrapped turn. Recording the
            // payload would make the next re-bootstrap seed a seed, and the
            // history would nest inside itself.
            session.record_best_effort(&TurnRow::from_turn(
                &backend,
                model.as_deref().unwrap_or_default(),
                prompt,
                &turn,
            ));

            // The vendor's own id, and the session the row landed in.
            announce(&session, turn.conversation_id.as_deref());
            Ok(())
        }
        Err(error) => {
            // Record the attempt even when it failed: a timeout still costs
            // wall clock and is worth seeing in the totals.
            session_log::append_best_effort(&SessionLogRow::from_error(
                &backend,
                model.as_deref().unwrap_or_default(),
                error.kind(),
                elapsed_ms,
            ));

            // …and in the session too: what was asked when nothing came back is
            // the half of a session that a later resume needs most.
            session.record_best_effort(&TurnRow::from_error(
                &backend,
                model.as_deref().unwrap_or_default(),
                prompt,
                error.kind(),
            ));

            // A failed turn is recorded too, so it is named for the same reason a
            // successful one is: `--session <id>` must be reachable without first
            // running `pie sessions`.
            announce(&session, None);
            Err(error)
        }
    }
}

/// Say which session the turn landed in, and the vendor id it was given.
///
/// Both go to **stderr**: stdout carries the answer, and under `--json` it is the
/// machine channel. `[session] <id>` is what makes `--session <id>` and
/// `--continue` usable in the middle of a conversation (`ACCEPTANCE.md` §8, stage
/// B) — without it the id can only be found by running `pie sessions` first.
/// `[resume] <vendor-id>` is unchanged: it is the id a backend issued, and pie's
/// own id is not one the vendor would accept.
fn announce(session: &Session, vendor_id: Option<&str>) {
    eprintln!("[session] {}", session.id());
    if let Some(id) = vendor_id {
        eprintln!("[resume] {id}");
    }
}

// ------------------------------------------------- which conversation, if any --

/// Which conversation a turn continues.
///
/// One concept, three flags: `--resume <ID>` names a *vendor* conversation id,
/// while `--continue` and `--session <ID>` name one of *pie's* sessions and get
/// the vendor id out of that session's own transcript.
#[derive(Debug, Clone)]
pub enum Selection {
    /// No selector: a fresh conversation. The default, on purpose.
    Fresh,
    /// `--resume <vendor-id>`: hand the id straight back to the backend.
    Vendor(String),
    /// `--continue` or `--session <id>`: continue one of pie's sessions.
    Session(Continuation),
}

/// A session this turn continues.
#[derive(Debug, Clone)]
pub struct Continuation {
    /// The index frame: the id to address, and how many turns came before this
    /// one.
    pub summary: SessionSummary,
    /// The transcript so far, oldest first. Stage C's input.
    pub rows: Vec<TurnRow>,
    /// The newest `conversation_id` the transcript recorded, when there is one.
    ///
    /// `None` — every recorded turn failed before the vendor issued an id — means
    /// the backend has no conversation to continue, so the turn is seeded from
    /// [`seed_prompt`] instead of resuming nothing.
    pub vendor_id: Option<String>,
}

impl Selection {
    /// The vendor conversation id this selection hands to the backend.
    pub fn vendor_id(&self) -> Option<&str> {
        match self {
            Selection::Vendor(id) => Some(id),
            Selection::Session(continuation) => continuation.vendor_id.as_deref(),
            Selection::Fresh => None,
        }
    }

    /// The session this turn continues, when a selector named one.
    pub fn continuation(&self) -> Option<&Continuation> {
        match self {
            Selection::Session(continuation) => Some(continuation),
            _ => None,
        }
    }
}

/// Resolve which conversation this turn continues.
///
/// * `--continue` takes the **newest** session whose provider **and** working
///   directory both match. Both, not either: a vendor conversation id belongs to
///   the backend that minted it, and a session is a conversation about the
///   directory it ran in.
/// * `--session <id>` names one, so it is matched by id alone — that is what
///   "explicitly" means, and the provider has to agree because the id is not
///   portable between backends.
/// * Neither selects nothing, and nothing is the default.
///
/// A selection that cannot be honoured is an error rather than a silent fresh
/// turn: silently starting over when the user asked to continue is the failure
/// mode this milestone is designed against.
pub fn select(root: &Path, cli: &Cli, provider: &str, cwd: &Path) -> Result<Selection> {
    // Before any store read: a contradiction is a contradiction whatever the
    // disk holds (`DESIGN.md` §5, exit 2).
    reject_conflict(cli)?;

    if let Some(id) = cli.resume.as_deref() {
        return Ok(Selection::Vendor(id.to_string()));
    }

    if !cli.continue_session && cli.session.is_none() {
        return Ok(Selection::Fresh);
    }

    let summaries = session::list_from(root)?;

    let summary = match cli.session.as_deref() {
        Some(id) => named(&summaries, root, id)?,
        None => newest_for(&summaries, provider, cwd)
            .cloned()
            .ok_or_else(|| {
                PieError::Config(format!(
                "no session to continue for `{provider}` in {} — a conversation is continued in \
                 the directory and on the backend it ran in. `pie sessions` lists what exists, \
                 and a plain `pie -p` starts a fresh one",
                cwd.display()
            ))
            })?,
    };

    // The one check the `--continue` filter cannot make for `--session`: a
    // conversation id belongs to the backend that issued it, so continuing an
    // agy session on codex would hand over an id codex has never heard of. That
    // is stage F's job (`--switch`), and it re-bootstraps rather than resuming.
    if summary.provider != provider {
        return Err(PieError::Config(format!(
            "session `{}` ran on `{}`, so it cannot be continued on `{provider}` — a conversation \
             id belongs to the backend that issued it. Pass `--model {}/<model>`, or drop \
             `--session` to start a fresh conversation.",
            summary.id, summary.provider, summary.provider
        )));
    }

    Ok(Selection::Session(continuation(root, summary)?))
}

/// Point the request at the conversation the selection names.
///
/// A session whose transcript recorded no vendor id (every turn in it failed)
/// has no conversation to hand back, so it is seeded from its own rows straight
/// away — the same re-bootstrap as the stale-id path, minus the failed attempt.
pub fn apply_selection(selection: &Selection, request: &mut ChatRequest, task: &str) {
    match selection {
        Selection::Vendor(id) => request.resume = Some(id.clone()),
        Selection::Session(continuation) => match &continuation.vendor_id {
            Some(id) => request.resume = Some(id.clone()),
            None => request.prompt = seed_prompt(&continuation.rows, task),
        },
        Selection::Fresh => {}
    }
}

/// Refuse more than one way to select the same conversation.
///
/// `--continue`, `--session` and `--resume` all answer one question, so a
/// combination has no honest precedence: silently preferring one would continue
/// a conversation the user did not choose. The error names the pair.
fn reject_conflict(cli: &Cli) -> Result<()> {
    let mut named: Vec<&str> = Vec::new();
    if cli.continue_session {
        named.push("--continue");
    }
    if cli.session.is_some() {
        named.push("--session");
    }
    if cli.resume.is_some() {
        named.push("--resume");
    }
    if named.len() < 2 {
        return Ok(());
    }

    Err(PieError::Config(format!(
        "{} are mutually exclusive — they all select the conversation to continue. Pass one, or \
         none for a fresh conversation.",
        named.join(" and ")
    )))
}

/// The newest summary that matches both the backend and the directory.
///
/// The comparison is explicit rather than "the first match in `list_from`'s
/// order": newest-by-`updated_at` is the contract, and it should not silently
/// become newest-by-listing-order if the listing ever changes.
fn newest_for<'a>(
    summaries: &'a [SessionSummary],
    provider: &str,
    cwd: &Path,
) -> Option<&'a SessionSummary> {
    summaries
        .iter()
        .filter(|summary| {
            summary.provider == provider && same_directory(Path::new(&summary.cwd), cwd)
        })
        .max_by(|a, b| {
            a.updated_at
                .cmp(&b.updated_at)
                .then_with(|| a.id.cmp(&b.id))
        })
}

/// One session by pie's own id.
fn named(summaries: &[SessionSummary], root: &Path, id: &str) -> Result<SessionSummary> {
    if let Some(summary) = summaries.iter().find(|summary| summary.id == id) {
        return Ok(summary.clone());
    }

    // Not in the index. `load_from` re-checks the *shape* of the id, so a
    // path-shaped id (`../usage`) is refused for the reason it deserves rather
    // than reported as an unknown session; a well-formed id that is merely
    // unknown falls through to the message a user can act on.
    if let Err(error) = session::load_from(root, id) {
        if matches!(error, PieError::Config(_)) {
            return Err(error);
        }
    }

    Err(PieError::Config(format!(
        "no session `{}` — `pie sessions` lists the {} recorded",
        crate::error::truncate(id, 40),
        summaries.len()
    )))
}

/// The store's view of one session: its summary plus its transcript.
fn continuation(root: &Path, summary: SessionSummary) -> Result<Continuation> {
    let rows = session::load_from(root, &summary.id)?;

    // The newest row that has one, not the first: a session can fail a turn
    // after a successful one, and a failure row carries no conversation id.
    let vendor_id = rows
        .iter()
        .rev()
        .find_map(|row| row.conversation_id.as_deref())
        .filter(|id| !id.trim().is_empty())
        .map(str::to_string);

    Ok(Continuation {
        summary,
        rows,
        vendor_id,
    })
}

/// Whether two paths name the same directory.
///
/// A session records its working directory as a string, so `--continue` compares
/// what the index holds against the directory this run will use. Both are
/// canonicalised when they exist; otherwise separators are unified and the text
/// is case-folded on Windows. `C:/work`, `C:\work` and `C:\work\` are one
/// directory, and a `--continue` that quietly ran somewhere else is exactly what
/// this comparison exists to prevent.
fn same_directory(a: &Path, b: &Path) -> bool {
    if let (Ok(a), Ok(b)) = (a.canonicalize(), b.canonicalize()) {
        return a == b;
    }
    normalised(a) == normalised(b)
}

/// A path as text, comparable across separators and (on Windows) case.
fn normalised(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    let text = text.trim_end_matches('/');
    if cfg!(windows) {
        text.to_ascii_lowercase()
    } else {
        text.to_string()
    }
}

// -------------------------------------------------------- re-bootstrapping ----

/// Render a session's recorded turns into the prompt that re-bootstraps it.
///
/// Used when there is no conversation to hand back: the vendor id is gone
/// (`RISKS.md` §6), or the transcript never recorded one. The shape is the
/// history, then `---`, then the task — the same shape `DESIGN.md` §11 asks
/// compaction for, with the volatile task text last.
///
/// **Stage C seam.** Compaction replaces the body of this function with the head
/// plus an extractive state block plus a tail window, and nothing else has to
/// move: this is the only place an outgoing prompt is rebuilt. It takes the rows
/// rather than a session so stage C can hand it whatever window it decides on.
///
/// `TurnRow::prompt` is the user's own text and never the payload that was sent,
/// so seeding a payload that already contains a transcript cannot nest history
/// inside history. The rows are rendered in full: bounding them is exactly the
/// compaction stage C owns, and the argv cap (`DESIGN.md` §1) reports a prompt
/// that has outgrown a backend rather than silently dropping history here.
pub fn seed_prompt(rows: &[TurnRow], task: &str) -> String {
    let mut out = String::from(
        "pie is re-sending the earlier part of this session, because the backend does not have \
         the conversation.\n",
    );

    for (index, row) in rows.iter().enumerate() {
        out.push_str(&format!(
            "\n## turn {} ({}{})\n",
            index + 1,
            row.provider,
            // The model is empty when the backend picked its own (`DESIGN.md` §7).
            if row.model.is_empty() {
                String::new()
            } else {
                format!("/{}", row.model)
            }
        ));
        out.push_str("user: ");
        out.push_str(&row.prompt);
        out.push('\n');

        out.push_str("assistant: ");
        if row.response.trim().is_empty() {
            // A failed turn is still history: what was asked, and that nothing
            // came back. `stop_reason` is where the reason lives (§11).
            out.push_str(&format!("(no answer: {})", row.stop_reason));
        } else {
            out.push_str(&row.response);
        }
        out.push('\n');
    }

    out.push_str("\n---\n\n");
    out.push_str(task);
    out
}

/// The prompt a failed resume is replaced by, when the failure means the backend
/// no longer has the conversation (`RISKS.md` §6).
///
/// The decision and the rebuild are one function on purpose: a retry exists only
/// where there is a history to send instead of resuming, so the two cannot drift
/// apart. `None` means "report the failure" — either nothing was being resumed,
/// the session had no vendor id to lose, the attempt already produced answer text
/// (so whatever failed, it was not an empty conversation), or the message is not
/// about a missing conversation at all.
///
/// Callers retry **once**: this says whether a fresh attempt is warranted, never
/// how many.
pub fn rebootstrap_prompt(
    selection: &Selection,
    error: &PieError,
    streamed: bool,
    task: &str,
) -> Option<String> {
    let continuation = selection.continuation()?;
    continuation.vendor_id.as_ref()?;
    if streamed || !is_stale_conversation(error) {
        return None;
    }
    Some(seed_prompt(&continuation.rows, task))
}

/// Whether a failure means the backend no longer has the conversation.
///
/// `RISKS.md` §6: vendor ids are purged or rotated (codex archives threads under
/// `~/.codex`, agy keeps its own database, claude has retention), and none of the
/// CLIs documents a distinct exit code or error class for it, so this reads the
/// message for a *missing thing* named as a conversation or a session. Two
/// independent signals are required because the costly mistake is the other
/// direction — a false positive re-runs a turn that failed for a real reason,
/// spending subscription quota twice and replacing the real error with the
/// retry's. A false negative costs nothing but an error message the user can act
/// on.
///
/// The phrasing is therefore a heuristic over vendor text, deliberately kept to
/// markers rather than guessed sentences: the two-signal rule is what makes it
/// safe to be approximate.
pub fn is_stale_conversation(error: &PieError) -> bool {
    const MISSING: [&str; 9] = [
        "not found",
        "no such",
        "no rollout",
        "unknown",
        "does not exist",
        "doesn't exist",
        "no longer",
        "expired",
        "invalid",
    ];
    const CONVERSATION: [&str; 4] = ["conversation", "thread", "rollout", "session"];

    let text = error.to_string().to_ascii_lowercase();
    MISSING.iter().any(|marker| text.contains(marker))
        && CONVERSATION.iter().any(|noun| text.contains(noun))
}

/// Assemble the request from the command line.
///
/// `resume` is not read from the flags here: which conversation the turn
/// continues is decided by [`select`], because `--continue` and `--session` have
/// to look in the store first.
fn build_request(cli: &Cli, prompt: &str, model: Option<&str>, profile: &Profile) -> ChatRequest {
    let mut request = ChatRequest::new(prompt);
    request.model = model.map(str::to_string);
    request.persona = profile.persona().map(str::to_string);
    request.effort = cli.effort.clone();
    request.cwd = cli
        .cwd
        .clone()
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));
    request.timeout = cli.timeout.map(Duration::from_secs);
    request
}

/// Where stream events go.
///
/// Human mode prints answer text to **stdout** so it can be piped, and tool
/// progress to **stderr** so it never corrupts that pipe. `--json` emits one
/// event per line on stdout instead.
fn make_sink(
    json: bool,
    streamed: Arc<AtomicBool>,
    session_id: &str,
) -> impl FnMut(StreamEvent) + Send {
    let session_id = session_id.to_string();

    move |event: StreamEvent| {
        if json {
            // Tracked in both modes: the flag says whether any answer text has
            // reached the user, and the re-bootstrap guard reads it under
            // `--json` too.
            if matches!(event, StreamEvent::TextDelta(_)) {
                streamed.store(true, Ordering::Relaxed);
            }
            if let Some(line) = json_line(&event, &session_id) {
                println!("{line}");
            }
            return;
        }

        match event {
            StreamEvent::TextDelta(text) => {
                use std::io::Write;
                streamed.store(true, Ordering::Relaxed);
                print!("{text}");
                let _ = std::io::stdout().flush();
            }
            StreamEvent::ToolStep { name, state, .. } => {
                eprintln!("[tool] {name} {state:?}");
            }
            // Init and Usage are folded into the outcome; Done would duplicate
            // the text we have already streamed.
            _ => {}
        }
    }
}

/// One `--json` line, with pie's session id added to the `init` event.
///
/// `StreamEvent` describes what a *backend* said, and no backend knows pie's
/// session id, so the key is added here — at the one place that knows both. It is
/// the same `init` event with one more key rather than a new event kind, so a
/// consumer that reads the keys it knows is unaffected.
///
/// Only `init` is rebuilt through a JSON map; every other event is serialised
/// straight from its type so its bytes stay exactly what they were. (A JSON object
/// has no key order, but a byte-exact consumer is entitled to one, and a map would
/// have silently reordered *every* event on the wire.)
///
/// `None` only if serialisation fails, which for these types it cannot; a dropped
/// event is what the previous `if let Ok(line)` did, and inventing a placeholder
/// would be worse than silence.
fn json_line(event: &StreamEvent, session_id: &str) -> Option<String> {
    match event {
        StreamEvent::Init { .. } => {
            let mut value = serde_json::to_value(event).ok()?;
            if let Some(data) = value
                .get_mut("data")
                .and_then(serde_json::Value::as_object_mut)
            {
                data.insert(
                    "session_id".to_string(),
                    serde_json::Value::String(session_id.to_string()),
                );
            }
            serde_json::to_string(&value).ok()
        }
        other => serde_json::to_string(other).ok(),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn a_directories_spelling_does_not_change_which_session_matches() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("work");
        std::fs::create_dir_all(&sub).unwrap();

        assert!(same_directory(&sub, &sub));
        assert!(
            same_directory(&sub.join("..").join("work"), &sub),
            "a path with `..` is the same directory"
        );
        assert!(!same_directory(&sub, &dir.path().join("elsewhere")));

        // Neither side has to exist: a recorded directory that has since been
        // deleted still compares by text.
        assert!(same_directory(
            Path::new(r"C:\tmp\work\"),
            Path::new("C:/tmp/work")
        ));
        assert!(same_directory(
            Path::new("/tmp/work/"),
            Path::new("/tmp/work")
        ));
        assert!(!same_directory(
            Path::new("/tmp/work"),
            Path::new("/tmp/other")
        ));
    }

    #[test]
    fn the_init_event_carries_the_session_id_and_other_events_do_not_change() {
        let init = StreamEvent::Init {
            conversation_id: Some("conv-1".into()),
            cwd: Some(PathBuf::from("/w")),
            tools: vec!["view_file".into()],
        };
        let line = json_line(&init, "s-1-2").unwrap();
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["event"], "init");
        assert_eq!(value["data"]["session_id"], "s-1-2");
        assert_eq!(value["data"]["conversation_id"], "conv-1");
        assert_eq!(
            value["data"]["cwd"], "/w",
            "the backend's own fields survive"
        );
        assert_eq!(value["data"]["tools"][0], "view_file");

        // The added key must not make the event unreadable by pie's own parser.
        let parsed: StreamEvent = serde_json::from_str(&line).expect("still a valid event");
        assert_eq!(parsed, init);

        // Every other event is byte-for-byte what it always was.
        assert_eq!(
            json_line(&StreamEvent::TextDelta("hi".into()), "s-1-2").unwrap(),
            r#"{"event":"text","data":"hi"}"#
        );
    }
}
