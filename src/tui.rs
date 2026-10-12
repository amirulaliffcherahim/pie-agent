//! The interactive TUI (`DESIGN.md` §12).
//!
//! Three layers, split so that the one part which cannot be automated stays as
//! small as possible:
//!
//! | Layer | Testable how |
//! |---|---|
//! | [`app`] — reducer and state | unit tests: a neutral [`app::Input`] reaches no crossterm type, so `key → Action → state` is asserted without a terminal |
//! | [`render`] | ratatui's `TestBackend`: draw into an in-memory buffer and assert its contents |
//! | this module's [`run_session`] | **not automatable.** Kept thin on purpose: crossterm setup, a select loop, and teardown — nothing else |
//!
//! ## Two constraints that shape the behaviour
//!
//! * **Delegate mode means the UI shows the vendor's tool steps, not pie's.**
//!   `agy --print` and `codex exec` run their own tools (`RISKS.md` §1), so this
//!   is architecturally unlike a TUI that displays its own tool cards, and the UI
//!   says so ([`app::VENDOR_TOOL_NOTE`]) rather than implying a capability pie
//!   does not have.
//! * **The UI never blocks on a turn.** A turn runs as a task and pushes events
//!   down an `mpsc` channel; the loop selects over that channel and the terminal's
//!   events. A synchronous turn would freeze the interface until the vendor CLI
//!   returned — up to ~40 s on agy.
//!
//! ## Entry point
//!
//! `pie tui`, opt-in. A bare `pie` keeps printing help, so CI and every existing
//! test are unaffected and the TUI can never be entered by accident from a script.
//!
//! ## What this file adds, and what its gate can honestly prove
//!
//! [`run_session`] is the one part of M6 no test can drive, so every decision
//! inside it is a call into a function of this module, and `tests/tui_loop.rs`
//! drives those directly. The turn half is **injectable** — [`stream_turn`] takes
//! the provider it runs — which is what lets the event ordering, the "exactly one
//! `Finished`" rule and the recording be asserted with no vendor CLI, no network
//! and no terminal.
//!
//! Three lines of `run_session` remain outside every test, and are named here so
//! they cannot be mistaken for verified behaviour: the `tokio::spawn` that starts a
//! turn, the `tokio::spawn` that fetches a model catalog, and the `handle.abort()`
//! that cancels one. Each is a single expression whose *arguments* and whose
//! decision to take that branch are covered; the spawn itself is not.

pub mod app;
pub mod render;

pub use app::VENDOR_TOOL_NOTE;
pub use app::{apply_event, reduce, Action, App, Entry, Input, Picker, PickerItem, Status};
pub use render::render;

use std::io::IsTerminal;
use std::path::Path;
use std::time::Instant;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{cursor, execute};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::mpsc;

use crate::config::Config;
use crate::error::{truncate, PieError, Result};
use crate::paths;
use crate::profile::Profile;
use crate::provider::{self, ChatRequest, Provider, StreamEvent, TurnOutcome};
use crate::session::{Session, SessionSummary, TurnRow};
use crate::session_log::{self, now_millis, SessionLogRow};

// ------------------------------------------------------------- the turn ------

/// One thing an in-flight turn produced.
///
/// A struct-of-events rather than a bare `StreamEvent`, because the loop has to
/// hear about the *end* of a turn as well as its events, and the ending is not a
/// `StreamEvent`: it is a `Result<TurnOutcome>` the vendor CLIs only produce once
/// the child has exited.
///
/// Not `Clone`: the error half is a [`PieError`], which owns a `std::io::Error` and
/// is deliberately not clonable — a turn's failure is reported once, by the loop that
/// saw it.
#[derive(Debug)]
pub enum TurnEvent {
    /// One event from the backend's stream, forwarded as it arrived.
    Stream(StreamEvent),
    /// The turn ended, successfully or not.
    ///
    /// Exactly one of these per turn, on every path: a turn the loop started and
    /// never heard the end of would leave the UI in [`Status::Running`] for ever,
    /// with the interface waiting on a spinner `reduce` only cancels — the failure
    /// mode [`stream_turn`] exists to make impossible.
    Finished(Result<TurnOutcome>),
}

/// What folding one turn event did.
///
/// The outcome is handed *back* rather than kept inside the app, because the row a
/// finished turn is recorded as needs what the transcript does not hold — the
/// vendor conversation id, the usage and the status (`DESIGN.md` §11) — and this is
/// the last point at which the outcome exists.
#[derive(Debug)]
pub enum Folded {
    /// The event was folded into the transcript; the turn continues.
    Streaming,
    /// The turn ended, and here is how.
    Ended(Result<TurnOutcome>),
}

/// Fold one turn event into the app.
///
/// The mapping from a [`StreamEvent`] to the app is **not** here: it lives in
/// [`app::apply_event`] and is called, never reimplemented, because two copies of
/// it would drift and this is the only consumer that would notice (`DESIGN.md`
/// §12).
///
/// A `Finished` does two things the app cannot do for itself.
///
/// * It **sets the status**. `Done`/`Failed` is the loop's own contract with the
///   status line, and it is set on the ending rather than on the last streamed
///   event for the plain reason that a turn can fail without ever producing one.
///   The reason is truncated because a backend's diagnostic can run to hundreds of
///   characters and the status line is a single line.
/// * It folds the outcome's **usage** through the same merge a streamed `Usage`
///   goes through. That is not belt-and-braces: no backend in this crate ever emits
///   a `StreamEvent::Usage` (grep: only `Done` is constructed in
///   `src/provider/agy.rs` and `src/provider/codex.rs`), so this call is the only
///   path by which [`App::last_usage`] is ever filled, and a status line without it
///   would report that a turn cost nothing.
///
/// The `Done` event is **not** re-synthesised from the outcome. Every backend that
/// returns `Ok` emitted one (`DESIGN.md` §6 rule 7 makes a missing terminal event an
/// error), so a second one would only add the risk of the answer landing in the
/// transcript twice.
pub fn apply_turn_event(app: &mut App, event: TurnEvent) -> Folded {
    match event {
        TurnEvent::Stream(raw) => {
            apply_event(app, &raw);
            Folded::Streaming
        }
        TurnEvent::Finished(Ok(outcome)) => {
            apply_event(app, &StreamEvent::Usage(outcome.usage.clone()));
            app.status = Status::Done;
            Folded::Ended(Ok(outcome))
        }
        TurnEvent::Finished(Err(error)) => {
            app.status = Status::Failed(truncate(&error.to_string(), 160));
            Folded::Ended(Err(error))
        }
    }
}

/// Run one turn on `backend`, pushing everything it produces down `tx`.
///
/// This is the task the loop spawns (`DESIGN.md` §12: the turn runs as a task and
/// pushes events down an `mpsc` channel), which is why every argument is owned — a
/// borrowed `&Config` could not cross the spawn boundary.
///
/// An unknown backend is answered as a failed turn rather than as an error the
/// caller has to handle: the loop has already committed to a turn (the prompt is
/// gone from `App::prompt`), so the only honest thing left is to end it with a
/// reason. Nothing is spawned on that path.
pub async fn run_turn(
    cfg: Config,
    backend: String,
    request: ChatRequest,
    tx: mpsc::UnboundedSender<TurnEvent>,
) {
    let Some(provider) = provider::by_id(&backend) else {
        let _ = tx.send(TurnEvent::Finished(Err(PieError::UnknownBackend {
            backend,
            known: crate::commands::models::known_backends(),
        })));
        return;
    };

    stream_turn(cfg, provider, request, tx).await;
}

/// [`run_turn`]'s other half: an already-resolved provider.
///
/// Split out so the turn half is injectable, which is the whole reason
/// `tests/tui_loop.rs` can prove the ordering and the ending without a vendor CLI.
/// Nothing here touches the terminal, the app or the store.
pub async fn stream_turn(
    cfg: Config,
    provider: Box<dyn Provider>,
    request: ChatRequest,
    tx: mpsc::UnboundedSender<TurnEvent>,
) {
    let outcome = {
        let mut sink = |event: StreamEvent| {
            // A send failure means the loop has gone — the TUI was quit while the
            // turn ran. The turn is allowed to finish and its output is dropped:
            // there is nothing left to draw and the child still has to be reaped.
            let _ = tx.send(TurnEvent::Stream(event));
        };
        provider.stream(&cfg, &request, &mut sink).await
    };

    // Sent on *every* path, including a failure that produced no event at all —
    // which is exactly the case a naive `?` here would swallow.
    let _ = tx.send(TurnEvent::Finished(outcome));
}

// --------------------------------------------------------- the loop's view ---

/// One thing the loop has to react to.
///
/// The keyboard, the turn in flight and a catalog being fetched are one queue
/// rather than three loops, because the loop must service them in an order it can
/// be reasoned about — and because a drained-to-completion turn channel is the
/// freeze [`next_step`] exists to prevent.
#[derive(Debug)]
pub enum Step {
    /// A key the user pressed.
    Key(Input),
    /// Something the in-flight turn did.
    Turn(TurnEvent),
    /// Rows for the model picker, from a catalog read that finished.
    Catalog(Vec<PickerItem>),
}

/// What the loop must do that [`act`] cannot.
///
/// The same split as [`Action`], one level up: state changes happen in [`act`]
/// (which decides in terms of [`reduce`] and [`app::apply_event`]), and an effect is
/// only the part that needs a spawn, an abort, an await or a `break`. `Run` carries
/// the prompt rather than a built request because building it reads `profile.toml`,
/// which the loop re-reads per turn — and because the prompt is then what the loop
/// acts on, rather than a value it has to go looking for in [`InFlight`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// Nothing left to do for this step.
    Continue,
    /// Leave the TUI.
    Quit,
    /// Run this prompt as a turn.
    Run(String),
    /// Abort the turn in flight.
    Abort,
    /// Read the model catalogs for the picker.
    Catalog,
}

/// The next thing the loop has to react to, from any of its sources.
///
/// Selecting over all three receivers *is* "the UI never blocks on a turn"
/// (`DESIGN.md` §12): a loop that drained the turn channel before looking at the
/// keyboard would freeze the interface for the length of a turn (up to ~40 s on
/// agy), which is the behaviour `cargo test --test tui_loop` exists to catch.
///
/// The select is `biased` with the keyboard first, deliberately. The turn is the
/// only unbounded producer here — a long answer arrives as thousands of deltas —
/// while a human types a few keys a second, so servicing keys first is what keeps
/// the loop responsive without starving the turn. The default unbiased select picks
/// at random among ready branches, which would make the servicing order vary from
/// run to run: a behaviour no test can pin down is a behaviour nobody can rely on.
///
/// A closed channel ends the loop rather than being skipped. The keyboard channel is
/// the one that can close while the loop runs (this function's caller holds the
/// other senders), and it closes exactly when the terminal stopped delivering
/// events — a session nobody can type into, which must not sit there looking alive.
pub async fn next_step(
    keys: &mut mpsc::UnboundedReceiver<Input>,
    turns: &mut mpsc::UnboundedReceiver<TurnEvent>,
    catalog: &mut mpsc::UnboundedReceiver<Vec<PickerItem>>,
) -> Option<Step> {
    tokio::select! {
        biased;
        input = keys.recv() => input.map(Step::Key),
        event = turns.recv() => event.map(Step::Turn),
        items = catalog.recv() => items.map(Step::Catalog),
    }
}

/// The [`Input`] a terminal event carries, when it carries one.
///
/// Everything the frozen keymap has no row for — a resize, a mouse event, a
/// function key — is `None` rather than an error: a terminal can always send
/// something the contract never mentions, and refusing to draw because of it would
/// be the UI failing over protocol noise.
pub fn input_for(event: &Event) -> Option<Input> {
    let Event::Key(key) = event else {
        return None;
    };
    input_from_key(*key)
}

/// The [`Input`] a key carries, when the frozen keymap has a row for it
/// (`DESIGN.md` §12).
///
/// `Release` is dropped. crossterm reports a *release* as well as a press on
/// Windows, so passing both through would double every keystroke — and a doubled
/// `Enter` is two turns, which is real subscription quota. `Repeat` is kept,
/// because a held `Backspace` is a keystroke the user asked for repeatedly.
///
/// Ctrl is lower-cased here because [`Input::Ctrl`] is documented as
/// "lower-cased by the caller": without it, the quit key would depend on whether
/// the keyboard's shift state happened to be set while Ctrl was held.
pub fn input_from_key(key: KeyEvent) -> Option<Input> {
    if key.kind == KeyEventKind::Release {
        return None;
    }

    if key.modifiers.contains(KeyModifiers::CONTROL) {
        return match key.code {
            KeyCode::Char(c) => Some(Input::Ctrl(c.to_ascii_lowercase())),
            _ => None,
        };
    }

    match key.code {
        KeyCode::Char(c) => Some(Input::Char(c)),
        KeyCode::Enter => Some(Input::Enter),
        KeyCode::Esc => Some(Input::Esc),
        KeyCode::Backspace => Some(Input::Backspace),
        KeyCode::Up => Some(Input::Up),
        KeyCode::Down => Some(Input::Down),
        KeyCode::Left => Some(Input::Left),
        KeyCode::Right => Some(Input::Right),
        KeyCode::Tab => Some(Input::Tab),
        _ => None,
    }
}

/// The loop's own per-turn bookkeeping.
///
/// [`App`] cannot hold it: the reducer clears [`App::prompt`] as part of accepting
/// a submission, and the frozen row's `duration_ms` is a wall clock the frozen
/// `App` has no field for. The loop therefore keeps both, and [`record_turn`] reads
/// them.
#[derive(Debug, Default)]
pub struct InFlight {
    /// The prompt the running turn was submitted with — the **user's own text**,
    /// never the payload that was sent. `commands/print.rs` records the same field
    /// for the same reason: a seeded payload would make the next rebuild seed a
    /// seed, and the history would nest inside itself.
    pub prompt: String,
    /// When the turn was submitted. The frozen log counts the wall clock the user
    /// actually waited, not the time the child was alive.
    pub started: Option<Instant>,
}

impl InFlight {
    /// Note a submission: the prompt, and the clock the row is measured from.
    pub fn begin(&mut self, prompt: String) {
        self.prompt = prompt;
        self.started = Some(Instant::now());
    }

    /// Wall clock since [`Self::begin`], in milliseconds.
    ///
    /// `0` when nothing was submitted, which the frozen schema reads as a
    /// zero-length attempt: the row has no null, and an invented duration would be
    /// indistinguishable from a measured one.
    pub fn elapsed_ms(&self) -> u64 {
        self.started
            .map(|started| started.elapsed().as_millis() as u64)
            .unwrap_or(0)
    }

    /// Forget the finished turn.
    ///
    /// Called when a turn ends so that a prompt cannot be recorded twice: a second
    /// `Finished` (a bug, or a task that somehow outlives its own ending) records an
    /// empty prompt rather than attributing the old one to a turn the user never
    /// asked for.
    pub fn clear(&mut self) {
        self.prompt.clear();
        self.started = None;
    }
}

/// Everything the loop does with one step that is not I/O.
///
/// The decision function: a key becomes an [`Effect`] through [`reduce`], a turn
/// event is folded through [`apply_turn_event`] and recorded when it ends, and a
/// catalog that has arrived is only taken if the picker is still open. Kept free of
/// the runtime so it can be asserted directly.
pub fn act(app: &mut App, session: &mut Session, in_flight: &mut InFlight, step: Step) -> Effect {
    match step {
        Step::Key(input) => act_on_key(app, in_flight, input),
        Step::Turn(event) => act_on_turn(app, session, in_flight, event),
        Step::Catalog(items) => act_on_catalog(app, items),
    }
}

/// One row of the frozen keymap (`DESIGN.md` §12).
///
/// The submittability rule is deliberately **not** repeated here: asking "is this
/// submittable?" in two places is how the reducer's rule and the turn's would drift
/// apart, so the text is simply captured and whether it is used is [`reduce`]'s
/// answer.
fn act_on_key(app: &mut App, in_flight: &mut InFlight, input: Input) -> Effect {
    // Read *before* `reduce`: accepting a submission clears `App::prompt`, and the
    // action that says so comes out of the same call, so afterwards the text the
    // turn is sent with is nowhere. Only `Enter` is worth a copy.
    let typed = match input {
        Input::Enter => app.prompt.clone(),
        _ => String::new(),
    };

    let effect = effect_for(reduce(app, input), typed);

    // The clock starts when the turn is dispatched, not when the key was read, and
    // the prompt is kept here as well as carried by the effect because the row is
    // written after the turn ends — by which time `App::prompt` has long been
    // cleared.
    match effect {
        Effect::Run(prompt) => {
            in_flight.begin(prompt.clone());
            Effect::Run(prompt)
        }
        other => other,
    }
}

/// What the loop must do about an [`Action`] the reducer returned.
///
/// A mapping of its own rather than a `match` inside the step handler, because it is
/// the one part of the loop that could not otherwise be reached: `app::reduce` is
/// stage A's file and this stage may not wait on it, so written this way
/// `cargo test --test tui_loop` asserts every row of the mapping directly. The match
/// is exhaustive on purpose — a future `Action` variant must not fall through a `_`
/// arm into `Continue`, which is the shape in which a new keybinding does nothing and
/// nothing says so.
///
/// `submitted` is only consulted for [`Action::Submit`]: the reducer has already
/// cleared the prompt by then, so the caller hands the text over instead.
pub fn effect_for(action: Action, submitted: String) -> Effect {
    match action {
        Action::Quit => Effect::Quit,
        Action::Submit => Effect::Run(submitted),
        Action::Cancel => Effect::Abort,
        Action::OpenPicker => Effect::Catalog,
        // State changes only: these have already happened inside `reduce`.
        Action::None
        | Action::ScrollUp
        | Action::ScrollDown
        | Action::PickerNext
        | Action::PickerPrev
        | Action::PickerAccept => Effect::Continue,
    }
}

/// Fold one turn event, and record the turn the moment it ends.
fn act_on_turn(
    app: &mut App,
    session: &mut Session,
    in_flight: &mut InFlight,
    event: TurnEvent,
) -> Effect {
    match apply_turn_event(app, event) {
        Folded::Streaming => Effect::Continue,
        Folded::Ended(outcome) => {
            // Recorded here, in the one place that holds the outcome *and* the
            // prompt *and* the clock. This is the reason the TUI records at all:
            // otherwise the interactive turns are the ones missing from the log
            // that `pie usage` totals and M7's "know me" job will mine.
            record_turn(session, app, in_flight, &outcome);
            in_flight.clear();
            Effect::Continue
        }
    }
}

/// Take the catalog rows, if the picker is still open.
///
/// A late result must not reopen an overlay the user has already dismissed: the
/// catalog read is spawned, so `Esc` can legitimately arrive while it is in flight.
fn act_on_catalog(app: &mut App, items: Vec<PickerItem>) -> Effect {
    if app.picker.is_some() {
        app.picker = Some(Picker { items, selected: 0 });
    }
    Effect::Continue
}

// ------------------------------------------------------- what a turn runs ----

/// The request one TUI turn runs.
///
/// The same fields `commands/print.rs` fills for `pie -p`, because M3's Done
/// condition is that every backend receives the identical persona — *"the persona
/// agy receives and the persona codex receives are the identical text"* — and a TUI
/// that folded a different projection would quietly falsify it. The persona comes
/// from [`Profile::persona`], the same accessor the one-shot path uses, so "no
/// persona configured" stays `None` and can never become `Some("")`.
///
/// `effort` and `timeout` are deliberately left unset: the frozen keymap has no key
/// for either, so a backend that needs an effort value gets its own configured
/// default from its own argv builder (`agy.default_effort`), and the deadline is the
/// resolved configuration (`req.timeout.unwrap_or_else(|| cfg.timeout())`).
pub fn build_request(app: &App, prompt: &str, profile: &Profile) -> ChatRequest {
    let mut request = ChatRequest::new(prompt);
    request.model = app.model.clone();
    request.persona = profile.persona().map(str::to_string);
    request
}

/// The turn a submitted prompt runs, or `None` when it cannot be built.
///
/// The profile is read per turn by the caller rather than once per process, because
/// a TUI outlives the file: a persona edited while it is open takes effect on the
/// next turn. It is read *before* anything is spawned, which is the order
/// `commands/print.rs` keeps so that a broken `profile.toml` costs a turn nothing —
/// the same order is why the failure is shown here instead of being returned: the
/// submission has already been accepted, and the status line is the only place left
/// to say so.
pub fn submitted_turn(
    app: &mut App,
    prompt: &str,
    profile: Result<Profile>,
) -> Option<ChatRequest> {
    match profile {
        Ok(profile) => Some(build_request(app, prompt, &profile)),
        Err(error) => {
            app.status = Status::Failed(truncate(&error.to_string(), 160));
            None
        }
    }
}

/// Read every backend's catalog and hand the rows back to the loop.
///
/// A task of its own, not a call inside the loop: the catalogs come from the vendor
/// CLIs and are slow on a cache miss (agy's probe costs ~10 s, `knowledge/agy.md`),
/// and freezing the interface for the consequence of one keystroke is the same
/// defect `DESIGN.md` §12 forbids for turns.
///
/// A backend whose catalog cannot be read contributes no rows and says nothing:
/// this draws a picker, and `pie models` is where a catalog that failed is
/// explained.
///
/// Untested by construction, and the only function in this module that is: it calls
/// every registered backend's own catalog path, which on a cache miss is a vendor
/// CLI (`tests/tui_loop.rs` may not spawn one). It builds no label of its own — every
/// row comes from [`Picker::from_models`], the one label authority — which is what
/// keeps a merged multi-backend list able to say which subscription each row selects
/// even though this function is the only caller that merges. How the rows are
/// delivered is [`Step::Catalog`].
async fn fill_picker(cfg: Config, tx: mpsc::UnboundedSender<Vec<PickerItem>>) {
    let mut items = Vec::new();
    for provider in provider::registry() {
        let Ok(models) = provider.list_models(&cfg).await else {
            continue;
        };
        // Merged in place, backend by backend: two rows for the same model id on two
        // different subscriptions must stay tellable apart, and only the backend in
        // the label says which one a row switches to (`DESIGN.md` §12, `PickerItem`).
        items.extend(Picker::from_models(provider.id(), &models).items);
    }
    let _ = tx.send(items);
}

// ---------------------------------------------------------- recording --------

/// Record a finished turn exactly as `commands/print.rs` records a `pie -p` turn.
///
/// Two writes, both **best-effort**, in the same order and with the same fields as
/// the one-shot path: the frozen `usage.jsonl` row says what the turn cost, and the
/// session store says what was said (`DESIGN.md` §7, §11). A TUI turn that skipped
/// this would be a turn where quota was spent and nothing was learned — missing from
/// `pie usage` and from the sessions M7's "know me" job is meant to mine, with
/// nothing to say so.
///
/// A write failure never fails the turn. `record_best_effort` logs and returns, and
/// the caller has nothing to return an error to: the answer is already on screen and
/// the user paid for it.
///
/// The prompt recorded is [`InFlight::prompt`] — the user's own text, never a
/// rebuilt payload — and the model is empty when the backend picked its own, because
/// the frozen row schema has no null and an invented model id would be a lie in the
/// one file an outside implementation reads.
pub fn record_turn(
    session: &mut Session,
    app: &App,
    in_flight: &InFlight,
    outcome: &Result<TurnOutcome>,
) {
    let model = app.model.as_deref().unwrap_or_default();
    let elapsed_ms = in_flight.elapsed_ms();
    let prompt = in_flight.prompt.as_str();

    match outcome {
        Ok(turn) => {
            session_log::append_best_effort(&SessionLogRow::from_turn(
                &app.backend,
                model,
                &turn.usage,
                elapsed_ms,
                &turn.status,
            ));
            session.record_best_effort(&TurnRow::from_turn(&app.backend, model, prompt, turn));
        }
        Err(error) => {
            // Recorded even when it failed: a timeout still costs wall clock, and
            // what was asked when nothing came back is the half of a session a later
            // resume needs most.
            session_log::append_best_effort(&SessionLogRow::from_error(
                &app.backend,
                model,
                error.kind(),
                elapsed_ms,
            ));
            session.record_best_effort(&TurnRow::from_error(
                &app.backend,
                model,
                prompt,
                error.kind(),
            ));
        }
    }
}

// ------------------------------------------------------------ the session ----

/// The directory a TUI session ran in, as the store records it.
///
/// One function for both ends of the session — `crate::commands::tui` mints the id,
/// [`session_for`] rebuilds the handle — because `--continue` matches a session by
/// provider **and** directory (`DESIGN.md` §11). Two spellings of the same directory
/// would make a TUI session unfindable from the command line.
///
/// The TUI has no `--cwd`, so this is the directory pie was started in, which is
/// also the directory a turn's child process runs in ([`ChatRequest::new`]).
pub fn session_directory() -> String {
    std::env::current_dir()
        .map(|dir| dir.display().to_string())
        .unwrap_or_else(|_| ".".to_string())
}

/// Mint the session a TUI run records its turns in.
///
/// Minted by the caller of [`run_session`] (so the id is in [`App::session`] before
/// the first turn) and *not* on disk: a session leaves no trace until a turn is
/// recorded, which is what keeps a TUI that was opened and quit immediately from
/// appearing in `pie sessions`.
pub fn begin_session(backend: &str) -> Session {
    Session::start_in(
        paths::sessions_dir(),
        backend,
        Path::new(&session_directory()),
    )
}

/// The store handle for the session the status line names.
///
/// [`run_session`] receives only `&mut App` (frozen signature), so the handle cannot
/// be passed in and is rebuilt from [`App::session`] instead of minted again: a
/// second `begin_session` here would mint a *different* id, and every turn would be
/// recorded in a session the status line does not name — which is precisely the
/// conversation a later `--resume` would fail to find (`DESIGN.md` §11).
///
/// `started_at` is now, because this process is what starts the session: the id was
/// minted moments ago and nothing has been recorded under it yet, so the first
/// `record` takes the frame's `updated_at` from the row it writes.
///
/// Public so `tests/tui_loop.rs` can assert the property that matters: the handle the
/// loop records through names the same transcript as the id the status line shows.
pub fn session_for(app: &App) -> Session {
    Session::reopen_in(
        paths::sessions_dir(),
        SessionSummary {
            id: app.session.clone(),
            provider: app.backend.clone(),
            cwd: session_directory(),
            started_at: now_millis(),
            ..SessionSummary::default()
        },
    )
}

// ----------------------------------------------------------- the terminal ----

/// Refuse to start unless **both** stdin and stdout are terminals.
///
/// Takes the two facts rather than reading them, so the refusal is a function a test
/// can drive; [`run_session`] is the only caller and it passes
/// `stdin().is_terminal()` / `stdout().is_terminal()`.
///
/// Without the check, `pie tui` inside a pipe or a CI job writes escape sequences
/// into that pipe as its output (`ACCEPTANCE.md` §9) and then waits for a keystroke
/// that will never come, which looks like a hang rather than a refusal. Exit 2,
/// because "you ran this somewhere it cannot run" is a usage problem, which is what
/// that code means (`DESIGN.md` §5).
pub fn ensure_terminal(stdin_is_terminal: bool, stdout_is_terminal: bool) -> Result<()> {
    if stdin_is_terminal && stdout_is_terminal {
        return Ok(());
    }

    let mut missing: Vec<&str> = Vec::new();
    if !stdin_is_terminal {
        missing.push("stdin");
    }
    if !stdout_is_terminal {
        missing.push("stdout");
    }

    // The subject is spelled out per case rather than interpolated as a list: the
    // first version substituted `stdin and stdout` into a singular clause and
    // produced "...and stdin and stdout is not one", which is the kind of sentence
    // a reader distrusts the rest of the message for. Three phrasings is cheaper
    // than one that only reads correctly when a single stream is missing.
    let subject = match missing.as_slice() {
        ["stdin"] => "stdin is not a terminal".to_string(),
        ["stdout"] => "stdout is not a terminal".to_string(),
        _ => "neither stdin nor stdout is a terminal".to_string(),
    };

    Err(PieError::Config(format!(
        "`pie tui` needs a terminal on stdin and on stdout, but {subject}. A full-screen \
         interface would emit escape codes into that stream and then wait for a keystroke it \
         can never receive. Run it in a terminal, or use `pie -p <PROMPT>` for a single turn \
         whose output can be piped."
    )))
}

/// Puts the terminal back on the way out — the `?` path included.
///
/// The teardown cannot be the last statement of [`run_session`]: raw mode and the
/// alternate screen are both entered several statements before the loop, so an early
/// return would leave a terminal that does not echo, on a screen that is not the
/// shell's, with the error message itself invisible. It is armed *before* either
/// change, so a failure halfway through the setup is undone too.
///
/// The cursor is shown again because ratatui hides it whenever a frame does not place
/// it, so a TUI that exits mid-turn can otherwise leave the shell with an invisible
/// cursor.
#[derive(Debug)]
struct Restored;

impl Drop for Restored {
    /// Best-effort, and each step `let _`: a failure to restore is not something the
    /// caller can act on, and a panicking `Drop` while unwinding would abort the
    /// process and take the real error with it.
    fn drop(&mut self) {
        let mut stdout = std::io::stdout();
        let _ = execute!(stdout, LeaveAlternateScreen, cursor::Show);
        let _ = terminal::disable_raw_mode();
    }
}

/// Feed the loop every key the user presses, from a thread of its own.
///
/// crossterm's `EventStream` is the obvious source and cannot be used here: awaiting
/// it needs `futures_core::Stream`, which reaches this crate only as a *transitive*
/// dependency of crossterm's `event-stream` feature — not something a module can
/// name, and `Cargo.toml` is not this stage's to change. `event::read()` on a
/// dedicated thread is the same source of events with the same non-blocking hand-off
/// to the select loop, which is the property that matters (`Cargo.toml` rejects the
/// third option, polling `event::poll` on a timer, for waking the process ~33 times a
/// second to discover nothing happened).
///
/// The thread is detached on purpose: `event::read()` returns only when a key
/// arrives, so joining it during teardown would block until the user pressed
/// something. It stops as soon as the loop drops the receiver.
fn spawn_key_reader(tx: mpsc::UnboundedSender<Input>) {
    std::thread::spawn(move || {
        while let Ok(event) = event::read() {
            let Some(input) = input_for(&event) else {
                continue;
            };
            if tx.send(input).is_err() {
                // The loop is gone; stop reading the terminal.
                break;
            }
        }
    });
}

/// Run the interactive session until the user quits.
///
/// Frozen signature (`DESIGN.md` §12). This is the only part of M6 that no test
/// can reach, so it must contain nothing worth testing:
///
/// 1. refuse to start unless stdout **and** stdin are terminals, so `pie tui` in
///    a pipe fails with a clear message instead of emitting escape codes into it
///    (`ACCEPTANCE.md` §9);
/// 2. put the terminal into raw mode and enter the alternate screen;
/// 3. loop: select over the terminal's events and the turn channel, map a
///    `KeyEvent` to an [`app::Input`] and call [`app::reduce`], act on the
///    returned [`app::Action`], and call [`render`] after each change;
/// 4. restore the terminal on the way out — including on an error, which is why
///    the teardown is not simply the last statement.
///
/// Every branch inside the loop must be a call into a tested function. Anything
/// that grows a decision here is a decision no gate can see.
pub async fn run_session(app: &mut App, cfg: &Config) -> Result<()> {
    // First, before the terminal is changed at all: a refusal that has already put
    // the terminal into raw mode is a refusal that has to be undone.
    ensure_terminal(
        std::io::stdin().is_terminal(),
        std::io::stdout().is_terminal(),
    )?;

    // Armed before the changes it undoes, so a failure partway through them is
    // rolled back as well.
    let _restored = Restored;
    terminal::enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;

    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;

    let (turn_tx, mut turn_rx) = mpsc::unbounded_channel::<TurnEvent>();
    let (key_tx, mut key_rx) = mpsc::unbounded_channel::<Input>();
    let (catalog_tx, mut catalog_rx) = mpsc::unbounded_channel::<Vec<PickerItem>>();
    spawn_key_reader(key_tx);

    // Rebuilt from `App::session`, never minted here — see [`session_for`].
    let mut session = session_for(app);
    let mut in_flight = InFlight::default();
    // The turn in flight, if any. `abort` on an already-finished task is a no-op, so
    // a stale handle needs no bookkeeping; what it does do is drop the future, and
    // with it the child process (`process.rs` sets `kill_on_drop`, so a cancelled
    // turn stops spending the subscription rather than running on unwatched).
    let mut running: Option<tokio::task::JoinHandle<()>> = None;

    terminal.draw(|frame| render(frame, app))?;

    while !app.should_quit {
        let Some(step) = next_step(&mut key_rx, &mut turn_rx, &mut catalog_rx).await else {
            break;
        };

        match act(app, &mut session, &mut in_flight, step) {
            Effect::Quit => break,
            Effect::Run(prompt) => {
                // `Profile::from_home()` is read here, per turn, and handed in as a
                // result: a malformed `profile.toml` therefore fails a turn that has
                // spent nothing (`submitted_turn`), and a persona edited while the
                // TUI is open applies to the next turn.
                if let Some(request) = submitted_turn(app, &prompt, Profile::from_home()) {
                    running = Some(tokio::spawn(run_turn(
                        cfg.clone(),
                        app.backend.clone(),
                        request,
                        turn_tx.clone(),
                    )));
                }
            }
            Effect::Abort => {
                if let Some(handle) = running.take() {
                    handle.abort();
                }
            }
            Effect::Catalog => {
                tokio::spawn(fill_picker(cfg.clone(), catalog_tx.clone()));
            }
            Effect::Continue => {}
        }

        terminal.draw(|frame| render(frame, app))?;
    }

    Ok(())
}
