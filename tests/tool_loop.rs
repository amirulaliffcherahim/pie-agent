//! The `tool_use` → `tool_result` loop, against scripted transports and real stand-in
//! children (`ACCEPTANCE.md` §17, stage B).
//!
//! Nothing here spawns a vendor CLI: there is no Claude subscription on this machine, so
//! every test either drives an in-memory [`Transport`] or a `.bat`/`.sh` stand-in that
//! speaks the same frames. The milestone is therefore machine-independent — the same
//! property gate G3d exists to protect.
//!
//! The three claims that carry the whole stage each have a test that fails if the claim
//! is false: the id a result answers (`a_tool_result_names_the_id_of_the_call_it_answers`),
//! the bound (`the_iteration_cap_stops_a_model_that_asks_for_tools_forever_and_says_so`)
//! and the deadlock
//! (`the_loop_finishes_against_a_child_that_will_not_read_its_stdin_until_it_has_written_its_stdout`).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use pie::error::PieError;
use pie::process::RunSpec;
use pie::provider::{by_id, Capabilities, StreamEvent};
use pie::tool_loop::{
    check_supported, run_loop, tool_result_frame, ChildTransport, LoopOutcome, ToolCall,
    ToolResult, ToolRunner, Transport,
};

/// Lock without `unwrap` noise: a poisoned lock only means another test panicked.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A transport that yields a fixed script and records what the loop sent back.
#[derive(Debug, Default)]
struct Scripted {
    events: Mutex<VecDeque<StreamEvent>>,
    sent: Mutex<Vec<Value>>,
    reads: AtomicUsize,
}

impl Scripted {
    fn new(events: Vec<StreamEvent>) -> Self {
        Self {
            events: Mutex::new(events.into()),
            sent: Mutex::new(Vec::new()),
            reads: AtomicUsize::new(0),
        }
    }

    fn sent(&self) -> Vec<Value> {
        lock(&self.sent).clone()
    }

    fn reads(&self) -> usize {
        self.reads.load(Ordering::SeqCst)
    }
}

impl Transport for Scripted {
    fn send(&mut self, frame: Value) -> Result<(), PieError> {
        lock(&self.sent).push(frame);
        Ok(())
    }

    fn recv(&mut self) -> Result<Option<StreamEvent>, PieError> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        Ok(lock(&self.events).pop_front())
    }
}

/// A model that asks for a tool forever. Used to prove the cap bounds it.
#[derive(Debug, Default)]
struct EndlessCalls {
    asked: AtomicUsize,
    sent: Mutex<Vec<Value>>,
}

impl EndlessCalls {
    /// How many events the loop asked for. A cap that works stops asking, so this is the
    /// count that shows the loop let go of the stream rather than draining it forever.
    fn reads(&self) -> usize {
        self.asked.load(Ordering::SeqCst)
    }
}

impl Transport for EndlessCalls {
    fn send(&mut self, frame: Value) -> Result<(), PieError> {
        lock(&self.sent).push(frame);
        Ok(())
    }

    fn recv(&mut self) -> Result<Option<StreamEvent>, PieError> {
        let n = self.asked.fetch_add(1, Ordering::SeqCst);
        Ok(Some(StreamEvent::ToolUse {
            id: format!("toolu_{n}"),
            name: "Read".to_string(),
            input: json!({ "path": "a.txt" }),
        }))
    }
}

/// Answers with the call's own id, and says which tool it ran.
#[derive(Debug)]
struct EchoRunner;

impl ToolRunner for EchoRunner {
    fn run(&self, call: &ToolCall) -> ToolResult {
        ToolResult {
            id: call.id.clone(),
            output: format!("ran {}", call.name),
            is_error: false,
        }
    }
}

/// Mints its own id: the only way a result can name a call that never happened.
#[derive(Debug)]
struct FabricatingRunner {
    id: String,
}

impl ToolRunner for FabricatingRunner {
    fn run(&self, _call: &ToolCall) -> ToolResult {
        ToolResult {
            id: self.id.clone(),
            output: "done".to_string(),
            is_error: false,
        }
    }
}

fn tool_use(id: &str, name: &str) -> StreamEvent {
    StreamEvent::ToolUse {
        id: id.to_string(),
        name: name.to_string(),
        input: json!({ "path": "a.txt" }),
    }
}

/// The provider's own capability table, so the gate is tested against the real thing
/// rather than against a `false` a test wrote for itself.
fn capabilities(backend: &str) -> Capabilities {
    by_id(backend)
        .expect("the backend is registered")
        .capabilities()
}

// ---------------------------------------------------------------------------
// The loop against a scripted transport
// ---------------------------------------------------------------------------

#[test]
fn a_tool_result_names_the_id_of_the_call_it_answers() {
    let script = vec![
        StreamEvent::TextDelta("looking".to_string()),
        tool_use("toolu_abc", "Read"),
        StreamEvent::Done {
            text: "lookingdone".to_string(),
            status: "SUCCESS".to_string(),
        },
    ];
    let mut transport = Scripted::new(script.clone());
    let mut seen: Vec<StreamEvent> = Vec::new();

    let outcome = run_loop(&mut transport, &EchoRunner, 4, &mut |event| {
        seen.push(event);
    })
    .expect("the loop finishes against a scripted transport");

    let sent = transport.sent();
    assert_eq!(sent.len(), 1, "one call, one result: {sent:?}");
    assert_eq!(
        sent[0]["tool_use_id"], "toolu_abc",
        "the result must name the call's own id, not merely carry one: {sent:?}"
    );
    assert_eq!(sent[0]["content"], "ran Read");
    assert_eq!(sent[0]["is_error"], false);

    assert_eq!(outcome.iterations, 1);
    assert!(!outcome.capped);
    assert_eq!(
        outcome.text, "looking",
        "the Done event repeats the answer and must not be appended as well"
    );
    assert_eq!(seen, script, "every event reaches the sink, in order");
}

#[test]
fn two_calls_in_one_turn_are_answered_in_order() {
    let mut transport = Scripted::new(vec![
        tool_use("toolu_first", "Read"),
        tool_use("toolu_second", "Bash"),
        StreamEvent::Done {
            text: "done".to_string(),
            status: "SUCCESS".to_string(),
        },
    ]);

    let outcome = run_loop(&mut transport, &EchoRunner, 4, &mut |_| {}).expect("the loop finishes");

    let sent = transport.sent();
    let ids: Vec<&Value> = sent.iter().map(|frame| &frame["tool_use_id"]).collect();
    assert_eq!(
        ids,
        vec![&json!("toolu_first"), &json!("toolu_second")],
        "two calls must yield two results, in the order they were asked for"
    );
    assert_eq!(outcome.iterations, 2);
    assert!(!outcome.capped, "the stream ended; the cap did not stop it");
}

#[test]
fn a_result_for_an_unknown_id_is_a_stated_protocol_error() {
    let mut transport = Scripted::new(vec![tool_use("toolu_real", "Read")]);

    let error = run_loop(
        &mut transport,
        &FabricatingRunner {
            id: "toolu_invented".to_string(),
        },
        4,
        &mut |_| {},
    )
    .expect_err("a result for a call nobody made cannot be sent as if it answered one");

    assert_eq!(error.exit_code(), 4, "this is a protocol failure: {error}");
    assert!(
        error.to_string().contains("toolu_invented"),
        "the error must name the offending id: {error}"
    );
    assert!(
        error.to_string().contains("toolu_real"),
        "and the call it should have answered: {error}"
    );
    assert!(
        transport.sent().is_empty(),
        "the fabricated frame must never reach the child"
    );
}

#[test]
fn a_call_with_no_id_is_refused_rather_than_answered_empty() {
    let mut transport = Scripted::new(vec![tool_use("", "Read")]);

    let error = run_loop(&mut transport, &EchoRunner, 4, &mut |_| {})
        .expect_err("an id-less call has nothing to answer");

    assert_eq!(error.exit_code(), 4);
    assert!(error.to_string().contains("empty id"), "{error}");
    assert!(transport.sent().is_empty());
}

#[test]
fn a_failing_transport_is_an_error_not_a_finished_conversation() {
    // A transport that fails mid-stream (a broken frame, a dead child) must surface that
    // error rather than end the loop as though the model had finished talking.
    #[derive(Debug)]
    struct Failing;

    impl Transport for Failing {
        fn send(&mut self, _frame: Value) -> Result<(), PieError> {
            Ok(())
        }

        fn recv(&mut self) -> Result<Option<StreamEvent>, PieError> {
            Err(PieError::protocol(
                "scripted",
                "a frame that is not a frame",
            ))
        }
    }

    let error = run_loop(&mut Failing, &EchoRunner, 4, &mut |_| {})
        .expect_err("the transport's error is the loop's error");
    assert_eq!(error.exit_code(), 4);
    assert!(error.to_string().contains("not a frame"), "{error}");
}

#[test]
fn a_stream_that_ends_without_asking_for_anything_is_a_finished_loop() {
    let mut transport = Scripted::new(vec![StreamEvent::TextDelta("a short answer".to_string())]);

    let outcome = run_loop(&mut transport, &EchoRunner, 4, &mut |_| {}).expect("the loop finishes");

    assert_eq!(
        outcome,
        LoopOutcome {
            iterations: 0,
            capped: false,
            text: "a short answer".to_string()
        }
    );
    assert!(transport.sent().is_empty());
}

#[test]
fn the_iteration_cap_stops_a_model_that_asks_for_tools_forever_and_says_so() {
    let mut transport = EndlessCalls::default();
    let mut seen: Vec<StreamEvent> = Vec::new();

    let outcome = run_loop(&mut transport, &EchoRunner, 3, &mut |event| {
        seen.push(event);
    })
    .expect("the cap ends the loop; it does not fail it");

    assert_eq!(outcome.iterations, 3, "three calls answered, no more");
    assert!(
        outcome.capped,
        "a truncated answer reported without `capped` reads as a complete one"
    );
    assert_eq!(
        lock(&transport.sent).len(),
        3,
        "the cap must not buy the model an extra round of tool execution"
    );
    assert_eq!(
        transport.reads(),
        4,
        "three answered plus the one it refused, and then the loop let go of the stream"
    );
    assert!(
        matches!(seen.last(), Some(StreamEvent::ToolUse { id, .. }) if id == "toolu_3"),
        "the call the loop refused is still reported, or `capped` has nothing to explain: {seen:?}"
    );
}

#[test]
fn text_accumulates_across_iterations_and_every_event_reaches_the_sink_in_order() {
    let scripted = vec![
        StreamEvent::Init {
            conversation_id: Some("conv".to_string()),
            cwd: None,
            tools: vec!["Read".to_string()],
        },
        StreamEvent::TextDelta("first ".to_string()),
        tool_use("toolu_1", "Read"),
        StreamEvent::TextDelta("second".to_string()),
        StreamEvent::ToolStep {
            name: "grep".to_string(),
            state: pie::provider::ToolState::Done,
            info: None,
        },
        StreamEvent::Other {
            kind: "vendor_extra".to_string(),
            raw: json!({ "whatever": true }),
        },
        StreamEvent::Done {
            text: "first second".to_string(),
            status: "SUCCESS".to_string(),
        },
    ];

    let mut transport = Scripted::new(scripted.clone());
    let mut seen: Vec<StreamEvent> = Vec::new();
    let outcome = run_loop(&mut transport, &EchoRunner, 4, &mut |event| {
        seen.push(event);
    })
    .expect("the loop finishes");

    assert_eq!(outcome.text, "first second", "text accumulates per delta");
    assert_eq!(
        seen, scripted,
        "every event reaches the sink, in the order the transport produced them"
    );
    assert_eq!(outcome.iterations, 1);
}

// ---------------------------------------------------------------------------
// The loop against a real child
// ---------------------------------------------------------------------------

/// Padding the stand-in writes **before** it reads a single byte of stdin.
///
/// Larger than any OS pipe buffer (64 KiB on Linux, smaller on Windows), which is what
/// makes the stand-in block on stdout — and therefore the deadlock reachable at all.
const PADDING_BYTES: usize = 256 * 1024;

/// The padding content: valid frames, so the loop drains them as events rather than
/// choking on them.
fn pad_lines() -> String {
    let mut body = String::with_capacity(PADDING_BYTES + 64);
    while body.len() < PADDING_BYTES {
        body.push_str(pad_line());
    }
    body
}

/// The result the loop has to deliver: also larger than a pipe buffer, so a transport
/// that writes it synchronously blocks on the child's full stdin while the child is
/// blocked on its own full stdout.
fn bulky_output() -> String {
    "x".repeat(PADDING_BYTES)
}

fn pad_line() -> &'static str {
    "{\"type\":\"other\",\"kind\":\"pad\",\"raw\":{}}\n"
}

/// A stand-in that says one thing, writes its whole output while ignoring stdin, and
/// only then reads the frame the loop sent back. It records what it read in `seen.txt`.
///
/// The reader has to handle a line longer than a pipe buffer on both platforms: `sh`'s
/// `read` and PowerShell's `[Console]::In.ReadLine()` both do, while Windows' batch
/// `set /p` silently truncates at 1023 characters — which would make the stand-in read a
/// *prefix* of the frame and exit, turning the healthy path into a broken pipe. The long
/// frame is the point: a frame smaller than the pipe buffer cannot deadlock anything.
fn writing_first_stand_in(dir: &Path) -> PathBuf {
    let padding = dir.join("padding.txt");
    std::fs::write(&padding, pad_lines()).expect("write the padding");

    let seen = dir.join("seen.txt");

    #[cfg(windows)]
    let path = dir.join("stand-in.bat");
    #[cfg(not(windows))]
    let path = dir.join("stand-in.sh");

    #[cfg(windows)]
    let script = format!(
        "@echo off\r\n\
         echo {{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"type\":\"tool_use\",\"id\":\"toolu_deadlock\",\"name\":\"Read\",\"input\":{{}}}}]}}}}\r\n\
         type \"{}\"\r\n\
         powershell -NoProfile -NonInteractive -Command \"$l = [Console]::In.ReadLine(); [IO.File]::WriteAllText('{}', $l)\"\r\n\
         echo {{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"type\":\"text\",\"text\":\"read it\"}}]}}}}\r\n\
         echo {{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"read it\"}}\r\n",
        padding.display(),
        seen.display()
    );

    #[cfg(not(windows))]
    let script = format!(
        "#!/bin/sh\n\
         echo '{{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"type\":\"tool_use\",\"id\":\"toolu_deadlock\",\"name\":\"Read\",\"input\":{{}}}}]}}}}'\n\
         cat \"{}\"\n\
         read -r frame\n\
         printf '%s\\n' \"$frame\" > \"{}\"\n\
         echo '{{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"type\":\"text\",\"text\":\"read it\"}}]}}}}'\n\
         echo '{{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"read it\"}}'\n",
        padding.display(),
        seen.display()
    );

    std::fs::write(&path, script).expect("write the stand-in");
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("chmod the stand-in");
    }
    path
}

/// A stand-in that never answers and never exits: the shape a deadlocked or stalled child
/// has, and the one the deadline has to convert into a failure.
fn silent_stand_in(dir: &Path) -> PathBuf {
    #[cfg(windows)]
    let (path, body) = (
        dir.join("silent.bat"),
        "@echo off\r\nping -n 60 127.0.0.1 > nul\r\n",
    );
    #[cfg(not(windows))]
    let (path, body) = (dir.join("silent.sh"), "#!/bin/sh\nsleep 60\n");

    std::fs::write(&path, body).expect("write the silent stand-in");
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("chmod the silent stand-in");
    }
    path
}

/// A stand-in that records the fact it ran, so a test can prove it never did.
fn marker_stand_in(dir: &Path) -> PathBuf {
    let marker = dir.join("called.txt");

    #[cfg(windows)]
    let path = dir.join("marker.bat");
    #[cfg(not(windows))]
    let path = dir.join("marker.sh");

    #[cfg(windows)]
    let script = format!("@echo off\r\necho called> \"{}\"\r\n", marker.display());
    #[cfg(not(windows))]
    let script = format!("#!/bin/sh\necho called > \"{}\"\n", marker.display());

    std::fs::write(&path, script).expect("write the marker stand-in");
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("chmod the marker stand-in");
    }
    path
}

/// A stand-in that says one thing, writes its whole output, and then **never reads its
/// stdin at all** — a child that has already stopped caring about input (it errored, it is
/// mid-turn, it has gone away) while pie still has a frame to hand it. Paired with the
/// write-first stand-in above, it is the other half of the same hazard, and the one that
/// proves the write is off the read path: a transport whose `send` blocks on the pipe
/// hangs here, where no in-loop deadline can reach it.
fn ignoring_stdin_stand_in(dir: &Path) -> PathBuf {
    let padding = dir.join("padding.txt");
    std::fs::write(&padding, pad_lines()).expect("write the padding");

    #[cfg(windows)]
    let path = dir.join("ignores-stdin.bat");
    #[cfg(not(windows))]
    let path = dir.join("ignores-stdin.sh");

    #[cfg(windows)]
    let script = format!(
        "@echo off\r\n\
         echo {{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"type\":\"tool_use\",\"id\":\"toolu_ignored\",\"name\":\"Read\",\"input\":{{}}}}]}}}}\r\n\
         type \"{}\"\r\n\
         ping -n 60 127.0.0.1 > nul\r\n",
        padding.display()
    );

    #[cfg(not(windows))]
    let script = format!(
        "#!/bin/sh\n\
         echo '{{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"type\":\"tool_use\",\"id\":\"toolu_ignored\",\"name\":\"Read\",\"input\":{{}}}}]}}}}'\n\
         cat \"{}\"\n\
         sleep 60\n",
        padding.display()
    );

    std::fs::write(&path, script).expect("write the stand-in");
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("chmod the stand-in");
    }
    path
}

fn spec_for(program: &Path, cwd: &Path, timeout: Duration) -> RunSpec {
    RunSpec {
        backend: "claude".to_string(),
        program: program.to_path_buf(),
        args: Vec::new(),
        cwd: cwd.to_path_buf(),
        scrub: Vec::new(),
        timeout,
    }
}

/// A runner whose output is bigger than a pipe buffer, so the frame the loop sends is
/// exactly the frame a synchronous write would block on.
#[derive(Debug)]
struct BulkyRunner;

impl ToolRunner for BulkyRunner {
    fn run(&self, call: &ToolCall) -> ToolResult {
        ToolResult {
            id: call.id.clone(),
            output: bulky_output(),
            is_error: false,
        }
    }
}

/// Run the loop on its own thread behind a wall-clock watchdog.
///
/// The failures this stage is about hang *inside* the loop — a blocked write, a child that
/// stops reading — where the transport's own deadline cannot reach them, and a hung test is
/// not a failing test. Every real-child test here goes through this, so the worst case is
/// always a named RED rather than a suite that stops producing output.
fn run_watched<T, R>(
    mut transport: T,
    runner: R,
    max_iterations: usize,
    watchdog: Duration,
    seen: Arc<Mutex<Vec<StreamEvent>>>,
) -> Result<LoopOutcome, PieError>
where
    T: Transport + 'static,
    R: ToolRunner + 'static,
{
    let (done_tx, done_rx) = mpsc::channel();
    let collected = Arc::clone(&seen);
    let worker = std::thread::spawn(move || {
        let outcome = run_loop(&mut transport, &runner, max_iterations, &mut |event| {
            lock(&collected).push(event)
        });
        let _ = done_tx.send(outcome);
    });

    let outcome = match done_rx.recv_timeout(watchdog) {
        Ok(outcome) => outcome,
        Err(RecvTimeoutError::Timeout) => panic!(
            "the loop did not finish within {watchdog:?}: it is blocked, most likely writing a \
             frame larger than a pipe buffer while the child's stdout is no longer being \
             drained — the two-pipe deadlock"
        ),
        Err(RecvTimeoutError::Disconnected) => {
            panic!("the loop thread died before reporting an outcome")
        }
    };
    worker.join().expect("the loop thread joins");
    outcome
}

#[test]
fn the_loop_finishes_against_a_child_that_will_not_read_its_stdin_until_it_has_written_its_stdout()
{
    // The classic two-pipe deadlock: the child writes more than a pipe buffer before it
    // reads anything, and the frame it is sent back is bigger than a pipe buffer too. A
    // transport that lets the loop's own thread read the child's stdout blocks in `send`,
    // stops draining, and neither side ever moves again. The assertion is that this test
    // *finishes*.
    let dir = tempfile::tempdir().expect("temp dir");
    let stand_in = writing_first_stand_in(dir.path());

    let started = Instant::now();
    let deadline = Duration::from_secs(15);
    let seen: Arc<Mutex<Vec<StreamEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let transport = ChildTransport::spawn(&spec_for(&stand_in, dir.path(), deadline))
        .expect("the stand-in spawns");

    let outcome = run_watched(
        transport,
        BulkyRunner,
        4,
        deadline + Duration::from_secs(10),
        Arc::clone(&seen),
    )
    .expect("the loop must finish against a child that writes first and reads second");

    assert!(
        started.elapsed() < deadline,
        "the loop reported success only by timing out first"
    );
    assert_eq!(outcome.iterations, 1, "one call, one result");

    // The child read the frame: the strongest evidence that the write *landed* rather
    // than merely being queued.
    let read_back =
        std::fs::read_to_string(dir.path().join("seen.txt")).expect("the child wrote it");
    assert!(
        read_back.contains("toolu_deadlock"),
        "the child never read the result we sent: {:?}",
        &read_back[..read_back.len().min(200)]
    );
    assert!(
        read_back.len() > PADDING_BYTES / 2,
        "the frame that arrived was truncated ({} bytes), so the child did not read the \
         whole thing and this test would not be exercising a frame bigger than a pipe \
         buffer",
        read_back.len()
    );

    let seen = lock(&seen).clone();
    assert!(
        seen.iter()
            .any(|event| matches!(event, StreamEvent::TextDelta(text) if text == "read it")),
        "the child's answer after reading the frame must reach the sink"
    );
    assert!(
        seen.iter()
            .any(|event| matches!(event, StreamEvent::Done { status, .. } if status == "success")),
        "the terminal event must reach the sink too"
    );
}

#[test]
fn a_child_that_never_reads_its_stdin_cannot_stall_the_loop() {
    // The other half of the same hazard, and the one that shows the write really is off the
    // read path: this child writes its whole output and then reads nothing at all. A
    // transport that writes the frame on the loop's own thread blocks in `send` forever,
    // where no deadline can reach it; because the frame is handed to a writer thread
    // instead, the loop keeps draining stdout, reaches its deadline when the child goes
    // quiet, and fails the run *within* it.
    let dir = tempfile::tempdir().expect("temp dir");
    let stand_in = ignoring_stdin_stand_in(dir.path());

    let deadline = Duration::from_secs(2);
    let seen: Arc<Mutex<Vec<StreamEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let transport = ChildTransport::spawn(&spec_for(&stand_in, dir.path(), deadline))
        .expect("the stand-in spawns");

    let started = Instant::now();
    let error = run_watched(
        transport,
        BulkyRunner,
        4,
        Duration::from_secs(20),
        Arc::clone(&seen),
    )
    .expect_err("a child that stops answering is a failed run, not a hung one");

    assert!(
        matches!(error, PieError::Timeout { .. }),
        "the run must fail on the deadline, got {error:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "the deadline must fire promptly, took {:?}",
        started.elapsed()
    );
    // The loop drained the child's whole output while the frame sat unsent: that is the
    // read path staying alive, and the thing a blocking write would have stopped after
    // one event.
    let drained = lock(&seen).len();
    assert!(
        drained > 1000,
        "the loop stopped reading stdout while the frame was queued, seeing {drained} events"
    );
}

/// A transport that reads and writes the child's pipes **on the calling thread** — the
/// naive shape the two-pipe deadlock comes from, kept as the control for the test above.
///
/// A deadlock test that passes for the wrong reason is worse than no test, so this asks
/// the question the other way round: if this control does *not* block, the stand-in is not
/// exercising the classic deadlock at all and the test above is asserting nothing. It owns
/// both pipes on purpose and spawns its own child — a test-only stand-in, never a vendor
/// CLI, so no quota is spent (`ACCEPTANCE.md` §17).
#[derive(Debug)]
struct NaiveTransport {
    child: std::process::Child,
    stdin: Option<std::process::ChildStdin>,
    stdout: BufReader<std::process::ChildStdout>,
    pending: VecDeque<StreamEvent>,
}

impl NaiveTransport {
    fn spawn(spec: &RunSpec) -> Self {
        let mut child = std::process::Command::new(&spec.program)
            .current_dir(&spec.cwd)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("the stand-in spawns");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = BufReader::new(child.stdout.take().expect("stdout"));
        Self {
            child,
            stdin: Some(stdin),
            stdout,
            pending: VecDeque::new(),
        }
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }
}

impl Transport for NaiveTransport {
    fn send(&mut self, frame: Value) -> Result<(), PieError> {
        let stdin = self.stdin.as_mut().expect("stdin");
        // The blocking write: nothing drains stdout while this is on the wire.
        stdin
            .write_all(format!("{frame}\n").as_bytes())
            .and_then(|()| stdin.flush())
            .map_err(PieError::Io)
    }

    fn recv(&mut self) -> Result<Option<StreamEvent>, PieError> {
        if self.pending.is_empty() {
            let mut line = String::new();
            if self.stdout.read_line(&mut line).map_err(PieError::Io)? == 0 {
                return Ok(None);
            }
            self.pending
                .extend(pie::tool_loop::events_from_sdk_line(&line)?);
        }
        Ok(self.pending.pop_front())
    }
}

/// Kill a stand-in a deliberately deadlocked transport still holds.
fn kill_stand_in(pid: u32) {
    let pid = pid.to_string();
    // The helper's own chatter is noise in the test output, and its exit status is not the
    // assertion.
    #[cfg(windows)]
    let _ = std::process::Command::new("taskkill")
        .args(["/PID", &pid, "/T", "/F"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    #[cfg(not(windows))]
    let _ = std::process::Command::new("kill")
        .args(["-9", &pid])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

#[test]
fn the_stand_in_really_can_deadlock_a_transport_that_reads_and_writes_on_one_thread() {
    // The control for `the_loop_finishes_against_a_child_that_will_not_read_its_stdin_…`:
    // the same stand-in, the same frame larger than a pipe buffer, but a transport whose
    // `recv` reads the child's stdout on the loop's own thread — so the moment it blocks in
    // `send`, the drain stops, the child blocks on a full stdout pipe, and neither side can
    // move. If this control ever completes, the stand-in has lost the property the deadlock
    // test is named after.
    let dir = tempfile::tempdir().expect("temp dir");
    let stand_in = writing_first_stand_in(dir.path());

    let mut naive =
        NaiveTransport::spawn(&spec_for(&stand_in, dir.path(), Duration::from_secs(60)));
    let pid = naive.pid();
    let (done_tx, done_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = done_tx.send(run_loop(&mut naive, &BulkyRunner, 4, &mut |_| {}));
    });

    let outcome = done_rx.recv_timeout(Duration::from_secs(5));
    kill_stand_in(pid);

    match outcome {
        Err(RecvTimeoutError::Timeout) => {}
        Ok(result) => panic!(
            "the stand-in did not deadlock a single-threaded transport ({result:?}), so the \
             deadlock test above is not exercising the two-pipe deadlock"
        ),
        Err(RecvTimeoutError::Disconnected) => {
            panic!("the naive loop thread died without reporting")
        }
    }
}

#[test]
fn a_child_that_never_answers_fails_the_run_instead_of_hanging() {
    // The same machinery the deadlock test relies on: a stalled child is a *failing* test,
    // never a blocked one. Without this, a deadlocked loop would hang the suite and read as
    // "still running".
    // The same machinery the deadlock test relies on: a stalled child is a *failing*
    // test, never a blocked one. Without this, a deadlocked loop would hang the suite
    // and read as "still running".
    let dir = tempfile::tempdir().expect("temp dir");
    let stand_in = silent_stand_in(dir.path());

    let started = Instant::now();
    let mut transport =
        ChildTransport::spawn(&spec_for(&stand_in, dir.path(), Duration::from_secs(2)))
            .expect("the stand-in spawns");
    let error =
        run_loop(&mut transport, &EchoRunner, 4, &mut |_| {}).expect_err("a stall is a failure");

    assert!(
        matches!(error, PieError::Timeout { .. }),
        "a child that stops talking is a timeout, got {error:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "the deadline must fire promptly, took {:?}",
        started.elapsed()
    );
}

#[test]
fn a_malformed_frame_from_a_child_is_a_stated_error_and_the_child_is_not_left_running() {
    let dir = tempfile::tempdir().expect("temp dir");

    #[cfg(windows)]
    let path = dir.path().join("malformed.bat");
    #[cfg(not(windows))]
    let path = dir.path().join("malformed.sh");

    #[cfg(windows)]
    let script =
        "@echo off\r\necho {\"type\":\"assistant\",\"message\":{\"content\":\"not an array\"}}\r\nping -n 60 127.0.0.1 > nul\r\n";
    #[cfg(not(windows))]
    let script =
        "#!/bin/sh\necho '{\"type\":\"assistant\",\"message\":{\"content\":\"not an array\"}}'\nsleep 60\n";

    std::fs::write(&path, script).expect("write the malformed stand-in");
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("chmod the malformed stand-in");
    }

    let started = Instant::now();
    let mut transport =
        ChildTransport::spawn(&spec_for(&path, dir.path(), Duration::from_secs(30)))
            .expect("the stand-in spawns");
    let error = run_loop(&mut transport, &EchoRunner, 4, &mut |_| {})
        .expect_err("a frame that claims a shape it does not have is an error");

    assert_eq!(error.exit_code(), 4, "a protocol error: {error}");
    assert!(error.to_string().contains("content array"), "{error}");
    // The child is still sleeping; dropping the transport must kill it rather than wait
    // out its 60 seconds (`RISKS.md` §6 — a child that outlives its parent).
    drop(transport);
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "the child was not killed on the way out, took {:?}",
        started.elapsed()
    );
}

// ---------------------------------------------------------------------------
// Gating
// ---------------------------------------------------------------------------

#[test]
fn a_delegate_mode_backend_is_refused_by_name_as_a_config_error() {
    for backend in ["agy", "codex"] {
        let caps = capabilities(backend);
        assert!(
            !caps.tool_loop,
            "{backend} is one-shot (RISKS.md §1); this test is only meaningful while it is"
        );

        let error = check_supported(backend, &caps)
            .expect_err("asking for the loop on a delegate-mode backend is refused");
        assert_eq!(error.exit_code(), 2, "a config error, not a hang: {error}");
        assert!(
            error.to_string().contains(backend),
            "the refusal must name the backend that cannot do it: {error}"
        );
    }

    // And the gate is not a blanket refusal: a backend that advertises the loop passes.
    let capable = Capabilities {
        tool_loop: true,
        ..Capabilities::default()
    };
    assert!(check_supported("claude", &capable).is_ok());
}

/// The caller's order: gate first, spawn second, loop last. Written as a function so the
/// test exercises the order rather than asserting it in a comment.
fn gated_run(
    backend: &str,
    caps: &Capabilities,
    spec: &RunSpec,
    transport: &mut dyn Transport,
) -> Result<LoopOutcome, PieError> {
    check_supported(backend, caps)?;
    let _child = ChildTransport::spawn(spec)?;
    run_loop(transport, &EchoRunner, 4, &mut |_| {})
}

#[test]
fn the_loop_never_spawns_or_reads_when_the_backend_is_not_supported() {
    let dir = tempfile::tempdir().expect("temp dir");
    let stand_in = marker_stand_in(dir.path());
    let marker = dir.path().join("called.txt");
    let spec = spec_for(&stand_in, dir.path(), Duration::from_secs(10));

    // Refused: codex is delegate mode, so neither the child nor the event stream is
    // touched.
    let mut refused = Scripted::new(vec![tool_use("toolu_1", "Read")]);
    let error = gated_run("codex", &capabilities("codex"), &spec, &mut refused)
        .expect_err("codex runs in delegate mode");
    assert_eq!(error.exit_code(), 2, "{error}");
    assert!(
        !marker.exists(),
        "the refusal must happen before a child is spawned, or the loop hangs against a \
         child that will never accept a tool_result"
    );
    assert_eq!(refused.reads(), 0, "not one event may be asked for");
    assert!(refused.sent().is_empty(), "and not one frame may be sent");

    // The control: with the capability advertised, the very same call path does spawn the
    // stand-in and does read from the transport. Without it, the assertions above would
    // hold for a helper that simply never got as far as spawning anything.
    let capable = Capabilities {
        tool_loop: true,
        ..Capabilities::default()
    };
    let mut allowed = Scripted::new(vec![tool_use("toolu_1", "Read")]);
    let outcome = gated_run("codex", &capable, &spec, &mut allowed)
        .expect("a backend that advertises the loop passes the gate");
    assert!(
        marker.exists(),
        "the control must actually spawn the stand-in"
    );
    assert!(
        allowed.reads() > 0,
        "and actually ask the transport for events"
    );
    assert_eq!(outcome.iterations, 1);
}

#[test]
fn a_frame_for_a_tool_result_is_what_the_child_reads() {
    // The neutral frame and the wire shape are asserted separately, because the loop
    // depends on the first and claude depends on the second.
    let frame = tool_result_frame(&ToolResult {
        id: "toolu_1".to_string(),
        output: "the output".to_string(),
        is_error: false,
    });
    assert_eq!(frame["type"], "tool_result");
    assert_eq!(frame["tool_use_id"], "toolu_1");
    assert_eq!(frame["content"], "the output");
}

/// **I2 is about every child, and M9 added a second spawn path.**
///
/// `ChildTransport::spawn` *mirrors* `process.rs`'s policy — hidden window, scrubbed env, a
/// deadline, a kill tree — rather than reusing it, because `run_streaming` hands the child
/// `stdin(null)` and the loop needs a pipe. A mirrored invariant is one that can stop
/// holding without anything failing, and I2 is the invariant that decides whether a vendor
/// CLI bills a subscription or an API key (`RISKS.md` §10).
///
/// So this asserts the behaviour rather than reading the code: a stand-in writes the value
/// of a scrubbed variable to a file, and the file must not contain it. It would fail if the
/// mirror ever lost the loop that applies the scrub, which is exactly the drift a comment
/// cannot catch.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_loops_own_spawn_path_scrubs_the_environment_too() {
    use pie::process::RunSpec;
    use pie::tool_loop::ChildTransport;

    const SECRET_VAR: &str = "PIE_TEST_TOOL_LOOP_SECRET";
    let dir = tempfile::tempdir().expect("a scratch dir");
    let written = dir.path().join("leaked.txt");

    #[cfg(windows)]
    let script = dir.path().join("echo-env.bat");
    #[cfg(windows)]
    let body = format!(
        "@echo off\r\necho %{SECRET_VAR}%> \"{}\"\r\n",
        written.display()
    );
    #[cfg(not(windows))]
    let script = dir.path().join("echo-env.sh");
    #[cfg(not(windows))]
    let body = format!(
        "#!/bin/sh\nprintf '%s' \"${SECRET_VAR}\" > '{}'\n",
        written.display()
    );
    std::fs::write(&script, body).expect("write the env stand-in");
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
            .expect("chmod the stand-in");
    }

    // Set it in *our* environment, so a child that inherits anything would see it.
    std::env::set_var(SECRET_VAR, "sk-must-not-reach-the-child");

    let spec = RunSpec {
        backend: "tool-loop-scrub-probe".to_string(),
        program: script,
        args: Vec::new(),
        cwd: dir.path().to_path_buf(),
        scrub: vec![SECRET_VAR.to_string()],
        timeout: std::time::Duration::from_secs(30),
    };

    {
        let _transport = ChildTransport::spawn(&spec).expect("the stand-in spawns");
        // The stand-in writes and exits; give it a moment on a loaded CI box.
        for _ in 0..100 {
            if written.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }
    std::env::remove_var(SECRET_VAR);

    let content = std::fs::read_to_string(&written).unwrap_or_default();
    assert!(
        !content.contains("sk-must-not-reach-the-child"),
        "the child inherited `{SECRET_VAR}`: the loop's second spawn path did not scrub it \
         (I2, RISKS.md section 10). Got: {content:?}"
    );
}
