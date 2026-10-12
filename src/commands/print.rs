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
//! `--switch <backend>` is the fourth answer: keep the session, change the
//! backend. A vendor conversation id belongs to the backend that minted it, so a
//! switch **never resumes** — the new backend receives pie's own transcript,
//! rebuilt by [`rebuild`], and the id *that* backend returns is what the next turn
//! records. Nothing on that path touches a vendor until the rebuilt payload is
//! sent, which is why stage F is verified at the payload level and spends no
//! quota.
//!
//! A resumed turn whose vendor id no longer exists re-bootstraps from pie's own
//! transcript exactly once (`RISKS.md` §6), through the same [`rebuild`] the
//! switch uses, so the two paths cannot send different shapes for the same
//! session. Compaction (`compact.rs`) is folded in there rather than here: the
//! rebuild is the only place an outgoing prompt is rebuilt, which is the seam
//! stage B left and stage C filled.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::cli::Cli;
use crate::compact;
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

    // `--switch` decides the backend, so it is resolved before the model is: an
    // unknown backend, a `--resume` a switch cannot honour and a switch with no
    // session to switch are all refused here, before a store read and long before
    // a spawn (`DESIGN.md` §5, exit 2).
    let switch = Switch::from_cli(cli)?;

    let (backend, model) = match &switch {
        Some(switch) => (switch.backend.clone(), switch.model.clone()),
        None => provider::resolve_target(cfg, cli.model.as_deref())?,
    };

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
    let mut session = session_for(&store, &selection, &backend, &request.cwd);

    // A switch hands back the payload it rebuilt, and that is reported before the
    // turn runs: history compaction left out is history the user paid for, so it
    // is never dropped in silence.
    if let Some(rebuilt) = apply_selection(&selection, &mut request, prompt) {
        let from = selection
            .continuation()
            .map(|continuation| continuation.summary.provider.as_str())
            .unwrap_or_default();
        eprintln!("{}", switch_notice(from, &backend, &rebuilt));
    }

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

/// `--switch <backend>`: continue one of pie's sessions on a different backend.
///
/// The whole reason this exists is that a vendor conversation id is **not
/// portable**: it belongs to the backend that minted it, and a backend that has
/// never seen it either errors or, worse, resolves it to something else. So a
/// switch never resumes. It re-sends pie's own transcript — rebuilt by [`rebuild`]
/// — and records the id *that* backend returns (`DESIGN.md` §11), which makes it a
/// payload transformation and therefore testable without spending quota.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Switch {
    /// The backend this turn runs on.
    pub backend: String,
    /// The model on that backend, when `--model` named one.
    pub model: Option<String>,
}

impl Switch {
    /// Read `--switch` off the command line, refusing the combinations that have no
    /// honest meaning.
    ///
    /// Four rules, each a [`PieError::Config`] (exit 2, `DESIGN.md` §5) decided
    /// before a store read and long before a spawn:
    ///
    /// * an **unknown backend** is named back at the user with the valid ids,
    ///   exactly as a misspelt `--model` is. Never a silent fallback to the default
    ///   backend: that would answer on a subscription the user did not ask for,
    ///   which is the failure routing exists to prevent.
    /// * `--model` may name the model **on** the new backend and nothing else. A
    ///   spec naming a third backend is a contradiction, and silently preferring
    ///   one of the two would leave the account it names.
    /// * `--resume <id>` names a vendor conversation on a specific backend, and the
    ///   point of a switch is to leave that conversation behind, so the pair is
    ///   refused rather than resolved: there is no precedence that honours both.
    /// * a switch needs something to switch. With neither `--continue` nor
    ///   `--session` there is no session to re-bootstrap, and quietly running a
    ///   fresh turn is the failure mode this milestone keeps designing against.
    ///
    /// `general.default_model` is deliberately **not** consulted: it names a
    /// backend, and a switch that quietly used another backend's configured model
    /// would leave the account it names. With `--switch` the model is either named
    /// beside the new backend, or the new CLI picks its own.
    pub fn from_cli(cli: &Cli) -> Result<Option<Self>> {
        let Some(backend) = cli.switch.as_deref() else {
            return Ok(None);
        };
        let backend = backend.trim();

        if provider::by_id(backend).is_none() {
            return Err(PieError::Config(format!(
                "unknown backend `{}` for `--switch` (known: {}) — a switch continues this session \
                 on one of those, and nothing else is a subscription pie can drive",
                crate::error::truncate(backend, 40),
                super::models::known_backends().join(", ")
            )));
        }

        if cli.resume.is_some() {
            return Err(PieError::Config(format!(
                "`--switch {backend}` and `--resume` are mutually exclusive — `--resume` names a \
                 vendor conversation on one backend, and the point of a switch is to leave it. \
                 Drop `--resume` (the conversation is rebuilt from pie's transcript instead), or \
                 drop `--switch` to resume it."
            )));
        }

        if !cli.continue_session && cli.session.is_none() {
            return Err(PieError::Config(format!(
                "`--switch {backend}` needs a session to switch — pass `--continue` to re-bootstrap \
                 the newest session in this directory, or `--session <id>` to name one. A switch \
                 continues a conversation pie already recorded; it is never a fresh one."
            )));
        }

        let model = match cli.model.as_deref() {
            Some(spec) => {
                let (named, model) = provider::parse_model_spec(spec)?;
                if named != backend {
                    return Err(PieError::Config(format!(
                        "`--switch {backend}` and `--model {spec}` name different backends — \
                         `--model` may name the model *on* the new backend, e.g. \
                         `--switch {backend} --model {backend}/<model>`"
                    )));
                }
                Some(model)
            }
            None => None,
        };

        Ok(Some(Self {
            backend: backend.to_string(),
            model,
        }))
    }
}

/// The session a turn is recorded in: the one it continues, or a new one.
///
/// A **switched** turn is recorded under the backend it runs on rather than the one
/// the session was recorded on. That is not bookkeeping: the index frame describes
/// the newest turn, so a frame still naming the old backend would disagree with the
/// row written beneath it — the two are asserted to agree — and `--continue` on the
/// new backend could never find the conversation it now holds, because the id it
/// hands back is one that backend issued.
fn session_for(store: &Path, selection: &Selection, backend: &str, cwd: &Path) -> Session {
    match selection.continuation() {
        // Re-opening keeps the id *and* the transcript it names, so a continued
        // turn lands *in* that conversation instead of beside it.
        Some(continuation) => {
            let provider = if selection.is_switch() {
                backend
            } else {
                continuation.summary.provider.as_str()
            };
            Session::reopen_on_in(store, continuation.summary.clone(), provider)
        }
        None => Session::start_in(store, backend, cwd),
    }
}

// ------------------------------------------------- which conversation, if any --

/// Which conversation a turn continues.
///
/// One concept, four flags: `--resume <ID>` names a *vendor* conversation id,
/// `--continue` and `--session <ID>` name one of *pie's* sessions and get the
/// vendor id out of that session's own transcript, and `--switch <backend>` does
/// the same but **drops** that id, because it names the backend being left.
#[derive(Debug, Clone)]
pub enum Selection {
    /// No selector: a fresh conversation. The default, on purpose.
    Fresh,
    /// `--resume <vendor-id>`: hand the id straight back to the backend.
    Vendor(String),
    /// `--continue` or `--session <id>`: continue one of pie's sessions.
    Session(Continuation),
    /// `--switch <backend>`: continue pie's session on a **different** backend.
    ///
    /// A variant of its own rather than a flag on [`Continuation`]: being a switch
    /// changes the payload (a rebuild, never a resume), the recording (the next
    /// row runs on the new backend) and the retry rule (`RISKS.md` §6 does not
    /// apply, because nothing was resumed). Three behaviours that must not be
    /// settable independently of one another.
    Switch(Continuation),
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
    ///
    /// `None` for a switch even when the transcript recorded one: that id belongs
    /// to the backend being left, and handing it to the new backend would ask it
    /// to continue a conversation it has never heard of (`DESIGN.md` §11).
    pub fn vendor_id(&self) -> Option<&str> {
        match self {
            Selection::Vendor(id) => Some(id),
            Selection::Session(continuation) => continuation.vendor_id.as_deref(),
            Selection::Switch(_) | Selection::Fresh => None,
        }
    }

    /// The session this turn continues, when a selector named one.
    pub fn continuation(&self) -> Option<&Continuation> {
        match self {
            Selection::Session(continuation) | Selection::Switch(continuation) => {
                Some(continuation)
            }
            _ => None,
        }
    }

    /// Whether this selection changes the backend under the session.
    ///
    /// The one selection whose payload is rebuilt instead of resumed, and the one
    /// whose next row is recorded on a different backend than the session's own.
    pub fn is_switch(&self) -> bool {
        matches!(self, Selection::Switch(_))
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
/// * `--switch <backend>` is the **only** case in which the backend is dropped
///   from the match, for `--continue` and `--session` alike: the session being
///   left is on another backend by definition, so requiring the new backend's id
///   would make a switch impossible. The turn is rebuilt from pie's own transcript
///   instead of resumed (`DESIGN.md` §11).
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

    let switching = cli.switch.is_some();

    let summaries = session::list_from(root)?;

    let summary = match cli.session.as_deref() {
        Some(id) => named(&summaries, root, id)?,
        None => newest_session(&summaries, provider, cwd, switching)?,
    };

    if switching {
        return Ok(Selection::Switch(continuation(root, summary)?));
    }

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
/// Returns the rebuild when the selection was a **switch**: the payload is pie's
/// own transcript in that case, and the caller reports what compaction dropped
/// (`ACCEPTANCE.md` §8, stage F). Every other selection returns `None` — a resumed
/// turn sends the task as given, and there is nothing to report about it.
///
/// A session whose transcript recorded no vendor id (every turn in it failed) has
/// no conversation to hand back, so it is seeded from its own rows straight away —
/// the same rebuild as the switch, minus the backend change.
pub fn apply_selection(
    selection: &Selection,
    request: &mut ChatRequest,
    task: &str,
) -> Option<Rebuilt> {
    match selection {
        Selection::Vendor(id) => {
            request.resume = Some(id.clone());
            None
        }
        Selection::Session(continuation) => {
            match &continuation.vendor_id {
                Some(id) => request.resume = Some(id.clone()),
                None => request.prompt = seed_prompt(&continuation.rows, task),
            }
            None
        }
        // A switch never resumes: the id in the transcript belongs to the backend
        // being left (`DESIGN.md` §11), so the new backend is handed the history
        // instead. `resume` is cleared rather than merely not set, so a request
        // that was already pointed at a vendor id cannot carry it across a
        // switch.
        Selection::Switch(continuation) => {
            let rebuilt = rebuild(&continuation.rows, task);
            request.resume = None;
            request.prompt = rebuilt.prompt.clone();
            Some(rebuilt)
        }
        Selection::Fresh => None,
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
    newest_where(summaries, |summary| {
        summary.provider == provider && same_directory(Path::new(&summary.cwd), cwd)
    })
}

/// The newest summary recorded in `cwd`, whatever backend it ran on.
///
/// `--switch`'s filter: the backend is what the turn is *leaving*, so it cannot
/// also be part of the match — requiring it would mean the one case a switch
/// exists for could never select anything.
fn newest_in<'a>(summaries: &'a [SessionSummary], cwd: &Path) -> Option<&'a SessionSummary> {
    newest_where(summaries, |summary| {
        same_directory(Path::new(&summary.cwd), cwd)
    })
}

/// The newest summary the predicate accepts, by `updated_at` and then id.
///
/// The tie-break on `id` is what keeps the answer deterministic when two sessions
/// share a millisecond, which is exactly what the seeded fixtures do.
fn newest_where(
    summaries: &[SessionSummary],
    keep: impl Fn(&SessionSummary) -> bool,
) -> Option<&SessionSummary> {
    summaries.iter().filter(|s| keep(s)).max_by(|a, b| {
        a.updated_at
            .cmp(&b.updated_at)
            .then_with(|| a.id.cmp(&b.id))
    })
}

/// The session `--continue` continues, or the error that says why none was found.
///
/// The two messages are not interchangeable. Without `--switch` the backend is
/// part of the match, so the error can name both it and the directory, and the
/// user's fix is a `--model` on the backend the session ran on. With `--switch`
/// the backend was deliberately dropped from the match, so naming it would send the
/// user looking for the wrong thing; naming the directory is all that is honest.
fn newest_session(
    summaries: &[SessionSummary],
    provider: &str,
    cwd: &Path,
    switching: bool,
) -> Result<SessionSummary> {
    let newest = match switching {
        true => newest_in(summaries, cwd),
        false => newest_for(summaries, provider, cwd),
    };
    if let Some(summary) = newest {
        return Ok(summary.clone());
    }

    Err(match switching {
        true => PieError::Config(format!(
            "no session to continue in {} — `--switch` re-sends a session pie already recorded, \
             and there is none here, so there is nothing to switch. `pie sessions` lists what \
             exists, and a plain `pie -p` starts a fresh conversation.",
            cwd.display()
        )),
        false => PieError::Config(format!(
            "no session to continue for `{provider}` in {} — a conversation is continued in the \
             directory and on the backend it ran in. `pie sessions` lists what exists, and a \
             plain `pie -p` starts a fresh one",
            cwd.display()
        )),
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

/// The line a rebuilt prompt opens with.
///
/// It says what the payload *is*: not the backend's own context but a
/// reconstruction pie is sending, so a replayed history is not misread as
/// something the model remembers. Kept from stage B, where a re-bootstrap was the
/// only caller of a rebuild; a switch needs it for the stronger reason, that the
/// conversation it re-sends was never this backend's at all.
pub const REBUILD_NOTE: &str = "pie is re-sending the earlier part of this session, because the \
                                 backend does not have the conversation.";

/// An outgoing prompt rebuilt from a session's own transcript, and what building
/// it had to elide.
///
/// The counts are part of the value rather than a side channel: compaction must
/// never be silent, because the turns it drops from the payload are turns the user
/// paid for (`DESIGN.md` §11).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rebuilt {
    /// The payload: the compacted history, then the task text.
    pub prompt: String,
    /// Turns the transcript held.
    pub turns: usize,
    /// Turns sent verbatim.
    pub kept: usize,
    /// Turns the state block stands in for.
    pub dropped: usize,
}

/// Rebuild an outgoing prompt from a session's own transcript.
///
/// Used when there is no conversation to hand back: the vendor id is gone
/// (`RISKS.md` §6), the transcript never recorded one, or the backend is being left
/// behind entirely (`--switch`, `DESIGN.md` §11). **One** function for both, so the
/// two paths cannot send different shapes for the same session.
///
/// The *order* is [`compact::assemble`]'s, never this function's — persona, head
/// turns, state block, tail turns, task — because it is a caching rule rather than
/// tidiness (`TOKEN-ECONOMICS.md` §4): the volatile task text goes last and nothing
/// volatile may precede something stable. Re-joining the pieces here is exactly how
/// that order would drift.
///
/// The persona is passed as `None` on purpose: agy and codex fold it in front of
/// the prompt themselves and claude receives it in `--append-system-prompt`, so
/// folding it here as well would send it twice.
///
/// `TurnRow::prompt` is the user's own text and never the payload that was sent,
/// so seeding a payload that already contains a transcript cannot nest history
/// inside history. The argv cap (`DESIGN.md` §1) still reports a rebuilt prompt that
/// has outgrown a backend, rather than silently dropping history here.
pub fn rebuild(rows: &[TurnRow], task: &str) -> Rebuilt {
    let compacted = compact::compact(rows, rebuild_budget(rows.len()));
    let body = compact::assemble(None, &compacted, task);

    // The note only appears when there is something to re-send: with an empty
    // transcript the payload is the task text alone — the bytes a fresh turn sends
    // — and announcing a replay that did not happen would be a lie inside the
    // payload itself.
    let prompt = if compacted.is_empty() {
        body
    } else {
        format!("{REBUILD_NOTE}\n\n{body}")
    };

    Rebuilt {
        prompt,
        turns: compacted.turns(),
        kept: compacted.kept(),
        dropped: compacted.elided(),
    }
}

/// The budget a rebuilt transcript is sent under.
///
/// `DESIGN.md` §11: *past* `agent.compact_after_turns` turns older turns collapse
/// into a state block; below it the whole transcript is replayed. The trigger and the
/// budget are two separate decisions — [`compact::is_due`] says whether to compact,
/// and the budget says what survives when it does — so a session either side of the
/// threshold can be asserted against the same one composition.
///
/// `config.rs` has no `agent.compact_after_turns` field, so the contract's default is
/// used; it is the one number here that belongs to configuration.
fn rebuild_budget(turns: usize) -> compact::Budget {
    if compact::is_due(turns, compact::DEFAULT_COMPACT_AFTER_TURNS) {
        compact::Budget::default()
    } else {
        // Head = every turn, tail = none: nothing is elided and the block comes back
        // empty, so a short transcript is replayed whole through the same
        // composition rather than through a second code path.
        compact::Budget::new(turns, 0)
    }
}

/// Render a session's recorded turns into the prompt that re-bootstraps it.
///
/// [`rebuild`]'s text, for the callers that do not report what compaction dropped.
/// It takes the rows rather than a session so a caller can hand it whatever window it
/// decided on.
pub fn seed_prompt(rows: &[TurnRow], task: &str) -> String {
    rebuild(rows, task).prompt
}

/// What a switch says on stderr: which backend changed, and what the rebuild had to
/// elide.
///
/// The count is the point. A compaction the user cannot see is history disappearing
/// from a payload they paid for, so the number is reported on every switch rather
/// than only when it is surprising — a quiet "all turns re-sent" is evidence that the
/// session is still being replayed whole.
pub fn switch_notice(from: &str, to: &str, rebuilt: &Rebuilt) -> String {
    let payload = if rebuilt.dropped == 0 {
        format!("all {} turn(s) re-sent", rebuilt.turns)
    } else {
        format!(
            "{} of {} turns compacted, {} re-sent",
            rebuilt.dropped, rebuilt.turns, rebuilt.kept
        )
    };
    format!("[switch] {from} -> {to}: {payload} from pie's transcript")
}

/// The prompt a failed resume is replaced by, when the failure means the backend
/// no longer has the conversation (`RISKS.md` §6).
///
/// The decision and the rebuild are one function on purpose: a retry exists only
/// where there is a history to send instead of resuming, so the two cannot drift
/// apart. `None` means "report the failure" — either nothing was being resumed,
/// the selection was not a session **resume** (a switch deliberately handed over no
/// vendor id, and `Fresh` and `Vendor` have no transcript to send), the session had
/// no vendor id to lose, the attempt already produced answer text (so whatever
/// failed, it was not an empty conversation), or the message is not about a missing
/// conversation at all.
///
/// Callers retry **once**: this says whether a fresh attempt is warranted, never
/// how many.
pub fn rebootstrap_prompt(
    selection: &Selection,
    error: &PieError,
    streamed: bool,
    task: &str,
) -> Option<String> {
    // Only a session resume has an id that can go stale. A switch resumed nothing,
    // so a missing conversation cannot be about pie's own id — retrying it would
    // spend the new backend's quota a second time on a payload that never changed,
    // and report the retry's outcome instead of the real error.
    let continuation = match selection {
        Selection::Session(continuation) => continuation,
        _ => return None,
    };
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
