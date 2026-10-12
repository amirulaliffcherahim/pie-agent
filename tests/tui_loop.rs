//! Gate for M6 stage C: the terminal loop's channel wiring, its fold, and the
//! recording of a turn (`planning/ACCEPTANCE.md` §9, row C; `planning/DESIGN.md`
//! §12).
//!
//! Row C is *"the turn runs as a task and its events reach the app through the
//! channel without blocking: assert a scripted sequence of events lands in order
//! while the loop stays responsive to a quit input"*.
//!
//! ## What this gate can and cannot prove
//!
//! `tui::run_session` manipulates the real terminal, so **no test here calls it**.
//! Instead the module is written so that every decision inside the loop is a call
//! into something reachable from here, and this file drives those:
//!
//! | Behaviour | Test |
//! |---|---|
//! | a turn's events, in order, delivered while a keystroke is still serviced first | [`the_turn_events_reach_the_app_in_order_while_a_quit_is_still_serviced`] |
//! | one `Finished` per turn, failure included | [`a_turns_events_arrive_in_order_and_the_turn_ends_exactly_once`], [`a_turn_that_fails_still_ends_so_the_ui_cannot_wait_forever`] |
//! | a failed turn sets `Status::Failed` with its reason | [`a_failed_turn_sets_the_status_a_status_line_can_show`] |
//! | exactly one log row and one session row per turn | [`a_turn_is_recorded_once_in_the_frozen_log_and_once_in_the_session`] |
//! | a failed turn is recorded as a failure | [`a_failed_turn_is_recorded_as_a_failure_and_not_as_a_success`] |
//! | the prompt recorded is the user's, and a second ending cannot reuse it | [`a_turn_that_ends_is_recorded_by_the_step_handler_and_forgets_its_prompt`] |
//! | every `Action` maps to the effect the loop acts on | [`every_kind_of_action_becomes_the_effect_the_loop_acts_on`] |
//! | the frozen keymap, including the release-doubling trap | [`the_key_mapping_follows_the_frozen_keymap_and_stops_there`] |
//! | a non-terminal stdin or stdout refuses to start | [`a_pipe_on_either_end_refuses_to_start_the_tui`] |
//! | the loop records into the session the status line names | [`the_loop_records_into_the_session_the_status_line_names`] |
//! | the persona sent is the projection `pie -p` sends | [`the_tui_sends_the_projection_the_one_shot_path_sends`] |
//! | the picker takes a late catalog, and does not reopen a closed one | [`a_catalog_that_arrives_after_the_picker_closed_does_not_reopen_it`] |
//!
//! **Not covered, and not claimed**: the `tokio::spawn` calls that start a turn and
//! fetch a catalog, the `abort()` that cancels one, the raw-mode/alternate-screen
//! setup and teardown, and the crossterm `KeyEvent` → [`pie::tui::Input`] *delivery*
//! (the mapping itself is asserted; the thread that feeds it is not).
//!
//! Nothing here spawns a vendor CLI, needs a network, or needs a terminal: the turn
//! half is driven through a stand-in [`Provider`], and the recording path writes into
//! a `tempfile::tempdir()` reached through `PIE_HOME` — the same isolation
//! `tests/sessions.rs` uses for the child process, and `tests/parse_agy.rs` for the
//! in-process case.
//!
//! ## The reducer is another stage's file
//!
//! `app::reduce` and `app::apply_event` are stage A's (`tests/tui_app.rs`), and this
//! stage may not wait on them. So this file asserts only what stage C owns: which
//! *step* comes out of the channel when, and what the loop *does* about it —
//! [`pie::tui::effect_for`] is public so the mapping can be checked without a
//! reducer, and the recorded rows are checked because this stage writes them. What a
//! `StreamEvent` does to `App::transcript` is deliberately **not** asserted here.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use pie::config::Config;
use pie::error::PieError;
use pie::profile::{Profile, TASK_SEPARATOR};
use pie::provider::{
    AuthState, Capabilities, ChatRequest, Model, Provider, StreamEvent, ToolState, TurnOutcome,
    Usage, UsageReport,
};
use pie::session::{self, Session};
use pie::session_log;
use pie::tui::{
    self, Action, App, Effect, Folded, InFlight, Input, Picker, PickerItem, Status, Step, TurnEvent,
};
use tokio::sync::mpsc;

/// The fourteen keys of the **frozen** `usage.jsonl` row (`DESIGN.md` §7).
///
/// Spelled out rather than derived from the struct, for the reason
/// `tests/sessions.rs` spells it out too: this is the one schema an outside
/// implementation reads, so a rename has to break a test that names it.
const FROZEN_USAGE_KEYS: [&str; 14] = [
    "cached_input_tokens",
    "cost_usd_micros",
    "credits_consumed",
    "credits_remaining",
    "duration_ms",
    "error_kind",
    "input_tokens",
    "model",
    "ok",
    "output_tokens",
    "provider",
    "reasoning_tokens",
    "stop_reason",
    "ts",
];

// --------------------------------------------------------------- a stand-in ---

/// How a [`Scripted`] backend ends its turn.
enum Ending {
    /// Answer, with this conversation id and usage.
    Answer {
        text: String,
        conversation_id: Option<String>,
        usage: Usage,
    },
    /// Fail after whatever events it emitted, the way a timeout does.
    Fail,
}

/// A backend that answers from a script.
///
/// This is what makes the turn half of stage C testable at all: the real backends
/// spawn a vendor CLI, which no test in this project may do (it spends subscription
/// quota — `planning/HANDOFF.md` §3). `tui::stream_turn` takes the provider it runs,
/// so a stand-in exercises the ordering, the ending and the failure path for real.
struct Scripted {
    events: Vec<StreamEvent>,
    ending: Ending,
}

#[async_trait]
impl Provider for Scripted {
    fn id(&self) -> &'static str {
        "scripted"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::default()
    }

    fn binary(&self, _cfg: &Config) -> Option<PathBuf> {
        None
    }

    async fn auth_status(&self, _cfg: &Config) -> AuthState {
        AuthState::Ready
    }

    async fn list_models(&self, _cfg: &Config) -> pie::error::Result<Vec<Model>> {
        Ok(Vec::new())
    }

    async fn stream(
        &self,
        _cfg: &Config,
        _req: &ChatRequest,
        sink: &mut (dyn FnMut(StreamEvent) + Send),
    ) -> pie::error::Result<TurnOutcome> {
        for event in &self.events {
            sink(event.clone());
        }

        match &self.ending {
            Ending::Answer {
                text,
                conversation_id,
                usage,
            } => Ok(TurnOutcome {
                text: text.clone(),
                usage: usage.clone(),
                status: "SUCCESS".to_string(),
                conversation_id: conversation_id.clone(),
                duration: Duration::from_millis(1),
            }),
            // A real deadline failure: the child produced something and then ran out
            // of time, which is the shape that must still end the turn.
            Ending::Fail => Err(PieError::Timeout {
                backend: "scripted".to_string(),
                secs: 3,
            }),
        }
    }

    async fn usage(&self, _cfg: &Config) -> pie::error::Result<UsageReport> {
        Err(PieError::Unsupported {
            backend: "scripted".to_string(),
            feature: "quota buckets".to_string(),
        })
    }
}

/// A successful turn with a vendor conversation id and real-looking accounting.
fn turn_outcome(text: &str) -> TurnOutcome {
    TurnOutcome {
        text: text.to_string(),
        usage: Usage {
            input_tokens: 20_704,
            output_tokens: 20,
            thinking_tokens: 19,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            total_tokens: 20_724,
            cost_usd_micros: 0,
        },
        status: "SUCCESS".to_string(),
        conversation_id: Some("conv-1".to_string()),
        duration: Duration::from_millis(1_200),
    }
}

/// The scripted turn every ordering assertion uses: an init, two deltas, a tool step,
/// a usage report and the ending — the sequence `ACCEPTANCE.md` §9 names for row C.
fn scripted_turn_events() -> Vec<TurnEvent> {
    vec![
        TurnEvent::Stream(StreamEvent::Init {
            conversation_id: Some("conv-1".to_string()),
            cwd: None,
            tools: vec!["view_file".to_string()],
        }),
        TurnEvent::Stream(StreamEvent::TextDelta("hel".to_string())),
        TurnEvent::Stream(StreamEvent::TextDelta("lo".to_string())),
        TurnEvent::Stream(StreamEvent::ToolStep {
            name: "run_command".to_string(),
            state: ToolState::Done,
            info: None,
        }),
        TurnEvent::Stream(StreamEvent::Usage(Usage {
            input_tokens: 20_704,
            ..Usage::default()
        })),
        TurnEvent::Finished(Ok(turn_outcome("hello"))),
    ]
}

/// One [`Step`] as a comparable string.
///
/// `TurnEvent` deliberately does not implement `PartialEq` (it carries a
/// [`PieError`]), so the order is asserted over this description instead — which has
/// the side benefit of naming what arrived when an assertion fails.
fn describe(step: Step) -> String {
    match step {
        Step::Key(Input::Char(c)) => format!("key:{c}"),
        Step::Key(other) => format!("key:{other:?}"),
        Step::Turn(TurnEvent::Stream(event)) => format!("stream:{}", event.kind()),
        Step::Turn(TurnEvent::Finished(Ok(outcome))) => format!("finished:{}", outcome.text),
        Step::Turn(TurnEvent::Finished(Err(error))) => format!("failed:{}", error.kind()),
        Step::Catalog(items) => format!("catalog:{}", items.len()),
    }
}

// ------------------------------------------------------------- isolated home ---

fn home_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Points `PIE_HOME` at a scratch directory for the length of one test.
///
/// The recording path resolves the log and the session store from `$PIE_HOME` at
/// call time (`session_log::append_best_effort`, `Session::start_in`), so there is no
/// root to pass in and the environment is the only seam — the in-process mirror of
/// what `tests/sessions.rs` does for the child process, and the same shape as
/// `tests/parse_agy.rs::PinHome`. `std::env` is process-global while the harness runs
/// tests on threads, so the lock, not the guard alone, is the isolation.
struct PinnedHome {
    _guard: MutexGuard<'static, ()>,
    previous: Option<String>,
}

impl PinnedHome {
    fn set(path: &Path) -> Self {
        let guard = home_lock().lock().unwrap_or_else(|e| e.into_inner());
        let previous = std::env::var("PIE_HOME").ok();
        std::env::set_var("PIE_HOME", path);
        Self {
            _guard: guard,
            previous,
        }
    }
}

impl Drop for PinnedHome {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(value) => std::env::set_var("PIE_HOME", value),
            None => std::env::remove_var("PIE_HOME"),
        }
    }
}

/// Every line of a file, or none when it does not exist.
fn lines_of(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(str::to_string)
        .collect()
}

// ------------------------------------------------- the loop and the channel ---

/// The substantive claim (`ACCEPTANCE.md` §9, row C): a scripted sequence of turn
/// events reaches the app **in order**, while a keystroke is still serviced.
///
/// A loop that drained the turn channel before looking at the keyboard fails at the
/// first assertion — the queued `Ctrl+c` would arrive seventh, or never — which is
/// the freeze this design exists to prevent (`DESIGN.md` §12: a synchronous turn
/// holds the interface for up to ~40 s on agy).
///
/// The second half folds the same scripted sequence `StreamEvent`-by-`StreamEvent`
/// and asserts one acknowledgement per event, in order, ending with the turn's end:
/// the events do not merely arrive, they are folded one at a time and the last one
/// closes the turn.
#[tokio::test]
async fn the_turn_events_reach_the_app_in_order_while_a_quit_is_still_serviced() {
    let (key_tx, mut keys) = mpsc::unbounded_channel::<Input>();
    let (turn_tx, mut turns) = mpsc::unbounded_channel::<TurnEvent>();
    let (_catalog_tx, mut catalog) = mpsc::unbounded_channel::<Vec<PickerItem>>();

    // A whole turn is already queued when the key arrives, which is the state that
    // separates a select loop from a drain.
    for event in scripted_turn_events() {
        turn_tx.send(event).unwrap();
    }
    key_tx.send(Input::Char('q')).unwrap();

    let mut delivered = Vec::new();
    for _ in 0..7 {
        let step = tui::next_step(&mut keys, &mut turns, &mut catalog)
            .await
            .expect("seven queued events");
        delivered.push(describe(step));
    }

    assert_eq!(
        delivered,
        vec![
            "key:q",
            "stream:init",
            "stream:text",
            "stream:text",
            "stream:tool",
            "stream:usage",
            "finished:hello",
        ],
        "the keystroke is serviced first, and every turn event keeps its order"
    );

    // The same sequence, folded: one acknowledgement per event, the last one ending
    // the turn.
    let mut app = App::new("codex", None, "s-1-2");
    let folded: Vec<String> = scripted_turn_events()
        .into_iter()
        .map(|event| match tui::apply_turn_event(&mut app, event) {
            Folded::Streaming => "streaming".to_string(),
            Folded::Ended(Ok(outcome)) => format!("ended:{}", outcome.text),
            Folded::Ended(Err(error)) => format!("ended:{}", error.kind()),
        })
        .collect();

    assert_eq!(
        folded,
        vec![
            "streaming",
            "streaming",
            "streaming",
            "streaming",
            "streaming",
            "ended:hello",
        ],
        "the app acknowledges each event in order and the turn ends once"
    );
    assert_eq!(app.status, Status::Done);
}

/// A terminal that stops delivering keys ends the loop instead of leaving it waiting
/// on a keystroke that cannot arrive.
#[tokio::test]
async fn a_closed_key_channel_ends_the_loop() {
    let (key_tx, mut keys) = mpsc::unbounded_channel::<Input>();
    let (_turn_tx, mut turns) = mpsc::unbounded_channel::<TurnEvent>();
    let (_catalog_tx, mut catalog) = mpsc::unbounded_channel::<Vec<PickerItem>>();

    // The reader thread exits when `event::read` fails — the terminal going away.
    drop(key_tx);

    assert!(
        tui::next_step(&mut keys, &mut turns, &mut catalog)
            .await
            .is_none(),
        "a loop with no keyboard must stop, not sit looking alive"
    );
}

/// The turn half, driven end to end through the channel: the events arrive in the
/// order the backend emitted them, and the turn ends **exactly once**.
///
/// The `while let` loop below ends when the channel closes, which only happens once
/// `stream_turn` has returned — so a second `Finished`, or a `Finished` sent before
/// the events, shows up in this vector. That is the assertion "a row exists" cannot
/// make for the ending.
#[tokio::test]
async fn a_turns_events_arrive_in_order_and_the_turn_ends_exactly_once() {
    let (tx, mut rx) = mpsc::unbounded_channel::<TurnEvent>();
    let provider: Box<dyn Provider> = Box::new(Scripted {
        events: vec![
            StreamEvent::Init {
                conversation_id: Some("conv-1".to_string()),
                cwd: None,
                tools: vec!["view_file".to_string()],
            },
            StreamEvent::TextDelta("hello".to_string()),
            StreamEvent::ToolStep {
                name: "run_command".to_string(),
                state: ToolState::Done,
                info: None,
            },
            StreamEvent::Done {
                text: "hello".to_string(),
                status: "SUCCESS".to_string(),
            },
        ],
        ending: Ending::Answer {
            text: "hello".to_string(),
            conversation_id: Some("conv-1".to_string()),
            usage: Usage {
                input_tokens: 7,
                ..Usage::default()
            },
        },
    });

    tui::stream_turn(
        Config::default(),
        provider,
        ChatRequest::new("say hello"),
        tx,
    )
    .await;

    let mut arrived = Vec::new();
    while let Some(event) = rx.recv().await {
        arrived.push(match event {
            TurnEvent::Stream(event) => format!("stream:{}", event.kind()),
            TurnEvent::Finished(Ok(outcome)) => format!("finished:{}", outcome.text),
            TurnEvent::Finished(Err(error)) => format!("failed:{}", error.kind()),
        });
    }

    assert_eq!(
        arrived,
        vec![
            "stream:init",
            "stream:text",
            "stream:tool",
            "stream:done",
            "finished:hello",
        ]
    );
}

/// A turn that fails still ends.
///
/// This is the trap a `?` in `stream_turn` would spring: the events would arrive,
/// the failure would return early, and the UI would sit in `Status::Running` for
/// ever with the one keystroke that cancels it refused by the reducer — a paid-for
/// turn the user cannot get out of.
#[tokio::test]
async fn a_turn_that_fails_still_ends_so_the_ui_cannot_wait_forever() {
    let (tx, mut rx) = mpsc::unbounded_channel::<TurnEvent>();
    let provider: Box<dyn Provider> = Box::new(Scripted {
        events: vec![StreamEvent::TextDelta("partial".to_string())],
        ending: Ending::Fail,
    });

    tui::stream_turn(Config::default(), provider, ChatRequest::new("hi"), tx).await;

    let mut arrived = Vec::new();
    while let Some(event) = rx.recv().await {
        arrived.push(match event {
            TurnEvent::Stream(event) => format!("stream:{}", event.kind()),
            TurnEvent::Finished(Ok(outcome)) => format!("finished:{}", outcome.text),
            TurnEvent::Finished(Err(error)) => format!("failed:{}", error.kind()),
        });
    }

    assert_eq!(arrived, vec!["stream:text", "failed:timeout"]);
}

/// A backend that is not in the registry ends the turn with the reason, and spawns
/// nothing — which is why this test is allowed to call the real entry point.
#[tokio::test]
async fn an_unknown_backend_ends_the_turn_with_a_reason() {
    let (tx, mut rx) = mpsc::unbounded_channel::<TurnEvent>();

    tui::run_turn(
        Config::default(),
        "geminy".to_string(),
        ChatRequest::new("hi"),
        tx,
    )
    .await;

    match rx.recv().await {
        Some(TurnEvent::Finished(Err(PieError::UnknownBackend { backend, known }))) => {
            assert_eq!(backend, "geminy");
            assert!(known.contains(&"agy".to_string()));
        }
        other => panic!("expected a failed turn, got {other:?}"),
    }
    assert!(
        rx.recv().await.is_none(),
        "exactly one ending: nothing else may follow it"
    );
}

// ------------------------------------------------------------ the fold --------

/// A failed turn says so where the status line can show it, and the reason cannot run
/// off the end of the line.
#[test]
fn a_failed_turn_sets_the_status_a_status_line_can_show() {
    let mut app = App::new("agy", Some("gemini-3.8-flash".to_string()), "s-1-2");

    let folded = tui::apply_turn_event(
        &mut app,
        TurnEvent::Finished(Err(PieError::Timeout {
            backend: "agy".to_string(),
            secs: 40,
        })),
    );

    match folded {
        Folded::Ended(Err(error)) => assert_eq!(error.kind(), "timeout"),
        other => panic!("expected the turn to end with its error, got {other:?}"),
    }

    match &app.status {
        Status::Failed(reason) => {
            assert!(reason.contains("timed out after 40s"), "{reason}");
        }
        other => panic!("expected a failed status, got {other:?}"),
    }

    // A backend's diagnostic can be hundreds of characters; `Status::Failed` is one
    // line, so the reason is bounded (and marked as cut, so a truncated message is
    // never mistaken for the whole one).
    let mut app = App::new("agy", None, "s-1-3");
    tui::apply_turn_event(
        &mut app,
        TurnEvent::Finished(Err(PieError::Config("x".repeat(1_000)))),
    );
    match &app.status {
        Status::Failed(reason) => {
            assert!(
                reason.chars().count() <= 160 + "…[truncated]".chars().count(),
                "the status line must not hold a diagnostic of {} characters",
                reason.chars().count()
            );
            assert!(reason.ends_with("…[truncated]"), "{reason}");
        }
        other => panic!("expected a failed status, got {other:?}"),
    }
}

// --------------------------------------------------------- the recording ------

/// A turn is recorded exactly as `commands/print.rs` records a `pie -p` turn: one row
/// in the frozen log **and** one row in the session store.
///
/// The row *counts* are the assertion that matters. "A row exists" passes for a path
/// that logs twice, and the double-log is not hypothetical: it would double every
/// total `pie usage` reports for interactive sessions and no other test would notice.
#[test]
fn a_turn_is_recorded_once_in_the_frozen_log_and_once_in_the_session() {
    let scratch = tempfile::tempdir().unwrap();
    let _home = PinnedHome::set(scratch.path());

    let mut session = tui::begin_session("agy");
    let id = session.id().to_string();
    let app = App::new("agy", Some("gemini-3.8-flash".to_string()), id.clone());
    let mut in_flight = InFlight::default();
    in_flight.begin("summarise the release notes".to_string());

    let outcome = Ok(turn_outcome("the answer"));
    tui::record_turn(&mut session, &app, &in_flight, &outcome);

    // 1. the frozen log, with the frozen schema
    let rows = lines_of(&scratch.path().join("usage.jsonl"));
    assert_eq!(
        rows.len(),
        1,
        "one turn is one row; a second row would double every total"
    );

    let row: serde_json::Value = serde_json::from_str(&rows[0]).unwrap();
    for key in FROZEN_USAGE_KEYS {
        assert!(row.get(key).is_some(), "frozen schema is missing `{key}`");
    }
    assert_eq!(row["provider"], "agy");
    assert_eq!(row["model"], "gemini-3.8-flash");
    assert_eq!(row["ok"], true);
    assert_eq!(row["input_tokens"], 20_704);
    assert_eq!(row["reasoning_tokens"], 19, "thinking → reasoning_tokens");
    assert_eq!(row["stop_reason"], "SUCCESS");
    assert!(row["error_kind"].is_null());

    // …and it round-trips through pie's own reader, so the row the TUI writes is the
    // row `pie usage` and M7 will read.
    let parsed = session_log::read_all(&scratch.path().join("usage.jsonl")).unwrap();
    assert_eq!(parsed.len(), 1);
    assert_eq!(parsed[0].input_tokens, 20_704);
    assert_eq!(parsed[0].reasoning_tokens, 19);

    // 2. the session store: the conversation, not just its cost
    let transcript = session::load(&id).unwrap();
    assert_eq!(transcript.len(), 1, "the turn lands in exactly one row");
    assert_eq!(transcript[0].prompt, "summarise the release notes");
    assert_eq!(transcript[0].response, "the answer");
    assert_eq!(transcript[0].conversation_id.as_deref(), Some("conv-1"));
    assert_eq!(transcript[0].usage.input_tokens, 20_704);
    assert!(transcript[0].ok);

    let index = session::list_from(&scratch.path().join("sessions")).unwrap();
    assert_eq!(index.len(), 1, "one index frame for one session");
    assert_eq!(index[0].provider, "agy");
    assert_eq!(index[0].turns, 1);
}

/// A turn that failed is recorded as one, in both files.
///
/// A failure row is not bookkeeping: it is the half of a session that says what was
/// *asked* when nothing came back, and `pie usage` counts the wall clock it cost.
#[test]
fn a_failed_turn_is_recorded_as_a_failure_and_not_as_a_success() {
    let scratch = tempfile::tempdir().unwrap();
    let _home = PinnedHome::set(scratch.path());

    let mut session = tui::begin_session("codex");
    let id = session.id().to_string();
    let app = App::new("codex", None, id.clone());
    let mut in_flight = InFlight::default();
    in_flight.begin("do the thing".to_string());

    let outcome = Err(PieError::Timeout {
        backend: "codex".to_string(),
        secs: 600,
    });
    tui::record_turn(&mut session, &app, &in_flight, &outcome);

    let rows = lines_of(&scratch.path().join("usage.jsonl"));
    assert_eq!(rows.len(), 1);
    let row: serde_json::Value = serde_json::from_str(&rows[0]).unwrap();
    assert_eq!(row["ok"], false);
    assert_eq!(row["error_kind"], "timeout");
    assert_eq!(row["input_tokens"], 0, "a failed turn consumed no tokens");
    assert_eq!(
        row["model"], "",
        "the backend picked its own model and the schema has no null"
    );

    let transcript = session::load(&id).unwrap();
    assert_eq!(transcript.len(), 1);
    assert!(!transcript[0].ok);
    assert_eq!(transcript[0].stop_reason, "timeout");
    assert_eq!(transcript[0].prompt, "do the thing");
    assert!(transcript[0].response.is_empty());
    assert!(transcript[0].conversation_id.is_none());
}

/// The step handler records a turn the moment it ends, and forgets the prompt so a
/// second ending cannot attribute it to a turn the user never asked for.
#[test]
fn a_turn_that_ends_is_recorded_by_the_step_handler_and_forgets_its_prompt() {
    let scratch = tempfile::tempdir().unwrap();
    let _home = PinnedHome::set(scratch.path());

    let mut session = tui::begin_session("codex");
    let id = session.id().to_string();
    let mut app = App::new("codex", Some("gpt-6-luna".to_string()), id.clone());
    let mut in_flight = InFlight::default();
    in_flight.begin("the typed prompt".to_string());

    let effect = tui::act(
        &mut app,
        &mut session,
        &mut in_flight,
        Step::Turn(TurnEvent::Finished(Ok(turn_outcome("first answer")))),
    );
    assert_eq!(effect, Effect::Continue);
    assert!(
        in_flight.prompt.is_empty() && in_flight.started.is_none(),
        "the finished turn must not stay in flight"
    );

    // A second ending — a bug, or a task that outlives its own ending — records an
    // empty prompt rather than re-attributing the first one.
    tui::act(
        &mut app,
        &mut session,
        &mut in_flight,
        Step::Turn(TurnEvent::Finished(Ok(turn_outcome("second answer")))),
    );

    let log = lines_of(&scratch.path().join("usage.jsonl"));
    assert_eq!(log.len(), 2, "two turns, two rows");
    let transcript = session::load(&id).unwrap();
    assert_eq!(transcript.len(), 2);
    assert_eq!(transcript[0].prompt, "the typed prompt");
    assert_eq!(transcript[0].response, "first answer");
    assert_eq!(
        transcript[1].prompt, "",
        "the prompt was forgotten when it ended"
    );
}

/// An unmeasured wall clock reads as zero rather than as an invented duration: the
/// frozen schema has no null, so a guess would be indistinguishable from a measurement.
#[test]
fn an_unknown_wall_clock_reads_as_zero_rather_than_a_guess() {
    let mut in_flight = InFlight::default();
    assert_eq!(in_flight.elapsed_ms(), 0, "nothing was submitted");

    in_flight.begin("hi".to_string());
    assert!(
        in_flight.elapsed_ms() < 1_000,
        "a submission starts the clock, and does not time-travel"
    );

    in_flight.clear();
    assert_eq!(in_flight.elapsed_ms(), 0);
}

// ------------------------------------------------------------- the keymap -----

/// Every kind of [`Action`] the reducer can return maps to the effect the loop acts
/// on.
///
/// Public and table-driven on purpose: `app::reduce` is stage A's file, so this is the
/// only way stage C's gate can assert its own half of the keymap — and the match in
/// `effect_for` is exhaustive, so a new `Action` cannot quietly do nothing.
#[test]
fn every_kind_of_action_becomes_the_effect_the_loop_acts_on() {
    assert_eq!(tui::effect_for(Action::Quit, String::new()), Effect::Quit);
    assert_eq!(
        tui::effect_for(Action::Submit, "what I typed".to_string()),
        Effect::Run("what I typed".to_string()),
        "the submitted text travels with the effect: the reducer has already cleared the prompt"
    );
    assert_eq!(
        tui::effect_for(Action::Cancel, String::new()),
        Effect::Abort
    );
    assert_eq!(
        tui::effect_for(Action::OpenPicker, String::new()),
        Effect::Catalog
    );

    for action in [
        Action::None,
        Action::ScrollUp,
        Action::ScrollDown,
        Action::PickerNext,
        Action::PickerPrev,
        Action::PickerAccept,
    ] {
        assert_eq!(
            tui::effect_for(action, String::new()),
            Effect::Continue,
            "{action:?} is a state change the reducer already made"
        );
    }
}

/// The frozen keymap (`DESIGN.md` §12), including the trap that makes a Windows
/// terminal double every keystroke.
#[test]
fn the_key_mapping_follows_the_frozen_keymap_and_stops_there() {
    let press = |code: KeyCode| KeyEvent::new(code, KeyModifiers::NONE);

    assert_eq!(
        tui::input_from_key(press(KeyCode::Char('a'))),
        Some(Input::Char('a'))
    );
    assert_eq!(
        tui::input_from_key(press(KeyCode::Char('A'))),
        Some(Input::Char('A')),
        "shift is already in the character, not a separate input"
    );
    assert_eq!(
        tui::input_from_key(press(KeyCode::Enter)),
        Some(Input::Enter)
    );
    assert_eq!(tui::input_from_key(press(KeyCode::Esc)), Some(Input::Esc));
    assert_eq!(
        tui::input_from_key(press(KeyCode::Backspace)),
        Some(Input::Backspace)
    );
    assert_eq!(tui::input_from_key(press(KeyCode::Up)), Some(Input::Up));
    assert_eq!(tui::input_from_key(press(KeyCode::Down)), Some(Input::Down));
    assert_eq!(tui::input_from_key(press(KeyCode::Left)), Some(Input::Left));
    assert_eq!(
        tui::input_from_key(press(KeyCode::Right)),
        Some(Input::Right)
    );
    assert_eq!(tui::input_from_key(press(KeyCode::Tab)), Some(Input::Tab));

    assert_eq!(
        tui::input_from_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        Some(Input::Ctrl('c'))
    );
    assert_eq!(
        tui::input_from_key(KeyEvent::new(
            KeyCode::Char('C'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT
        )),
        Some(Input::Ctrl('c')),
        "the quit key must not depend on the keyboard's shift state"
    );
    assert_eq!(
        tui::input_from_key(KeyEvent::new(KeyCode::Up, KeyModifiers::CONTROL)),
        None,
        "Ctrl+Up is not an arrow row in the keymap"
    );

    for code in [
        KeyCode::F(1),
        KeyCode::Insert,
        KeyCode::Delete,
        KeyCode::Home,
        KeyCode::End,
        KeyCode::PageUp,
        KeyCode::Null,
        KeyCode::BackTab,
    ] {
        assert_eq!(
            tui::input_from_key(press(code)),
            None,
            "{code:?} has no row in the frozen keymap"
        );
    }

    // The doubling trap: Windows reports a release for every press, and two `Enter`
    // inputs are two turns' worth of subscription quota.
    let press_enter =
        KeyEvent::new_with_kind(KeyCode::Enter, KeyModifiers::NONE, KeyEventKind::Press);
    let release_enter =
        KeyEvent::new_with_kind(KeyCode::Enter, KeyModifiers::NONE, KeyEventKind::Release);
    assert_eq!(tui::input_from_key(press_enter), Some(Input::Enter));
    assert_eq!(
        tui::input_from_key(release_enter),
        None,
        "a release is not a keystroke"
    );

    // Auto-repeat is a real keystroke: a held Backspace must delete repeatedly.
    assert_eq!(
        tui::input_from_key(KeyEvent::new_with_kind(
            KeyCode::Backspace,
            KeyModifiers::NONE,
            KeyEventKind::Repeat
        )),
        Some(Input::Backspace)
    );

    // Events a terminal sends that the keymap never mentions carry no input.
    assert_eq!(tui::input_for(&Event::Resize(80, 24)), None);
    assert_eq!(tui::input_for(&Event::FocusGained), None);
    assert_eq!(
        tui::input_for(&Event::Key(press(KeyCode::Tab))),
        Some(Input::Tab)
    );
}

// ----------------------------------------------------------- the terminal -----

/// `pie tui` on a pipe refuses to start, naming the stream that is not a terminal.
///
/// Exercised as a function, because starting the TUI in a test would take over the
/// terminal the harness is reporting on (`ACCEPTANCE.md` §9 asks for the refusal,
/// not for an escape-code show).
#[test]
fn a_pipe_on_either_end_refuses_to_start_the_tui() {
    assert!(tui::ensure_terminal(true, true).is_ok());

    let stdin_pipe = tui::ensure_terminal(false, true).unwrap_err();
    assert!(matches!(&stdin_pipe, PieError::Config(_)));
    assert_eq!(stdin_pipe.exit_code(), 2, "a usage error, per DESIGN §5");
    let message = stdin_pipe.to_string();
    assert!(
        message.contains("but stdin is not a terminal"),
        "the offending stream is named: {message}"
    );
    assert!(
        message.contains("pie -p"),
        "it says what to do instead: {message}"
    );

    let stdout_pipe = tui::ensure_terminal(true, false).unwrap_err();
    assert!(
        stdout_pipe
            .to_string()
            .contains("but stdout is not a terminal"),
        "{stdout_pipe}"
    );

    let both = tui::ensure_terminal(false, false).unwrap_err();
    let both = both.to_string();
    assert!(
        both.contains("but neither stdin nor stdout is a terminal"),
        "both are named, and the sentence still reads: {both}"
    );
    assert!(
        !both.contains("stdout is not one"),
        "the singular/compound mismatch must not come back: {both}"
    );
}

/// The handle the loop records through names the same transcript as the id the status
/// line shows.
///
/// `run_session` receives only `&mut App`, so the store handle is rebuilt from
/// `App::session`. A second mint there would record every turn in a session the user
/// cannot name — and that `--resume` could never find (`DESIGN.md` §11).
#[test]
fn the_loop_records_into_the_session_the_status_line_names() {
    let scratch = tempfile::tempdir().unwrap();
    let _home = PinnedHome::set(scratch.path());

    let minted = tui::begin_session("codex");
    let app = App::new("codex", None, minted.id());

    let rebuilt = tui::session_for(&app);

    assert_eq!(rebuilt.id(), minted.id());
    assert_eq!(
        rebuilt.transcript_path(),
        minted.transcript_path(),
        "the rebuilt handle must write the file the minted id names"
    );
    assert!(
        rebuilt.transcript_path().starts_with(scratch.path()),
        "and it stays inside the store"
    );
}

// ------------------------------------------------------------- the request ----

/// The persona the TUI sends is the projection the one-shot path sends.
///
/// M3's Done condition is that agy and codex receive byte-identical personas; a TUI
/// that read the profile some other way (or folded it itself) would falsify it with
/// nothing to say so. The bytes come from `Profile::persona`, the same accessor
/// `commands/print.rs` uses, and the task text is left **unfolded** — folding is the
/// backend's job, through one shared function.
#[test]
fn the_tui_sends_the_projection_the_one_shot_path_sends() {
    let scratch = tempfile::tempdir().unwrap();
    std::fs::write(
        scratch.path().join("profile.toml"),
        "name = \"pie\"\ntone = \"terse\"\n",
    )
    .unwrap();
    std::fs::write(scratch.path().join("AGENTS.md"), "no emojis\n").unwrap();

    let profile = Profile::load(scratch.path()).unwrap();
    let persona = profile.persona().expect("a configured persona");
    assert!(persona.contains("no emojis"), "{persona}");

    let app = App::new("agy", Some("gemini-3.8-flash".to_string()), "s-1-2");
    let request = tui::build_request(&app, "summarise the log", &profile);

    assert_eq!(
        request.persona.as_deref(),
        profile.persona(),
        "the same bytes the one-shot path hands to every backend"
    );
    assert!(request.has_persona());
    assert_eq!(
        request.prompt, "summarise the log",
        "the task is not folded here"
    );
    assert_eq!(
        request.folded_prompt(),
        format!("{persona}{TASK_SEPARATOR}summarise the log"),
        "and the fold is the one shared rule"
    );
    assert_eq!(request.model.as_deref(), Some("gemini-3.8-flash"));
    assert!(
        request.resume.is_none(),
        "a TUI turn is its own conversation, never a resumed vendor id"
    );

    // No projection configured: `None`, never `Some("")`, which would still change the
    // payload every backend receives.
    let bare = Profile::load(&scratch.path().join("nowhere")).unwrap();
    let request = tui::build_request(&app, "task", &bare);
    assert!(request.persona.is_none());
    assert!(!request.has_persona());
    assert_eq!(request.folded_prompt(), "task");
}

/// A submission whose persona cannot be read fails the turn instead of running one.
///
/// The order matters: `commands/print.rs` loads the projection before anything is
/// spawned, so a broken `profile.toml` costs nothing. A submission has already been
/// accepted by the time this runs, so the reason goes on the status line.
#[test]
fn a_submission_that_cannot_be_built_fails_the_turn_without_running_one() {
    let mut app = App::new("codex", None, "s-1-2");

    let request = tui::submitted_turn(
        &mut app,
        "do the thing",
        Err(PieError::Config(
            "profile.toml: expected a value".to_string(),
        )),
    );
    assert!(
        request.is_none(),
        "nothing may be spawned from a broken persona"
    );
    match &app.status {
        Status::Failed(reason) => assert!(reason.contains("profile.toml"), "{reason}"),
        other => panic!("expected the profile error to be shown, got {other:?}"),
    }

    let scratch = tempfile::tempdir().unwrap();
    let profile = Profile::load(scratch.path()).unwrap();
    let request = tui::submitted_turn(&mut app, "do the thing", Ok(profile));
    assert_eq!(
        request.map(|request| request.prompt),
        Some("do the thing".to_string()),
        "the prompt the reducer accepted is the one the request carries"
    );
}

// -------------------------------------------------------------- the picker ----

/// The picker rows carry the backend as well as the model, because taking a row has
/// to set both.
///
/// Built through [`Picker::from_models`] rather than a second constructor here. There
/// used to be one, in `src/tui.rs`, and it produced a bare `gpt-6-luna (GPT-6 Luna)`
/// label while `from_models` produced the backend-qualified one — so the merged list
/// `fill_picker` builds rendered rows that said which *model* but not which
/// *subscription* (`DESIGN.md` §12). Two constructors with one job is how they drifted,
/// so `from_models` is now the only authority and the label is asserted here.
#[test]
fn the_picker_rows_name_the_backend_and_the_command_line_spelling() {
    let models = vec![
        Model::plain("gemini-3.8-flash", "gemini-3.8-flash"),
        Model {
            id: "gpt-6-luna".to_string(),
            display: "GPT-6 Luna".to_string(),
            efforts: Vec::new(),
            default_effort: None,
        },
        Model::plain("no-display", ""),
    ];

    let items = Picker::from_models("agy", &models).items;

    assert_eq!(items.len(), 3);
    assert_eq!(
        items[0].backend, "agy",
        "a row must be able to change backend"
    );
    assert_eq!(items[0].model, "gemini-3.8-flash");
    assert_eq!(
        items[0].label, "agy/gemini-3.8-flash",
        "the label must lead with the backend/model spelling, so the row says which \
         subscription it selects and can be quoted on a command line"
    );
    assert_eq!(items[1].label, "agy/gpt-6-luna (GPT-6 Luna)");
    assert_eq!(
        items[2].label, "agy/no-display",
        "an empty display name is not rendered as an empty bracket"
    );
}

/// Two subscriptions offering the same model id, merged the way `fill_picker` merges them.
///
/// The property the picker exists for, and the one it did not have: `render` draws only
/// `PickerItem::label`, so with the backend left out of the label the two rows read alike
/// and "which subscription does `enter` switch me to" — the whole reason a row carries a
/// backend as well as a model — cannot be answered from the screen. This fails if the
/// backend ever drops back out of the label.
#[test]
fn two_backends_offering_the_same_model_are_tellable_apart_in_one_merged_list() {
    let model = Model::plain("gpt-6-luna", "GPT-6 Luna");

    // Merged exactly as `fill_picker` merges it: one backend's rows, then the next's.
    let mut items = Picker::from_models("codex", std::slice::from_ref(&model)).items;
    items.extend(Picker::from_models("agy", std::slice::from_ref(&model)).items);

    assert_eq!(
        items.len(),
        2,
        "one row per subscription offering the model"
    );
    assert_ne!(
        items[0].label, items[1].label,
        "the same model id on two backends must not read as one row: {items:?}"
    );
    for item in &items {
        assert!(
            item.label.starts_with(&format!("{}/", item.backend)),
            "the label must name the backend the row selects: {item:?}"
        );
    }

    // Still the pair the reducer takes, not merely two distinct strings.
    assert_eq!(
        (items[0].backend.as_str(), items[0].model.as_str()),
        ("codex", "gpt-6-luna")
    );
    assert_eq!(
        (items[1].backend.as_str(), items[1].model.as_str()),
        ("agy", "gpt-6-luna")
    );
}

/// A catalog that arrives after the picker was closed does not reopen it: the read is
/// a task of its own, so `Esc` can legitimately land while it is still running.
#[test]
fn a_catalog_that_arrives_after_the_picker_closed_does_not_reopen_it() {
    let scratch = tempfile::tempdir().unwrap();
    let mut session = Session::start_in(scratch.path(), "codex", Path::new("."));
    let mut app = App::new("codex", None, "s-1-2");
    let mut in_flight = InFlight::default();

    let rows = vec![PickerItem {
        backend: "agy".to_string(),
        model: "gemini-3.8-flash".to_string(),
        label: "gemini-3.8-flash".to_string(),
    }];

    // Closed in the meantime (Esc): the late rows are dropped.
    assert_eq!(
        tui::act(
            &mut app,
            &mut session,
            &mut in_flight,
            Step::Catalog(rows.clone())
        ),
        Effect::Continue
    );
    assert!(
        app.picker.is_none(),
        "a late catalog must not reopen a dismissed picker"
    );

    // Still open: the rows land, with the highlight at the top.
    app.picker = Some(Picker::default());
    assert_eq!(
        tui::act(
            &mut app,
            &mut session,
            &mut in_flight,
            Step::Catalog(rows.clone())
        ),
        Effect::Continue
    );
    let picker = app.picker.expect("the picker is open");
    assert_eq!(picker.items.len(), 1);
    assert_eq!(picker.selected, 0);
}
