//! The SSE wire the API-key backends speak (`planning/DEFERRED-BACKENDS.md`,
//! `planning/ACCEPTANCE.md` §18 stage A).
//!
//! OpenRouter and DeepSeek are OpenAI-compatible: `text/event-stream`, one `data:` frame
//! per JSON chunk, `[DONE]` as the terminator, `tool_calls` fragments to be stitched back
//! together. This module is the **wire and nothing else** — no socket, no credential, no
//! vendor constant — so the whole of it is testable from bytes, which is the only way a
//! test may touch it (stage A's rule: no test reaches the network, ever).
//!
//! ## Incremental by construction, and why that is not optional
//!
//! SSE arrives in arbitrary byte chunks and a frame boundary lands wherever the network
//! put it: a `data:` payload routinely straddles two reads, and a chunk can end
//! mid-UTF-8-character. [`SseDecoder`] therefore holds the buffer between calls and only
//! ever dispatches a frame whose terminating blank line it has actually seen. Framing is
//! done on **bytes** and decoding on completed lines, so a multi-byte character split
//! across two chunks cannot panic the way a `from_utf8` on a partial chunk would.
//!
//! ## What is tolerated, and what is a stated error
//!
//! Invariant I5 ("a bad stdout line never aborts a run") is the reason a garbage frame is
//! survivable, and the project's other habit — *a silently truncated answer reads as a
//! complete one* — is the reason some other cases are not. The line between them is
//! **whether the bytes prove the stream is intact**:
//!
//! | Case | Decision | Why |
//! |---|---|---|
//! | a frame that is not `data:` (`event:`, `id:`, `retry:`, a `:` comment) | ignored | SSE allows them; a keepalive comment is not an error |
//! | a `data:` payload that is not JSON | tolerated, forwarded as [`StreamEvent::Unparsed`] | I5: one frame we cannot read must not discard the frames we can |
//! | a body that closes without the final blank line | the last frame is still dispatched | the separator is what *normally* ends a frame, and a server that closes the body after the last frame sent a complete one; refusing it would fail a healthy turn over a formatting detail |
//! | a body that closes **mid-frame** with a payload that is not JSON | error | the JSON is provably incomplete — that is the signature of a cut connection |
//! | a body with no `[DONE]` but with a `finish_reason` | tolerated, `Done` emitted | the provider signalled completion; `[DONE]` is a convention, not a protocol requirement |
//! | a body with neither `[DONE]` nor a `finish_reason` | error | nothing in the stream says the answer ended, and a half answer that reads as whole is the failure this refuses to make |
//! | a frame carrying an `error` object mid-stream | error | the request was accepted with a 200 and then failed; tolerating it would report the truncated prefix as the answer |
//! | a frame larger than [`MAX_FRAME_BYTES`] | error | an unbounded buffer is a memory hazard a peer chooses |

use std::collections::BTreeMap;

use serde_json::{json, Value};

use crate::error::{truncate, PieError, Result};
use crate::jsonl;
use crate::process;
use crate::provider::{StreamEvent, Usage};

/// The terminator OpenAI-compatible streams use.
pub const DONE_MARKER: &str = "[DONE]";

/// Kind of the [`StreamEvent::Other`] that carries a provider-reported cost.
///
/// The cost needs a carrier and the frozen enum has no field for it: `Usage` is the token
/// accounting (`input_tokens` … `total_tokens`) and `TurnOutcome` carries a `Usage`, so a
/// real cost has nowhere to live in the M9 shapes. An additive variant would mean editing
/// `src/provider.rs`, which this stage does not own, and inventing a `cost_usd_micros` of
/// `0` — which is what the field defaults to — would be a **claim that the turn was free**
/// (`TOKEN-ECONOMICS.md` §10c). So the cost rides in the event kind that exists for exactly
/// this: a shape pie does not model yet, forwarded so `--json` carries it and nothing is
/// lost. [`cost_usd_micros`] is the typed reader; a zero is never invented.
pub const COST_EVENT_KIND: &str = "usage_cost";

/// Largest frame the decoder will buffer before giving up.
///
/// A legitimate frame is a few kilobytes; this bound exists so a peer cannot make pie
/// allocate without limit by never sending a newline. 16 MiB leaves room for a large
/// tool argument while still bounding the damage.
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

/// How much of the answer the terminal event carries back.
///
/// The deltas have already carried every byte to the caller, so this copy exists only for
/// a consumer that reads `Done` alone (as `agy`/`codex` routers allow). Capped because it
/// is a second copy of the whole answer, and an unbounded copy of a stream whose length the
/// provider chooses is a memory hazard with no reader.
const MAX_ANSWER_CHARS: usize = 200_000;

/// One `tool_calls[]` entry, still being stitched together.
#[derive(Debug, Default)]
struct PartialCall {
    id: String,
    name: String,
    /// `function.arguments`, which arrives as a **string** built up one fragment at a time.
    arguments: String,
    /// A provider that sent whole arguments as real JSON instead of a string. Kept
    /// verbatim: re-serialising it would be a faithful copy of a value we already have.
    provided: Option<Value>,
}

/// The incremental SSE parser.
///
/// Feed it bytes, get events. The buffer, the open frame and the half-assembled tool calls
/// all live here between calls, because a frame boundary is decided by the network.
#[derive(Debug)]
pub struct SseDecoder {
    /// Backend id, for errors — this module is shared, so it cannot name itself.
    backend: String,
    /// Bytes received but not yet a complete line.
    buffer: Vec<u8>,
    /// `data:` payloads of the frame being read, joined with `\n` at its end (the SSE spec
    /// says concatenate with a newline, and a payload split across lines is only whole once
    /// the blank line arrives).
    frame: Vec<String>,
    /// Tool calls by their `index`, which is why this is a map: the fragments of one call
    /// interleave with nothing else, but a map keyed on the vendor's own index cannot
    /// merge two parallel calls no matter what order the frames arrive in.
    calls: BTreeMap<u64, PartialCall>,
    /// The vendor's stop reason, once a choice reports one.
    finish_reason: Option<String>,
    /// `[DONE]` seen: the stream is over, and later bytes must not resurrect it.
    terminated: bool,
    /// The assembled calls have been emitted, so a `finish_reason` and a later `[DONE]`
    /// cannot emit them twice.
    calls_flushed: bool,
    /// Answer text so far, for the terminal event.
    answer: String,
}

impl SseDecoder {
    /// A decoder for one backend, named in every error it raises.
    pub fn new(backend: impl Into<String>) -> Self {
        Self {
            backend: backend.into(),
            buffer: Vec::new(),
            frame: Vec::new(),
            calls: BTreeMap::new(),
            finish_reason: None,
            terminated: false,
            calls_flushed: false,
            answer: String::new(),
        }
    }

    /// Whether the terminator has arrived.
    pub fn terminated(&self) -> bool {
        self.terminated
    }

    /// Feed one arbitrary chunk of the body and collect whatever it completed.
    ///
    /// Returns a `Result` rather than a plain `Vec` because three of the cases in the table
    /// above are failures the caller must see: a mid-stream `error` frame, an oversized
    /// frame, and (from [`SseDecoder::finish`]) a body that was cut. Nothing that a frame
    /// could carry is discarded silently — an unreadable payload comes back as
    /// [`StreamEvent::Unparsed`].
    pub fn feed(&mut self, chunk: &[u8]) -> Result<Vec<StreamEvent>> {
        if self.terminated {
            // `[DONE]` is the terminator, so anything after it is not part of this turn.
            // Ignoring it is the only safe reading: dispatching a post-terminator frame
            // would append to an answer the caller has already been told is final.
            return Ok(Vec::new());
        }

        self.buffer.extend_from_slice(chunk);
        if self.buffer.len() > MAX_FRAME_BYTES {
            return Err(PieError::protocol(
                self.backend.as_str(),
                format!(
                    "an SSE frame grew past {MAX_FRAME_BYTES} bytes without the newline that \
                     would end it; refusing to buffer further rather than let the peer choose \
                     how much memory pie allocates"
                ),
            ));
        }

        // Every complete line is taken in one pass, then processed: re-scanning the whole
        // buffer per line would be quadratic in the number of frames in a chunk, and a
        // chunk is whatever size the network handed us.
        let Some(last) = self.buffer.iter().rposition(|byte| *byte == b'\n') else {
            return Ok(Vec::new());
        };
        let head: Vec<u8> = self.buffer.drain(..=last).collect();

        let mut events = Vec::new();
        // `split_inclusive`, so every piece is one line *including* its newline. A blank
        // line — the frame separator — is then a piece that is nothing but that newline.
        // Splitting on the newline instead leaves an empty piece after the final one, and
        // treating that as a separator dispatches a frame whose second `data:` line has not
        // arrived yet: the end of a line is not the end of a frame.
        for piece in head.split_inclusive(|byte| *byte == b'\n') {
            let line = String::from_utf8_lossy(&piece[..piece.len() - 1]);
            self.absorb_line(&line, &mut events)?;
        }
        Ok(events)
    }

    /// The body has ended: dispatch whatever it still held, or say why it cannot.
    ///
    /// Call this once, when the response stream reports end-of-body. It is idempotent, and
    /// it is the only place that can tell a complete turn from a cut connection — which is
    /// why the transport must call it rather than treating end-of-body as end-of-stream.
    pub fn finish(&mut self) -> Result<Vec<StreamEvent>> {
        let mut events = Vec::new();
        if self.terminated {
            return Ok(events);
        }

        // A body may close without the blank line that would normally end its last frame.
        // The buffer holds no newline at all here — `feed` drains through the last one — so
        // this is a single partial-or-complete line.
        if !self.buffer.is_empty() {
            let head = std::mem::take(&mut self.buffer);
            for raw in head.split(|byte| *byte == b'\n') {
                let line = String::from_utf8_lossy(raw);
                self.absorb_line(&line, &mut events)?;
            }
        }

        // An unterminated frame is dispatched only when its payload is provably whole: a
        // payload that parses as JSON (or is the terminator) cannot be a fragment of
        // anything, so the missing blank line cost us nothing. Anything else is a frame cut
        // by the network, and buffering it as if it were complete would hide exactly the
        // truncation this module exists to refuse.
        if !self.frame.is_empty() {
            let payload = std::mem::take(&mut self.frame).join("\n");
            let whole = payload.trim() == DONE_MARKER
                || serde_json::from_str::<Value>(payload.trim()).is_ok();
            if !whole {
                return Err(self.cut(
                    "closed mid-frame, so its last frame was never delivered whole",
                    &payload,
                ));
            }
            self.dispatch(&payload, &mut events)?;
        }

        if self.terminated {
            return Ok(events);
        }

        // No `[DONE]`, but the provider said the choice finished. `[DONE]` is an OpenAI
        // convention rather than a requirement of SSE, and a turn that stopped after a
        // `finish_reason` is complete: failing it would throw away a whole answer over a
        // terminator nobody needed.
        if let Some(status) = self.finish_reason.clone() {
            self.flush_calls(&mut events)?;
            self.terminated = true;
            events.push(StreamEvent::Done {
                text: self.take_answer(),
                status,
            });
            return Ok(events);
        }

        Err(self.cut("ended with no `[DONE]` and no `finish_reason`", ""))
    }

    /// One line of the body, with the frame it belongs to.
    fn absorb_line(&mut self, line: &str, events: &mut Vec<StreamEvent>) -> Result<()> {
        // A CRLF connection is not a different protocol, so the carriage return goes before
        // anything looks at the line.
        let line = line.strip_suffix('\r').unwrap_or(line);

        if line.is_empty() {
            // A blank line ends the frame. Nothing buffered means the blank line stood
            // alone, which is a separator rather than an empty frame.
            if self.frame.is_empty() {
                return Ok(());
            }
            let payload = std::mem::take(&mut self.frame).join("\n");
            return self.dispatch(&payload, events);
        }

        if let Some(rest) = line.strip_prefix("data:") {
            // The space after the colon is optional in SSE, and providers differ.
            self.frame
                .push(rest.strip_prefix(' ').unwrap_or(rest).to_string());
            return Ok(());
        }

        // `event:`, `id:`, `retry:`, a `:` keepalive comment and anything else the spec
        // grows: ignored, not fatal. An unknown field is not a broken stream (I5).
        Ok(())
    }

    /// One complete `data:` payload.
    fn dispatch(&mut self, payload: &str, events: &mut Vec<StreamEvent>) -> Result<()> {
        let trimmed = payload.trim();

        if trimmed == DONE_MARKER {
            self.flush_calls(events)?;
            self.terminated = true;
            events.push(StreamEvent::Done {
                text: self.take_answer(),
                // The vendor's own word for why it stopped (`stop`, `tool_calls`,
                // `length`). `unknown` when it never said, so the status is never a value
                // pie invented.
                status: self
                    .finish_reason
                    .clone()
                    .unwrap_or_else(|| "unknown".into()),
            });
            return Ok(());
        }

        let value: Value = match serde_json::from_str(trimmed) {
            Ok(value) => value,
            // I5. Forwarded rather than dropped so a consumer can see what arrived — and
            // so a provider that starts sending a wrapper frame does not silently lose its
            // content.
            Err(_) => {
                events.push(StreamEvent::Unparsed(payload.to_string()));
                return Ok(());
            }
        };

        // A 200 that fails mid-stream. This is not the "bad line" I5 tolerates: the
        // provider is *telling* us the turn failed, and the frames already delivered are a
        // prefix of an answer that will never be finished. Reporting that prefix as the
        // answer is the one outcome worse than an error.
        if let Some(error) = value.get("error") {
            if !error.is_null() {
                let message = jsonl::s_nonempty(error, "message")
                    .or_else(|| jsonl::s_nonempty(&value, "message"))
                    .unwrap_or_else(|| jsonl::brief(&value, 300));
                let status = jsonl::u(error, "code")
                    .map(|code| format!("HTTP {code}"))
                    .unwrap_or_else(|| "stream error".to_string());
                return Err(PieError::Backend {
                    backend: self.backend.clone(),
                    status,
                    detail: truncate(&process::redact(&message), 600),
                });
            }
        }

        // The accounting object. It arrives on the last content frame or on a frame of its
        // own with an empty `choices`, depending on the provider, so it is read wherever it
        // appears rather than from one expected position.
        if let Some(usage) = value.get("usage").filter(|usage| usage.is_object()) {
            self.emit_usage(usage, events);
        }

        for choice in value
            .get("choices")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let delta = choice.get("delta");

            if let Some(content) = delta.and_then(|delta| delta.get("content")) {
                self.emit_content(content, events);
            }

            if let Some(calls) = delta
                .and_then(|delta| delta.get("tool_calls"))
                .and_then(Value::as_array)
            {
                for call in calls {
                    self.absorb_call(call);
                }
            }

            if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                self.finish_reason = Some(reason.to_string());
                // The call is answered only now, and that is the point: a tool call's
                // arguments are a string built up across frames, so a runner handed the
                // call before this point would run it on a fragment of its arguments —
                // which for a `bash` tool is a *different command*, not a smaller one.
                self.flush_calls(events)?;
            }
        }

        Ok(())
    }

    /// `delta.content`, which is text, or (in newer OpenAI shapes) a list of text parts.
    fn emit_content(&mut self, content: &Value, events: &mut Vec<StreamEvent>) {
        if let Some(text) = content.as_str() {
            if !text.is_empty() {
                self.push_answer(text);
                events.push(StreamEvent::TextDelta(text.to_string()));
            }
            return;
        }

        // A part list carries the same text one piece per element. Ignoring the shape
        // would drop the whole answer on a provider that adopted it, and the `type` is
        // checked so a future part kind is skipped rather than concatenated blind.
        for part in content.as_array().into_iter().flatten() {
            if part.get("type").and_then(Value::as_str) == Some("text") {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    if !text.is_empty() {
                        self.push_answer(text);
                        events.push(StreamEvent::TextDelta(text.to_string()));
                    }
                }
            }
        }
    }

    /// One `tool_calls[]` fragment.
    fn absorb_call(&mut self, call: &Value) {
        // `index` is required by the OpenAI schema and is what ties the fragments of one
        // call together. A provider that omits it is serialising one call at a time, so 0
        // is the right default rather than a guess.
        let index = call.get("index").and_then(Value::as_u64).unwrap_or(0);
        let entry = self.calls.entry(index).or_default();

        // The id and the name arrive on the first fragment and are absent afterwards, so
        // each is taken when it is present and never overwritten.
        if let Some(id) = call.get("id").and_then(Value::as_str) {
            if !id.is_empty() {
                entry.id = id.to_string();
            }
        }
        if let Some(name) = call
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(Value::as_str)
        {
            if !name.is_empty() {
                entry.name = name.to_string();
            }
        }
        if let Some(arguments) = call.get("function").and_then(|f| f.get("arguments")) {
            match arguments {
                Value::Null => {}
                Value::String(fragment) => entry.arguments.push_str(fragment),
                // Whole arguments as real JSON. Kept as they arrived.
                other => entry.provided = Some(other.clone()),
            }
        }
    }

    /// Emit the assembled calls, in the order the provider indexed them.
    fn flush_calls(&mut self, events: &mut Vec<StreamEvent>) -> Result<()> {
        if self.calls_flushed {
            return Ok(());
        }
        self.calls_flushed = true;

        for (index, call) in std::mem::take(&mut self.calls) {
            let input = match call.provided {
                Some(value) => value,
                // The empty string is what a no-argument call sends, and it is not a
                // malformed call: `{}` and `""` both mean "no arguments".
                None if call.arguments.trim().is_empty() => Value::Null,
                None => serde_json::from_str(&call.arguments).map_err(|error| {
                    PieError::protocol(
                        self.backend.as_str(),
                        format!(
                            "tool call {index} (`{}`) carried arguments that are not JSON: \
                             {error} — its raw text was {}",
                            truncate(&process::redact(&call.name), 120),
                            truncate(&process::redact(&call.arguments), 200)
                        ),
                    )
                })?,
            };

            // An id is the only handle a `tool_result` has on a `tool_use`. Sending one
            // with no id would produce a result the model cannot match, and the loop
            // refuses such a frame anyway — so the failure is stated here, where the wire
            // is still responsible for it.
            if call.id.trim().is_empty() {
                return Err(PieError::protocol(
                    self.backend.as_str(),
                    format!(
                        "a tool call for `{}` arrived with no id; a `tool_result` cannot \
                         answer a call it cannot name",
                        truncate(&process::redact(&call.name), 120)
                    ),
                ));
            }

            events.push(StreamEvent::ToolUse {
                id: call.id,
                name: call.name,
                input,
            });
        }
        Ok(())
    }

    /// The accounting object, however much of it the provider filled in.
    fn emit_usage(&self, usage: &Value, events: &mut Vec<StreamEvent>) {
        let prompt = jsonl::u(usage, "prompt_tokens").unwrap_or(0);
        let completion = jsonl::u(usage, "completion_tokens").unwrap_or(0);
        // `completion_tokens` *includes* reasoning tokens in the OpenAI-compatible shape,
        // while pie's `Usage` counts them in their own field and `computed_total` sums the
        // parts. Reporting both whole would count reasoning twice and inflate every total
        // by the thinking budget, so the reasoning share is moved rather than duplicated.
        let reasoning = jsonl::u(usage, "completion_tokens_details.reasoning_tokens")
            .unwrap_or(0)
            .min(completion);

        let mapped = Usage {
            input_tokens: prompt,
            output_tokens: completion - reasoning,
            thinking_tokens: reasoning,
            // Two spellings of the same number: OpenAI-compatible
            // (`prompt_tokens_details.cached_tokens`) and DeepSeek's own
            // (`prompt_cache_hit_tokens`).
            cache_read_tokens: jsonl::u(usage, "prompt_tokens_details.cached_tokens")
                .or_else(|| jsonl::u(usage, "prompt_cache_hit_tokens"))
                .unwrap_or(0),
            // No OpenAI-compatible provider reports cache *writes*; Anthropic does, and
            // this wire is not Anthropic's. Zero here means "not reported", which is
            // `Usage::merge`'s documented reading of a zero.
            cache_write_tokens: 0,
            total_tokens: jsonl::u(usage, "total_tokens").unwrap_or(0),
            // The field exists since M10 (`src/provider.rs`), so the figure travels with the
            // usage rather than only in a side-channel event. Both carry it on purpose: this
            // one is what `session_log` writes, and the `Other` event below keeps the raw
            // provider JSON available to `--json` without pie inventing a shape for it.
            cost_usd_micros: reported_cost_micros(usage).unwrap_or(0),
        };

        if mapped != Usage::default() {
            events.push(StreamEvent::Usage(mapped));
        }

        // A real cost, or no cost at all. `DEFERRED-BACKENDS.md` is explicit that
        // `cost_usd_micros` is real for this class of backend, and M4 removed the cost
        // column precisely *because* a zero implies cost is measured when it is not — so
        // the one thing this must never do is invent one.
        if let Some(micros) = reported_cost_micros(usage) {
            events.push(StreamEvent::Other {
                kind: COST_EVENT_KIND.to_string(),
                raw: json!({
                    "cost_usd_micros": micros,
                    "unit": "USD",
                    "source": "usage.cost",
                }),
            });
        }
    }

    /// Keep the answer copy bounded.
    fn push_answer(&mut self, text: &str) {
        if self.answer.chars().count() >= MAX_ANSWER_CHARS {
            return;
        }
        self.answer.push_str(text);
    }

    /// Hand the answer out once.
    fn take_answer(&mut self) -> String {
        std::mem::take(&mut self.answer)
    }

    /// The error for a body that cannot be read to the end, naming what arrived.
    fn cut(&self, why: &str, tail: &str) -> PieError {
        let evidence = if tail.trim().is_empty() {
            String::new()
        } else {
            format!(
                " Its last frame was {}",
                truncate(&process::redact(tail), 200)
            )
        };
        PieError::protocol(
            self.backend.as_str(),
            format!(
                "the SSE body {why}. A stream that stops early is not an empty answer: reporting \
                 one would hand the caller a prefix of an answer that reads as the whole \
                 thing.{evidence}"
            ),
        )
    }
}

/// The cost a provider reported in `usage`, in micro-USD.
///
/// OpenRouter's usage accounting reports money as a **fractional dollar** value in
/// `usage.cost`, which is the only cost field an OpenAI-compatible response carries; a
/// provider that reports nothing yields `None` rather than `Some(0)`, because zero is a
/// claim about the invoice (`TOKEN-ECONOMICS.md` §10c) and this is the field the whole
/// M10 cost decision rests on. A negative or non-finite value is not money, so it is
/// treated as not reported rather than rendered as a nonsensical figure.
fn reported_cost_micros(usage: &Value) -> Option<u64> {
    let cost = usage.get("cost").and_then(Value::as_f64)?;
    if !cost.is_finite() || cost < 0.0 {
        return None;
    }
    let micros = (cost * 1_000_000.0).round();
    if micros > u64::MAX as f64 {
        return None;
    }
    Some(micros as u64)
}

/// The provider-reported cost carried by an event, in micro-USD.
///
/// The typed reader for [`COST_EVENT_KIND`], so a caller never has to know the JSON shape
/// it is stored in. `None` means the provider did not report a cost — never that it
/// reported zero, because those are different statements about the invoice.
///
/// It reads the **typed** field since M10 as well as the side-channel event: the figure now
/// lives on `Usage`, where `session_log` can find it, and the `Other` event still carries
/// the raw JSON for `--json`. Checking both means a caller cannot get `None` from a turn
/// that plainly reported a cost because it happened to look at the other carrier.
pub fn cost_usd_micros(event: &StreamEvent) -> Option<u64> {
    match event {
        StreamEvent::Other { kind, raw } if kind == COST_EVENT_KIND => {
            raw.get("cost_usd_micros").and_then(Value::as_u64)
        }
        StreamEvent::Usage(usage) if usage.cost_usd_micros > 0 => Some(usage.cost_usd_micros),
        _ => None,
    }
}
