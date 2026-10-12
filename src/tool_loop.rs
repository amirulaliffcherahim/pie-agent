//! The `tool_use` → `tool_result` loop (`DESIGN.md` §18).
//!
//! `RISKS.md` §1 is why M0–M8 are delegate-mode: `agy --print` and `codex exec` are
//! one-shot, they run their own tools, and they **cannot** accept an injected
//! `tool_result`. claude in SDK mode can, so this is where pie finally owns a loop — and
//! it is the code path M10's HTTP backends reuse (`RISKS.md` §8). One implementation, two
//! consumers.
//!
//! ## Scope, and why it is drawn here
//!
//! This module is the **loop**, not a tool suite: a [`ToolRunner`] arrives from the
//! caller and this crate ships no concrete tools. `PLAN.md` §2's `agent/tools.rs`
//! (read/bash/edit/write/grep/find/ls) is a separate concern with its own design
//! questions — what a `bash` tool may do unattended is not a decision to make in passing —
//! and inventing one here would put uninspected code on the paid path.
//!
//! ## The four things that make a loop wrong, each with a seam
//!
//! 1. **Matching.** A `tool_result` must name the `tool_use_id` it answers. An answer for
//!    an id nobody asked about is a **stated protocol error**, never a dropped frame —
//!    dropping it would leave the model waiting for a result that will never come.
//! 2. **Deadlock.** Writing the child's stdin while reading its stdout is the classic
//!    two-pipe deadlock. [`Transport`] is the seam that lets a test prove the loop
//!    *finishes* against something that refuses to read stdin until it has written
//!    everything.
//! 3. **Bounding.** A model that asks for tools forever must be stopped by an iteration
//!    cap, and **the cap's effect must be reported** ([`LoopOutcome::capped`]) rather than
//!    swallowed — a silently truncated answer reads as a complete one.
//! 4. **Gating.** The loop cannot work against a delegate-mode backend, so
//!    [`check_supported`] turns "asked for the loop on agy" into a config error instead of
//!    a hang.
//!
//! ## Frames are neutral here, and the transport owns the wire shape
//!
//! The loop hands the transport a **neutral** frame ([`tool_result_frame`]:
//! `{"type":"tool_result","tool_use_id":…,"content":…,"is_error":…}`) rather than
//! claude's own envelope. `claude`'s stream-json input is a *user message* wrapping that
//! block, an HTTP backend's is a `{"role":"tool","tool_call_id":…}` object, and a
//! scripted test transport's is whatever the test asserts on. The loop therefore carries
//! no vendor knowledge, and [`ChildTransport`] does the one translation claude needs —
//! which is also how M10 reuses this loop instead of writing a second one.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::error::{truncate, PieError, Result};
use crate::process::{self, RunSpec};
use crate::provider::{Capabilities, StreamEvent};

/// One tool the CLI asked pie to run.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    /// The vendor's id, echoed back by the matching result.
    pub id: String,
    /// Tool name, as the vendor spelled it.
    pub name: String,
    /// Arguments, opaque to pie — it routes them, it does not interpret them.
    pub input: Value,
}

/// What pie sends back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolResult {
    /// The id of the call this answers. The vendor matches on it, so a wrong id is a
    /// result for a call that never happened.
    pub id: String,
    /// The output, already stringified.
    pub output: String,
    /// Whether the tool failed. Sent explicitly rather than encoded in the output,
    /// because a tool that legitimately prints an error message is not a failed tool.
    pub is_error: bool,
}

/// Runs one call. Injected, so this module ships no tools and every test is offline.
pub trait ToolRunner: Send + Sync {
    /// Run a call and return its result, with `id` already set to the call's.
    fn run(&self, call: &ToolCall) -> ToolResult;
}

/// How the loop talks to the model, so the loop itself needs no process.
///
/// A concrete implementation writes to a child's stdin and reads its stdout — the
/// deadlock-prone half — while a scripted one is a `Vec` behind a mutex. The loop cannot
/// tell them apart, which is what makes the delicate part testable.
pub trait Transport: Send + Sync {
    /// Send one frame.
    fn send(&mut self, frame: Value) -> Result<()>;
    /// The next event, or `None` when the stream has ended.
    fn recv(&mut self) -> Result<Option<StreamEvent>>;
}

/// What a finished loop did, including how it stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoopOutcome {
    /// How many model turns the loop took.
    pub iterations: usize,
    /// Whether the iteration cap stopped it. **Reported, never swallowed**: a capped run
    /// produced a partial answer, and a caller that cannot tell has been told a lie.
    pub capped: bool,
    /// The answer text accumulated across the turns.
    pub text: String,
}

/// Refuse the loop on a backend that cannot accept an injected result.
///
/// `RISKS.md` §1: `agy --print` and `codex exec` are one-shot, so asking for the loop there
/// is a **config error naming the backend** rather than a hang or a silently ignored flag.
pub fn check_supported(backend: &str, caps: &Capabilities) -> Result<()> {
    if caps.tool_loop {
        return Ok(());
    }
    Err(PieError::Config(format!(
        "`{backend}` runs in delegate mode: it performs its own tool calls and cannot accept \
         a `tool_result` from pie, so the tool loop is not available on it (RISKS.md §1). Use a \
         backend with `tool_loop = true`, or drop the request to run tools"
    )))
}

/// Drive one conversation, answering tool calls until the model stops asking.
///
/// The control flow, per event the transport yields:
///
/// 1. the event is forwarded to `sink` **before** the loop acts on it, so a `tool_use` the
///    loop refuses to answer is still reported — the caller can see the call that stopped
///    it. A hook that fired only on the calls the loop agreed to run would hide exactly
///    the event a `capped` outcome is about.
/// 2. a [`StreamEvent::TextDelta`] is appended to [`LoopOutcome::text`]. `Done` carries
///    the whole answer again, so appending that too would double every result.
/// 3. a [`StreamEvent::ToolUse`] is answered: the runner executes it, its result's id is
///    checked against the call's, and the neutral frame goes back through the transport.
/// 4. a `ToolUse` that arrives after `max_iterations` calls have been answered ends the
///    loop with [`LoopOutcome::capped`] set **without** being answered. Answering it and
///    then stopping was considered and rejected: the extra call would leave the child
///    waiting for a result that never arrives, and the cap would have bought one more
///    round of tool execution than the caller allowed.
///
/// The loop ends when the transport reports the stream has ended. `iterations` counts
/// answered calls rather than model turns, because an answered call is the unit that can
/// repeat forever — the thing the cap exists to bound. A turn that asks for two tools
/// therefore costs two iterations, and the cap cannot be walked past by asking for tools
/// in parallel.
///
/// `max_iterations == 0` refuses the first call; that is the degenerate case of the same
/// rule, not a special case.
pub fn run_loop(
    transport: &mut dyn Transport,
    runner: &dyn ToolRunner,
    max_iterations: usize,
    sink: &mut dyn FnMut(StreamEvent),
) -> Result<LoopOutcome> {
    let mut outcome = LoopOutcome {
        iterations: 0,
        capped: false,
        text: String::new(),
    };

    while let Some(event) = transport.recv()? {
        let call = match &event {
            StreamEvent::TextDelta(chunk) => {
                outcome.text.push_str(chunk);
                None
            }
            StreamEvent::ToolUse { id, name, input } => Some(ToolCall {
                id: id.clone(),
                name: name.clone(),
                input: input.clone(),
            }),
            _ => None,
        };

        sink(event);

        let Some(call) = call else { continue };

        // An id is the only handle the vendor has on a result, so a call without one can
        // never be answered: sending an empty id would produce a frame the model cannot
        // match, which reads as a stalled conversation rather than as the broken frame it
        // is.
        if call.id.trim().is_empty() {
            return Err(PieError::protocol(
                "tool_loop",
                format!(
                    "a `tool_use` for tool `{}` arrived with an empty id; there is nothing to \
                     answer, and a `tool_result` with no id is a result the model cannot match",
                    truncate(&call.name, 120)
                ),
            ));
        }

        if outcome.iterations >= max_iterations {
            outcome.capped = true;
            break;
        }

        let result = runner.run(&call);
        if result.id != call.id {
            // The one place a `tool_result` can name an id nobody asked about: a runner
            // that mints its own id. Sending it would answer a call that never happened and
            // leave `call` unanswered forever.
            return Err(PieError::protocol(
                "tool_loop",
                format!(
                    "a tool result names `{}`, which is not the id of the call it answers \
                     (`{}`); a result for an unknown id is a frame the model cannot match, and \
                     the call it was meant to answer stays open",
                    process::redact(&truncate(&result.id, 120)),
                    process::redact(&truncate(&call.id, 120)),
                ),
            ));
        }

        transport.send(tool_result_frame(&result))?;
        outcome.iterations += 1;
    }

    Ok(outcome)
}

/// The neutral frame that answers a call.
///
/// Neutral on purpose: it is neither claude's `user` envelope nor an HTTP backend's
/// `role: "tool"` message. [`Transport`] implementations translate it, which is what
/// keeps the loop free of vendor knowledge (`RISKS.md` §8 — one loop, two consumers).
///
/// ```json
/// {"type":"tool_result","tool_use_id":"…","content":"…","is_error":false}
/// ```
pub fn tool_result_frame(result: &ToolResult) -> Value {
    json!({
        "type": "tool_result",
        "tool_use_id": result.id,
        "content": result.output,
        "is_error": result.is_error,
    })
}

/// Map one line of claude's `stream-json` output onto events.
///
/// This is the **minimum** the loop needs, not the full envelope: `system`/`init`,
/// `usage` and every unknown kind arrive as [`StreamEvent::Other`] so nothing is lost
/// (I5), and the richer router belongs to `provider::claude`, which is the module that
/// owns claude's schema. A line that is not JSON at all — a banner, a warning, a progress
/// bar — becomes [`StreamEvent::Unparsed`] rather than an error, for the same reason
/// `process::read_line_capped` tolerates a stray byte: a CLI printing one line we did not
/// expect is not a reason to discard a good conversation.
///
/// A frame that *does* claim a shape it does not have is a stated error instead. Silently
/// skipping an `assistant` frame whose `content` is not an array would drop whatever tool
/// call it held, and a dropped call is a model waiting for a result that never comes —
/// the first of the four failures this module exists to prevent.
///
/// One line can yield several events: an assistant message may hold both text and a tool
/// call (`ACCEPTANCE.md` §17).
pub fn events_from_sdk_line(line: &str) -> Result<Vec<StreamEvent>> {
    parse_sdk_line("stream-json", line)
}

/// [`events_from_sdk_line`] with the caller's backend name in any error.
fn parse_sdk_line(backend: &str, line: &str) -> Result<Vec<StreamEvent>> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        // A blank line is not a frame and not a failure: a child that ends its output
        // with a newline would otherwise turn a healthy turn into a protocol error.
        return Ok(Vec::new());
    }

    let value: Value = match serde_json::from_str(trimmed) {
        Ok(value) => value,
        Err(_) => return Ok(vec![StreamEvent::Unparsed(trimmed.to_string())]),
    };

    let Some(object) = value.as_object() else {
        // Valid JSON, but not a frame: a bare array or string on stdout is a banner in
        // JSON clothing.
        return Ok(vec![StreamEvent::Unparsed(trimmed.to_string())]);
    };

    match object.get("type").and_then(Value::as_str) {
        Some("assistant") => {
            let malformed = |why: &str| {
                PieError::protocol(
                    backend,
                    format!(
                        "an `assistant` frame {why}: {}. A frame that may hold a tool call cannot \
                         be skipped — a dropped call leaves the model waiting for a result that \
                         will never come",
                        process::redact(&truncate(trimmed, 300))
                    ),
                )
            };

            let blocks = object
                .get("message")
                .and_then(|message| message.get("content"))
                .and_then(Value::as_array)
                .ok_or_else(|| malformed("carried no content array"))?;

            let mut events = Vec::with_capacity(blocks.len());
            for block in blocks {
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(text) = block.get("text").and_then(Value::as_str) {
                            events.push(StreamEvent::TextDelta(text.to_string()));
                        }
                    }
                    Some("tool_use") => {
                        let id = block
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        if id.trim().is_empty() {
                            return Err(malformed("held a `tool_use` block with no id"));
                        }
                        events.push(StreamEvent::ToolUse {
                            id,
                            name: block
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                            input: block.get("input").cloned().unwrap_or(Value::Null),
                        });
                    }
                    // A block type this mapping does not know (thinking, images, a
                    // vendor addition after an auto-update, `RISKS.md` §3) is forwarded
                    // rather than dropped, so a new block kind cannot silently swallow a
                    // part of the stream.
                    _ => events.push(StreamEvent::Other {
                        kind: "assistant_block".to_string(),
                        raw: block.clone(),
                    }),
                }
            }
            Ok(events)
        }
        Some("result") => {
            let status = object
                .get("subtype")
                .and_then(Value::as_str)
                .or_else(|| object.get("status").and_then(Value::as_str))
                .unwrap_or("unknown")
                .to_string();
            Ok(vec![StreamEvent::Done {
                text: object
                    .get("result")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                status,
            }])
        }
        // Every other kind — `system`, `user`, a vendor addition — is preserved whole so
        // `--json` can carry it and a stricter consumer can re-parse it.
        Some(other) => Ok(vec![StreamEvent::Other {
            kind: other.to_string(),
            raw: value,
        }]),
        None => Ok(vec![StreamEvent::Other {
            kind: "untyped".to_string(),
            raw: value,
        }]),
    }
}

/// One line of the child's stdout, or the read that failed.
#[derive(Debug)]
enum Incoming {
    /// A complete line, lossily decoded.
    Line(String),
    /// The reader stopped; the loop reports this instead of a silent end of stream.
    Failed(std::io::Error),
}

/// A [`Transport`] over a spawned child: frames in on stdin, events out on stdout.
///
/// ## How it avoids the two-pipe deadlock, and why that is structural
///
/// The deadlock is: pie blocks writing a frame into the child's stdin while the child
/// blocks writing into a full stdout pipe that pie has stopped draining. Both are then
/// waiting on the other. It needs a frame larger than the OS pipe buffer, and it is
/// reachable with a real tool result — a `read` of a large file is exactly that.
///
/// Three structural choices make it unreachable here:
///
/// 1. **`send` never touches the pipe.** It serialises the frame and hands the line to a
///    writer thread, then returns. The read path can therefore always keep draining
///    stdout, which is what unblocks the child.
/// 2. **A reader thread owns the blocking read**, pushing lines into a channel that
///    `recv` drains. That is what makes the deadline possible at all: a blocking read on
///    a pipe cannot be cancelled, but `recv_timeout` on a channel can — so a child that
///    stops talking fails the run with [`PieError::Timeout`] instead of hanging it.
/// 3. **The frame queue is bounded by the iteration cap.** One frame is queued per
///    answered call and the loop does not answer another until the child has replied, so
///    the queue cannot grow without bound — which is why an unbounded channel is safe
///    here and a bounded one would only add a way to block the read path.
///
/// Spawning follows `process.rs` (`DESIGN.md` §6): hidden window, scrubbed env, a
/// whole-turn deadline, a bounded stderr drain, and a kill tree on the way out. It cannot
/// *use* `process::run_streaming`, because that closes the child's stdin and only
/// delivers stdout — one-way by construction, which is the whole point of a delegate-mode
/// turn and the opposite of what a loop needs.
#[derive(Debug)]
pub struct ChildTransport {
    backend: String,
    timeout: Duration,
    child: Child,
    /// Dropped to end the writer thread, which then drops the child's stdin (EOF).
    frames: Option<mpsc::Sender<String>>,
    incoming: Mutex<mpsc::Receiver<Incoming>>,
    /// Events parsed from a line, waiting to be returned one at a time.
    pending: VecDeque<StreamEvent>,
    /// The first failed write to the child's stdin, surfaced on the next `send`/at EOF.
    writer_error: Arc<Mutex<Option<std::io::Error>>>,
    /// Last bytes of stderr, for a caller that wants to explain a failure.
    stderr_tail: Arc<Mutex<String>>,
    deadline: Instant,
    killed: bool,
}

impl ChildTransport {
    /// Spawn `spec` and wire its stdin/stdout into a transport.
    pub fn spawn(spec: &RunSpec) -> Result<Self> {
        let mut cmd = Command::new(&spec.program);
        cmd.args(&spec.args)
            .current_dir(&spec.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(process::CREATE_NO_WINDOW);
        }

        // Own process group on unix, so the kill below reaches the whole tree rather
        // than the direct child (`process.rs`, `DESIGN.md` §1).
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }

        for key in &spec.scrub {
            cmd.env_remove(key);
        }

        let mut child = cmd.spawn().map_err(|source| PieError::Spawn {
            backend: spec.backend.clone(),
            source,
        })?;

        let stdin = child.stdin.take().ok_or_else(|| {
            PieError::protocol(
                &spec.backend,
                "child produced no stdin handle to write frames to",
            )
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            PieError::protocol(
                &spec.backend,
                "child produced no stdout handle to read events from",
            )
        })?;
        let stderr = child.stderr.take().ok_or_else(|| {
            PieError::protocol(&spec.backend, "child produced no stderr handle to drain")
        })?;

        // `Instant + Duration` panics on overflow and the timeout is an unvalidated u64
        // from `--timeout` / `PIE_TIMEOUT_SECS`, so a nonsense value must become a config
        // error (`process.rs` does the same).
        let deadline = Instant::now().checked_add(spec.timeout).ok_or_else(|| {
            PieError::Config(format!(
                "timeout of {}s is too large to schedule",
                spec.timeout.as_secs()
            ))
        })?;

        let writer_error: Arc<Mutex<Option<std::io::Error>>> = Arc::new(Mutex::new(None));
        let stderr_tail: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));

        // stderr must be drained even though the loop never reads it: a child that fills a
        // full stderr pipe blocks, and the loop would then wait for stdout that can never
        // come (`process.rs`'s reason for draining it too). Detached: joining it in `Drop`
        // would hang on a grandchild that inherited the handle, and a hung drop is worse
        // than a thread that ends when the pipe closes.
        {
            let tail = Arc::clone(&stderr_tail);
            let _ = std::thread::spawn(move || drain_tail(stderr, tail));
        }

        let (frame_tx, frame_rx) = mpsc::channel::<String>();
        {
            let errors = Arc::clone(&writer_error);
            let mut stdin = stdin;
            let _ = std::thread::spawn(move || {
                while let Ok(line) = frame_rx.recv() {
                    if let Err(error) = stdin
                        .write_all(line.as_bytes())
                        .and_then(|()| stdin.flush())
                    {
                        // Recorded, not returned: `send` has already told the loop the
                        // frame was accepted, so the failure has to be reported from
                        // there — a result the child never read must not be followed by a
                        // cheerful end of stream.
                        if let Ok(mut slot) = errors.lock() {
                            *slot = Some(error);
                        }
                        break;
                    }
                }
                // Dropping `stdin` here closes the child's stdin.
            });
        }

        let (line_tx, line_rx) = mpsc::channel::<Incoming>();
        {
            let mut reader = BufReader::new(stdout);
            let _ = std::thread::spawn(move || loop {
                match read_capped_line(&mut reader) {
                    Ok(Some(line)) => {
                        if line_tx.send(Incoming::Line(line)).is_err() {
                            break; // the transport is gone
                        }
                    }
                    Ok(None) => break,
                    Err(error) => {
                        let _ = line_tx.send(Incoming::Failed(error));
                        break;
                    }
                }
            });
        }

        Ok(Self {
            backend: spec.backend.clone(),
            timeout: spec.timeout,
            child,
            frames: Some(frame_tx),
            incoming: Mutex::new(line_rx),
            pending: VecDeque::new(),
            writer_error,
            stderr_tail,
            deadline,
            killed: false,
        })
    }

    /// The last bytes of the child's stderr, for a diagnostic.
    pub fn stderr_tail(&self) -> String {
        lock(&self.stderr_tail).clone()
    }

    /// The first failed write to the child's stdin, taken once.
    fn take_writer_error(&self) -> Option<std::io::Error> {
        lock(&self.writer_error).take()
    }

    /// Next line from the child, or `None` at end of stream.
    fn next_line(&mut self) -> Result<Option<String>> {
        // A zero-length `recv_timeout` returns `Timeout` immediately, which would turn a
        // child that exited right at the deadline into a spurious timeout, so the last
        // instant is spent on `try_recv` instead.
        let remaining = self.deadline.checked_duration_since(Instant::now());
        let received = match remaining {
            Some(remaining) if !remaining.is_zero() => lock(&self.incoming).recv_timeout(remaining),
            _ => lock(&self.incoming)
                .try_recv()
                .map_err(|error| match error {
                    mpsc::TryRecvError::Empty => mpsc::RecvTimeoutError::Timeout,
                    mpsc::TryRecvError::Disconnected => mpsc::RecvTimeoutError::Disconnected,
                }),
        };

        match received {
            Ok(Incoming::Line(line)) => Ok(Some(line)),
            Ok(Incoming::Failed(error)) => Err(PieError::Io(error)),
            Err(mpsc::RecvTimeoutError::Disconnected) => Ok(None),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // The bounded tail is what makes a timeout explainable, and it is a vendor's
                // own stderr, so it is redacted on the way out (I10) and truncated before
                // it can become the log line.
                let tail = self.stderr_tail();
                let trimmed = tail.trim();
                if !trimmed.is_empty() {
                    tracing::warn!(
                        backend = %self.backend,
                        secs = self.timeout.as_secs(),
                        stderr = %truncate(&process::redact(trimmed), 400),
                        "the tool-loop child went quiet until its deadline"
                    );
                }
                self.kill();
                Err(PieError::Timeout {
                    backend: self.backend.clone(),
                    secs: self.timeout.as_secs(),
                })
            }
        }
    }

    /// Kill the child and everything it spawned, once.
    fn kill(&mut self) {
        if self.killed {
            return;
        }
        self.killed = true;
        kill_tree(Some(self.child.id()));
        let _ = self.child.wait();
    }
}

impl Transport for ChildTransport {
    fn send(&mut self, frame: Value) -> Result<()> {
        if let Some(error) = self.take_writer_error() {
            return Err(deaf_child(&self.backend, &error));
        }

        let line = frame_line(&frame);
        let Some(frames) = &self.frames else {
            return Err(PieError::protocol(
                &self.backend,
                "the child's stdin is already closed, so a tool result cannot be delivered",
            ));
        };

        // Handing the bytes to the writer thread is what keeps this call off the pipe:
        // a synchronous write of a frame larger than the pipe buffer would block here,
        // with the child blocked on the stdout nobody is draining any more.
        frames.send(line).map_err(|_| {
            PieError::protocol(
                &self.backend,
                "the child's stdin writer stopped before this tool result was queued",
            )
        })
    }

    fn recv(&mut self) -> Result<Option<StreamEvent>> {
        loop {
            if let Some(event) = self.pending.pop_front() {
                return Ok(Some(event));
            }

            let Some(line) = self.next_line()? else {
                // End of stream. A frame that was queued but never landed is reported
                // here rather than swallowed: the loop would otherwise return a
                // successful outcome for a conversation the model abandoned.
                if let Some(error) = self.take_writer_error() {
                    return Err(deaf_child(&self.backend, &error));
                }
                return Ok(None);
            };

            self.pending.extend(parse_sdk_line(&self.backend, &line)?);
        }
    }
}

impl Drop for ChildTransport {
    fn drop(&mut self) {
        // Closing the frame channel ends the writer thread, which drops the child's
        // stdin. Then the tree is killed: a child that outlives its parent keeps a
        // subscription turn running with nobody reading it (`RISKS.md` §6).
        self.frames = None;
        self.kill();
    }
}

/// Report a child that is not reading what we write.
///
/// A `tool_result` the child never read is worse than a failed turn: the conversation
/// stalls with the model waiting, so the loop must end in an error that names the cause
/// rather than in a clean end-of-stream.
fn deaf_child(backend: &str, error: &std::io::Error) -> PieError {
    PieError::protocol(
        backend,
        format!(
            "writing a `tool_result` to the child's stdin failed ({error}); the child is no \
             longer reading its input, so the answer it was waiting for cannot be delivered"
        ),
    )
}

/// Turn a neutral frame into the line the child reads, terminator included.
///
/// The one place claude's wire shape appears: its stream-json **input** is a user message
/// whose content holds the `tool_result` block, while the loop's frame *is* the block. A
/// frame that is already a message (the first prompt, a control frame) goes through
/// unchanged.
///
/// The trailing newline is not cosmetic. stream-json input is NDJSON, so a frame without
/// its newline is a *partial* line the CLI never processes, and the conversation stalls
/// with the model waiting for the very result we thought we had sent — a silent version
/// of the failure [`deaf_child`] exists to make loud.
fn frame_line(frame: &Value) -> String {
    let mut line = if frame.get("type").and_then(Value::as_str) == Some("tool_result") {
        json!({
            "type": "user",
            "message": { "role": "user", "content": [frame] },
        })
        .to_string()
    } else {
        frame.to_string()
    };
    line.push('\n');
    line
}

/// Read one line, retaining at most [`process::MAX_LINE_BYTES`] bytes, lossily.
///
/// The synchronous twin of `process::read_line_capped`, for the same reasons: a line with
/// no newline must not grow the buffer without bound, and one byte that is not valid
/// UTF-8 must not abort a healthy turn (I5). Written here rather than in `process.rs`
/// because that reader is async and a [`Transport`] is not — see the note in the M9
/// report about folding the two together.
fn read_capped_line<R: BufRead>(reader: &mut R) -> std::io::Result<Option<String>> {
    let cap = process::MAX_LINE_BYTES;
    let mut out: Vec<u8> = Vec::new();
    let mut saw_any = false;
    let mut truncated = false;

    loop {
        let (mut chunk, consume, newline) = {
            let available = reader.fill_buf()?;
            if available.is_empty() {
                if !saw_any {
                    return Ok(None);
                }
                break; // end of stream: what we have is the last line
            }
            match available.iter().position(|&byte| byte == b'\n') {
                Some(at) => (available[..at].to_vec(), at + 1, true),
                None => (available.to_vec(), available.len(), false),
            }
        };

        saw_any = true;
        let room = cap.saturating_sub(out.len());
        if chunk.len() <= room {
            out.append(&mut chunk);
        } else {
            out.extend_from_slice(&chunk[..room]);
            truncated = true;
        }
        reader.consume(consume);

        if newline {
            break;
        }
    }

    if truncated {
        // A line this long signals a broken or hostile stream rather than a big answer, and
        // staying quiet would make the truncation look like a short frame (`process.rs`
        // warns for the same reason).
        tracing::warn!(
            cap_bytes = cap,
            "truncated an oversized stdout line from the tool-loop child; a vendor line longer \
             than the cap cannot be buffered without bound"
        );
    }

    Ok(Some(finish_line(out)))
}

/// Trim a trailing CR and interpret the bytes lossily.
fn finish_line(mut bytes: Vec<u8>) -> String {
    if bytes.last() == Some(&b'\r') {
        bytes.pop();
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Keep the last [`process::STDERR_TAIL_BYTES`] of the child's stderr.
fn drain_tail<R: std::io::Read>(reader: R, tail: Arc<Mutex<String>>) {
    let mut reader = BufReader::new(reader);
    let mut kept: Vec<u8> = Vec::new();

    while let Ok(Some(line)) = read_capped_line(&mut reader) {
        kept.extend_from_slice(line.as_bytes());
        kept.push(b'\n');
        if kept.len() > process::STDERR_TAIL_BYTES {
            let excess = kept.len() - process::STDERR_TAIL_BYTES;
            kept.drain(..excess);
        }
    }

    *lock(&tail) = String::from_utf8_lossy(&kept).into_owned();
}

/// Kill a process and everything it spawned, without a runtime.
///
/// The synchronous twin of `process::kill_tree`, and deliberately the same policy:
/// `taskkill /T /F` on Windows, a negative-pid signal to the child's own process group
/// elsewhere, with the env scrub applied to the helper too. The other twin lives in
/// `process.rs` because that one is async and a [`Transport`] is not.
fn kill_tree(pid: Option<u32>) {
    let Some(pid) = pid else { return };

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let mut cmd = Command::new("taskkill");
        cmd.args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(process::CREATE_NO_WINDOW);
        // Even a trusted OS helper gets the scrub: the invariant is about what any
        // child can observe, not about which children we happen to trust.
        for key in crate::config::BUILTIN_SCRUB {
            cmd.env_remove(key);
        }
        let _ = cmd.status();
    }

    #[cfg(not(windows))]
    {
        let group = format!("-{pid}");
        for signal in ["-TERM", "-KILL"] {
            let mut cmd = Command::new("kill");
            cmd.args([signal, &group])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            for key in crate::config::BUILTIN_SCRUB {
                cmd.env_remove(key);
            }
            let _ = cmd.status();
            if signal == "-TERM" {
                // Give the tree a moment to exit cleanly before escalating.
                std::thread::sleep(Duration::from_millis(250));
            }
        }
    }
}

/// Lock without `unwrap`: a poisoned mutex means another thread panicked, and the data
/// on the other side of these locks is a diagnostics tail, not a broken invariant.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn a_tool_result_frame_is_neutral_and_carries_the_id_it_answers() {
        let frame = tool_result_frame(&ToolResult {
            id: "toolu_1".to_string(),
            output: "ok".to_string(),
            is_error: true,
        });
        assert_eq!(frame["type"], "tool_result");
        assert_eq!(frame["tool_use_id"], "toolu_1");
        assert_eq!(frame["content"], "ok");
        assert_eq!(frame["is_error"], true);
    }

    /// The claude envelope is the transport's business, so the assertion is on the line
    /// the child would actually read — including its newline, because stream-json input is
    /// NDJSON and an unterminated line is a frame the CLI never sees.
    #[test]
    fn claude_reads_a_result_as_a_user_message_wrapping_the_block() {
        let frame = tool_result_frame(&ToolResult {
            id: "toolu_1".to_string(),
            output: "ok".to_string(),
            is_error: false,
        });
        let line = frame_line(&frame);
        assert!(
            line.ends_with('\n'),
            "a frame without its newline stalls the CLI"
        );
        let parsed: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed["type"], "user");
        assert_eq!(parsed["message"]["role"], "user");
        assert_eq!(parsed["message"]["content"][0]["tool_use_id"], "toolu_1");
    }

    #[test]
    fn a_banner_line_is_tolerated_rather_than_fatal() {
        assert_eq!(
            events_from_sdk_line("Welcome to the CLI").unwrap(),
            vec![StreamEvent::Unparsed("Welcome to the CLI".to_string())]
        );
        assert!(events_from_sdk_line("   ").unwrap().is_empty());
        assert_eq!(
            events_from_sdk_line("[1,2,3]").unwrap(),
            vec![StreamEvent::Unparsed("[1,2,3]".to_string())]
        );
    }

    #[test]
    fn one_assistant_message_can_carry_text_and_a_tool_call() {
        let line = r#"{"type":"assistant","message":{"content":[
            {"type":"text","text":"let me look"},
            {"type":"tool_use","id":"toolu_9","name":"Read","input":{"path":"a.txt"}}]}}"#;
        let events = events_from_sdk_line(line).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0], StreamEvent::TextDelta("let me look".to_string()));
        match &events[1] {
            StreamEvent::ToolUse { id, name, input } => {
                assert_eq!(id, "toolu_9");
                assert_eq!(name, "Read");
                assert_eq!(input["path"], "a.txt");
            }
            other => panic!("expected a tool call, got {other:?}"),
        }
    }

    #[test]
    fn a_result_frame_becomes_done_and_an_unknown_kind_is_preserved() {
        let done = events_from_sdk_line(
            r#"{"type":"result","subtype":"success","result":"final answer"}"#,
        )
        .unwrap();
        assert_eq!(
            done,
            vec![StreamEvent::Done {
                text: "final answer".to_string(),
                status: "success".to_string(),
            }]
        );

        let init = events_from_sdk_line(r#"{"type":"system","subtype":"init"}"#).unwrap();
        assert!(matches!(init.as_slice(), [StreamEvent::Other { kind, .. }] if kind == "system"));
    }

    /// A frame that claims a shape it does not have is an error, not a skip: skipping it
    /// would drop whatever tool call it held and leave the model waiting.
    #[test]
    fn an_assistant_frame_without_a_content_array_is_a_stated_error() {
        let error =
            events_from_sdk_line(r#"{"type":"assistant","message":{"content":"hi"}}"#).unwrap_err();
        assert_eq!(error.exit_code(), 4);
        assert!(error.to_string().contains("content array"), "{error}");

        let no_id = events_from_sdk_line(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Read"}]}}"#,
        )
        .unwrap_err();
        assert!(no_id.to_string().contains("no id"), "{no_id}");
    }
}
