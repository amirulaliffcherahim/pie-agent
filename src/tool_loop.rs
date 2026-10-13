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
//! no vendor knowledge, and [`SdkTransport`] does the one translation claude needs —
//! which is also how M10 reuses this loop instead of writing a second one.

use std::collections::VecDeque;

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
    /// Token accounting merged across the turns.
    ///
    /// Accumulated here rather than by the caller because the loop is where the `Usage`
    /// events pass through, and M10 needs the figure to reach the session log with its
    /// `cost_usd_micros` intact — a caller that had to re-derive it would be a second place
    /// for the merge rule to live.
    pub usage: crate::provider::Usage,
    /// Wall clock for the whole loop.
    ///
    /// Measured here rather than passed in, unlike `driver::drive`'s `started`: that one
    /// receives an instant because its caller already had one, while this is the only place
    /// that knows when the loop began. It is a measurement, not a decision — the
    /// clock-as-a-parameter rule is about decisions (`due`, decay, the report's month),
    /// which must be testable at an arbitrary instant.
    pub duration: std::time::Duration,
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
    let started = std::time::Instant::now();
    let mut outcome = LoopOutcome {
        iterations: 0,
        capped: false,
        text: String::new(),
        usage: crate::provider::Usage::default(),
        duration: std::time::Duration::ZERO,
    };

    while let Some(event) = transport.recv()? {
        let call = match &event {
            StreamEvent::TextDelta(chunk) => {
                outcome.text.push_str(chunk);
                None
            }
            // Merged with `Usage::merge`'s rule (a zero field means "not reported"), so a
            // later partial report cannot blank a figure an earlier one already gave — and
            // so `cost_usd_micros` survives a frame that omits it.
            StreamEvent::Usage(usage) => {
                outcome.usage.merge(usage.clone());
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

    outcome.duration = started.elapsed();
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

/// A [`Transport`] over a [`process::ChildSession`], speaking claude's `stream-json`.
///
/// **It carries no spawn policy of its own, and that is the point.** M9 wrote it as a
/// second copy of `process.rs`'s policy — hidden window, process group, env scrub, a
/// whole-turn deadline, a kill tree — because `run_streaming` hands the child
/// `stdin(null)` and the loop needs a pipe. `DESIGN.md` §20 ("One spawn policy, not
/// three") removed the copy: all of that is now
/// [`ChildSession`](process::ChildSession)'s, and what is left here is the two pieces that
/// are genuinely the loop's — claude's wire shape ([`frame_line`]) and the mapping from a
/// line to [`StreamEvent`]s ([`parse_sdk_line`]).
///
/// The one consequence worth knowing when reading a failure: `send` returns as soon as the
/// frame is handed to the session's writer thread, so a child that has stopped reading its
/// stdin is reported on the *next* `send` or at end of stream ([`deaf_child`]), never as a
/// blocked call here.
#[derive(Debug)]
pub struct SdkTransport {
    backend: String,
    session: process::ChildSession,
    /// Events parsed from a line, waiting to be returned one at a time.
    pending: VecDeque<StreamEvent>,
}

impl SdkTransport {
    /// Spawn `spec` and wire its stdin/stdout into a transport.
    pub fn spawn(spec: &RunSpec) -> Result<Self> {
        // All of it is `process.rs`'s: the hidden window (I1), the process group, the env
        // scrub (I2), the `Instant + Duration` overflow rule, the stdin writer thread, the
        // stdout reader thread, the detached stderr drain, the deadline and the kill tree.
        Ok(Self {
            backend: spec.backend.clone(),
            session: process::ChildSession::spawn(spec)?,
            pending: VecDeque::new(),
        })
    }

    /// The last bytes of the child's stderr, for a diagnostic.
    pub fn stderr_tail(&self) -> String {
        self.session.stderr_tail()
    }
}

impl Transport for SdkTransport {
    fn send(&mut self, frame: Value) -> Result<()> {
        if let Some(error) = self.session.take_write_error() {
            return Err(deaf_child(&self.backend, &error));
        }

        // The frame's *shape* is claude's and is decided here; getting the bytes onto the
        // pipe — and keeping this call off it, so the read path never stops draining — is
        // the session's.
        self.session.send_bytes(frame_line(&frame))
    }

    fn recv(&mut self) -> Result<Option<StreamEvent>> {
        loop {
            if let Some(event) = self.pending.pop_front() {
                return Ok(Some(event));
            }

            let Some(line) = self.session.next_line()? else {
                // End of stream. A frame that was queued but never landed is reported
                // here rather than swallowed: the loop would otherwise return a
                // successful outcome for a conversation the model abandoned.
                if let Some(error) = self.session.take_write_error() {
                    return Err(deaf_child(&self.backend, &error));
                }
                return Ok(None);
            };

            self.pending
                .extend(parse_sdk_line(&self.backend, &line.text)?);
        }
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
