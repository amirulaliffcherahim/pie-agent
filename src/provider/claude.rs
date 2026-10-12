//! Anthropic Claude Code (`claude`) backend — future-proofing only.
//!
//! There is **no Claude subscription on this machine**, so this adapter is
//! written to spec and left disabled. It must never block the critical path
//! (`PLAN.md` §1): the live v0 path is agy + codex.
//!
//! Spec source: `knowledge/claude.md`.
//!
//! `tool_loop` is reported as `false` deliberately. Claude Code's SDK mode
//! (`--input-format stream-json --output-format stream-json`) genuinely does
//! support `tool_use` → `tool_result` round-trips, but M9 stage A is the
//! **stream parser** only: the loop itself is stage B, and the flag flips in the
//! same commit as the loop (stage C). Advertising a capability we cannot honour
//! would make the agent loop branch into a path that does not exist.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;

use crate::auth::{self, Credential};
use crate::config::Config;
use crate::error::{truncate, PieError, Result};
use crate::jsonl::{self, LineRead};
use crate::process;
use crate::provider::driver::EventRouter;
use crate::provider::{
    AuthState, Capabilities, ChatRequest, Model, Provider, StreamEvent, TurnOutcome, Usage,
    UsageReport,
};

/// Provider id, in the registry, in errors and on the stream.
pub const BACKEND: &str = "claude";

/// Static catalog. `--model` also accepts a caller-supplied id, so this list is
/// a convenience, not a restriction (`knowledge/claude.md`).
pub const STATIC_MODELS: [(&str, &str); 2] = [
    ("claude-sonnet-4-6", "Claude Sonnet 4.6"),
    ("claude-opus-4-6", "Claude Opus 4.6"),
];

/// Accepted `--model` aliases.
pub const ALIASES: [&str; 2] = ["sonnet", "opus"];

/// The claude backend.
#[derive(Debug, Default, Clone, Copy)]
pub struct Claude;

#[async_trait]
impl Provider for Claude {
    fn id(&self) -> &'static str {
        BACKEND
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            // **Still `false`, and the reason changed in M9.** The original note said it would
            // flip "together with the tool loop itself". The loop now exists
            // (`src/tool_loop.rs`), and implementing it is what showed the note was wrong: a
            // capability answers "can this backend accept an injected `tool_result`?", and a
            // backend whose `stream` returns `Unsupported` cannot accept anything — you cannot
            // run a turn on it at all.
            //
            // So the flag flips when claude's **turn path** is wired and verified, which needs
            // a subscription this machine does not have. Setting it `true` now would have no
            // effect today (`check_supported` gates a path nothing can reach) while telling
            // every future reader that claude turns work. The invariant it keeps is the one
            // worth keeping: `tool_loop` is true exactly when this backend's `stream` can
            // answer a call.
            tool_loop: false,
            resume: true,
            stdin_prompt: true,
            max_prompt_argv: 0,
            // The one backend with a real system prompt: the persona goes
            // through `--append-system-prompt` rather than being folded into
            // the prompt text (`DESIGN.md` §10, `knowledge/claude.md`).
            system_prompt: true,
        }
    }

    fn binary(&self, cfg: &Config) -> Option<PathBuf> {
        process::resolve_binary(&cfg.claude.bin)
    }

    async fn auth_status(&self, cfg: &Config) -> AuthState {
        if self.binary(cfg).is_none() {
            return AuthState::BinaryMissing {
                path: cfg.claude.bin.clone(),
            };
        }
        match auth::claude_credential() {
            Credential::Subscription { .. } if cfg.claude.enabled => AuthState::Ready,
            Credential::Subscription { .. } => AuthState::Error(
                "a claude credential exists but `claude.enabled` is false; set it to true in \
                 config.toml to opt in"
                    .to_string(),
            ),
            // The expected state on this machine.
            Credential::Absent => AuthState::NoSubscription,
            Credential::ApiKeyOnly { evidence } => AuthState::Error(format!(
                "claude credential is API-key only ({evidence}); pie drives the subscription login only"
            )),
            Credential::Unreadable(why) => AuthState::Error(why),
        }
    }

    async fn list_models(&self, _cfg: &Config) -> Result<Vec<Model>> {
        // Static and free: listing models must never spend quota.
        Ok(STATIC_MODELS
            .iter()
            .map(|(id, display)| Model::plain(*id, *display))
            .collect())
    }

    async fn stream(
        &self,
        _cfg: &Config,
        _req: &ChatRequest,
        _sink: &mut (dyn FnMut(StreamEvent) + Send),
    ) -> Result<TurnOutcome> {
        Err(PieError::Unsupported {
            backend: BACKEND.into(),
            feature:
                "delegate turn (M9 — needs a Claude subscription to verify the stream envelope)"
                    .into(),
        })
    }

    async fn usage(&self, _cfg: &Config) -> Result<UsageReport> {
        Err(PieError::Unsupported {
            backend: BACKEND.into(),
            feature: "quota probe (M9 — print-mode `/usage` support is unverified)".into(),
        })
    }
}

/// Build the argv for one turn. Pure and unit-tested.
///
/// Written to spec from `knowledge/claude.md` and **unverified live**: there is
/// no Claude subscription on this machine, so `stream()` is still
/// [`PieError::Unsupported`] and nothing outside the tests calls this yet. It
/// exists now because M3's contract is "one mechanism per backend, one identical
/// persona", and the mechanism is the part that can be got wrong on paper.
///
/// ```text
/// claude -p <PROMPT> --output-format stream-json --dangerously-skip-permissions
///     --add-dir <CWD> [--model <ID>] [--append-system-prompt <PERSONA>]
/// ```
///
/// The prompt is **not** folded here: claude has a system role, so the persona
/// travels in `--append-system-prompt` and the task text stays the task text.
/// The bytes handed over are the same bytes agy and codex fold in.
pub fn build_args(cfg: &Config, req: &ChatRequest) -> Result<Vec<OsString>> {
    let _ = cfg;
    let mut args: Vec<OsString> = Vec::with_capacity(11);

    args.push(OsString::from("-p"));
    args.push(OsString::from(&req.prompt));
    args.push(OsString::from("--output-format"));
    args.push(OsString::from("stream-json"));
    args.push(OsString::from("--dangerously-skip-permissions"));
    args.push(OsString::from("--add-dir"));
    args.push(req.cwd.clone().into_os_string());

    if let Some(model) = &req.model {
        args.push(OsString::from("--model"));
        args.push(OsString::from(model));
    }

    // Never `Some("")`: an empty system prompt would still be an injection.
    if let Some(persona) = req.persona.as_deref().filter(|p| !p.trim().is_empty()) {
        args.push(OsString::from("--append-system-prompt"));
        args.push(OsString::from(persona));
    }

    Ok(args)
}

/// Build the argv for the **SDK-mode loop** (`DESIGN.md` §18, M9 stage B).
///
/// A separate function from [`build_args`] on purpose, and it must stay separate: the
/// `-p` one-shot path is asserted byte-for-byte by the existing tests, and adding
/// `--input-format stream-json` to it would make the CLI read its prompt from a stdin
/// nobody ever writes to — a hang, not an error.
///
/// ```text
/// claude -p --input-format stream-json --output-format stream-json
///     --dangerously-skip-permissions --add-dir <CWD>
///     [--model <ID>] [--append-system-prompt <PERSONA>]
/// ```
///
/// **The prompt is not in this argv.** In SDK mode the first frame written to stdin is
/// the `user` message carrying it, which is what makes the turn interactive at all; an
/// argv prompt alongside `--input-format stream-json` would either be a second copy of
/// the task or an error. Writing that frame is the loop's job, not the argv builder's.
///
/// The persona keeps the mechanism [`build_args`] established — `--append-system-prompt`,
/// never folded into the prompt — because `Capabilities::system_prompt` is `true` only for
/// claude and the loop must not become the one path where the bytes differ.
pub fn build_loop_args(cfg: &Config, req: &ChatRequest) -> Result<Vec<OsString>> {
    let _ = cfg;
    let mut args: Vec<OsString> = Vec::with_capacity(13);

    args.push(OsString::from("-p"));
    args.push(OsString::from("--input-format"));
    args.push(OsString::from("stream-json"));
    args.push(OsString::from("--output-format"));
    args.push(OsString::from("stream-json"));
    args.push(OsString::from("--dangerously-skip-permissions"));
    args.push(OsString::from("--add-dir"));
    args.push(req.cwd.clone().into_os_string());

    if let Some(model) = &req.model {
        args.push(OsString::from("--model"));
        args.push(OsString::from(model));
    }

    // Same rule as the one-shot path: never `Some("")`, because an empty system
    // prompt would still be an injection.
    if let Some(persona) = req.persona.as_deref().filter(|p| !p.trim().is_empty()) {
        args.push(OsString::from("--append-system-prompt"));
        args.push(OsString::from(persona));
    }

    Ok(args)
}

/// claude's word for a turn that worked (`result.subtype`).
///
/// Deliberately **not** agy's `SUCCESS`/`OK`: claude spells it lowercase, and
/// accepting a word the vendor never sends would let a mapping bug hide behind a
/// status nobody produced. Any other subtype is a failure — see
/// [`ClaudeRouter::outcome`].
pub const SUCCESS_SUBTYPES: [&str; 1] = ["success"];

/// Status recorded when a terminal `result` event arrived with no `subtype`.
///
/// The name says the field was **absent** rather than that the turn failed: the
/// signal is lost, and laundering that into `success` or `error` would hide the drift.
pub const UNKNOWN_STATUS: &str = "UNKNOWN";

/// Status recorded when stdout closed with no terminal event, but the process
/// exited 0 and answer text was captured.
pub const UNTERMINATED_STATUS: &str = "UNTERMINATED";

/// Status used for a failing terminal event that carried no `subtype` of its own.
pub const ERROR_STATUS: &str = "ERROR";

/// Kind forwarded for a JSON line that carries no `type` field at all.
pub const UNKNOWN_EVENT: &str = "unknown";

/// Kind forwarded for a content block that carries no `type` field.
pub const UNKNOWN_BLOCK: &str = "unknown_block";

/// Name used for a tool call the CLI reported without a name.
pub const UNNAMED_TOOL: &str = "unknown";

/// Whether a terminal subtype means the turn succeeded.
pub fn is_success_status(status: &str) -> bool {
    SUCCESS_SUBTYPES
        .iter()
        .any(|ok| status.trim().eq_ignore_ascii_case(ok))
}

/// The terminal `result` event, kept so [`ClaudeRouter::outcome`] can decide.
#[derive(Debug, Clone, Default)]
struct Terminal {
    /// `result.subtype`, verbatim — the vendor's own word for what happened.
    status: String,
    /// The answer, or the error text when `is_error` is set.
    text: String,
    /// `result.is_error`. A failure signal that does not depend on the subtype.
    is_error: bool,
    /// The error text, when this was an error terminal.
    error: Option<String>,
}

/// Accumulating parser for claude's `--output-format stream-json` envelope.
///
/// Tolerant by contract (`planning/RISKS.md` §3, invariant I5): a line that is not
/// JSON, an event kind from a newer claude, or a field that is missing or of the
/// wrong type is forwarded or skipped — never fatal, because the alternative is
/// throwing away a turn that actually succeeded.
///
/// # One line, several events
///
/// A claude assistant message carries a **list** of content blocks, and one message
/// may hold two text blocks and a `tool_use`. [`EventRouter::on_line`] can return
/// only one event, so that surface cannot express it, and using it alone would drop
/// a `tool_use` — the model then waits forever for a result pie never sent.
///
/// [`ClaudeRouter::on_line_all`] therefore exists and is the faithful surface: it
/// returns every event the line produced, in the CLI's own order. `on_line` queues
/// the remainder in a FIFO and hands it out one event at a time, which preserves the
/// order but **lags** — [`ClaudeRouter::drain_pending`] (or another `on_line_all`)
/// is what flushes the tail. The loop must use `on_line_all`.
#[derive(Debug, Default)]
pub struct ClaudeRouter {
    text: String,
    usage: Usage,
    conversation_id: Option<String>,
    terminal: Option<Terminal>,
    /// Events a multi-block message produced that `on_line` could not return yet.
    pending: VecDeque<StreamEvent>,
}

impl ClaudeRouter {
    /// A router with no accumulated state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Answer text accumulated from `text` blocks so far.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Usage accumulated from `assistant` messages and the terminal event.
    pub fn usage(&self) -> &Usage {
        &self.usage
    }

    /// Whether a terminal `result` event has been seen.
    pub fn saw_terminal(&self) -> bool {
        self.terminal.is_some()
    }

    /// Every event one stdout line produced, in the order claude sent them.
    ///
    /// This is the faithful surface for a multi-block assistant message; see the
    /// type-level note. Anything `on_line` left queued is returned first, so the
    /// two surfaces can be mixed without reordering the stream.
    pub fn on_line_all(&mut self, line: &str) -> Vec<StreamEvent> {
        for event in self.absorb(line) {
            self.pending.push_back(event);
        }
        self.drain_pending()
    }

    /// Hand back everything still queued by [`EventRouter::on_line`].
    pub fn drain_pending(&mut self) -> Vec<StreamEvent> {
        self.pending.drain(..).collect()
    }

    /// Parse one line into every event it produces. Never fatal.
    fn absorb(&mut self, line: &str) -> Vec<StreamEvent> {
        let value = match jsonl::parse_line(line) {
            LineRead::Blank => return Vec::new(),
            LineRead::Unparsed(raw) => return vec![StreamEvent::Unparsed(raw)],
            LineRead::Value(value) => value,
        };

        // claude stamps `session_id` on every event, not only on `init`, so a
        // stream whose `init` was lost still resumes. Read before dispatch, from
        // one place, so a renamed key cannot disable `--continue` in silence.
        if let Some(id) = session_id(&value) {
            self.conversation_id = Some(id);
        }

        let raw_kind = jsonl::s_nonempty(&value, "type").unwrap_or_default();
        match raw_kind.as_str() {
            "system" => self.on_system(&value),
            "assistant" => self.on_assistant(&value),
            // pie does **not** echo its own `tool_result` back as a stream event.
            // It is pie's own output coming back, so a `TextDelta` here would
            // double-count the tool output in the transcript.
            "user" => vec![StreamEvent::Other {
                kind: "user".to_string(),
                raw: value,
            }],
            "result" => vec![self.on_result(&value)],
            // Protocol drift is forwarded, never fatal (`planning/RISKS.md` §3).
            _ => vec![StreamEvent::Other {
                kind: if raw_kind.is_empty() {
                    UNKNOWN_EVENT.to_string()
                } else {
                    raw_kind
                },
                raw: value,
            }],
        }
    }

    /// `system` → the turn's `Init`, or a forwarded event for any other subtype.
    fn on_system(&mut self, line: &Value) -> Vec<StreamEvent> {
        let subtype = jsonl::s_nonempty(line, "subtype").unwrap_or_default();
        if !subtype.eq_ignore_ascii_case("init") {
            // `system` also carries `compact_boundary`, hook notifications and
            // other subtypes. Treating any `system` line as a turn start would
            // invent a conversation id and a tool list for a turn that is not
            // starting, so only `init` maps and the rest stays visible.
            return vec![StreamEvent::Other {
                kind: "system".to_string(),
                raw: line.clone(),
            }];
        }

        vec![StreamEvent::Init {
            conversation_id: session_id(line),
            cwd: jsonl::s_nonempty(line, "cwd").map(PathBuf::from),
            tools: name_list(jsonl::at(line, "tools")),
        }]
    }

    /// `assistant` → one event per content block, in order.
    fn on_assistant(&mut self, line: &Value) -> Vec<StreamEvent> {
        // Merged, never assigned wholesale: a later report that carries only
        // some fields must not zero the ones it omits (`Usage::merge`).
        let message = jsonl::at(line, "message").unwrap_or(line);
        if let Some(usage) =
            usage_from(jsonl::at(message, "usage").or_else(|| jsonl::at(line, "usage")))
        {
            self.usage.merge(usage);
        }

        // Once the terminal event has arrived the answer is already decided, and
        // a delta after `done` reads as the answer changing after it was final.
        if self.terminal.is_some() {
            return Vec::new();
        }

        // `content` is a list of blocks in the documented envelope; a bare string
        // is also legal for a message, and is exactly one text block.
        if let Some(text) = jsonl::at(message, "content").and_then(Value::as_str) {
            return self.text_block(text);
        }

        let mut events: Vec<StreamEvent> = Vec::new();
        for block in jsonl::objects(message, "content") {
            let block_type = jsonl::s_nonempty(&block, "type").unwrap_or_default();
            match block_type.as_str() {
                "text" => {
                    events.extend(self.text_block(&jsonl::s(&block, "text").unwrap_or_default()))
                }
                "tool_use" => {
                    // The call is surfaced even when it has no `id`: a `ToolUse`
                    // with nothing to echo is visible and fails loudly in the
                    // loop, while dropping the block makes the model wait for a
                    // result pie will never send — a hang, which is worse.
                    events.push(StreamEvent::ToolUse {
                        id: jsonl::s_nonempty(&block, "id")
                            .or_else(|| jsonl::s_nonempty(&block, "tool_use_id"))
                            .unwrap_or_default(),
                        name: jsonl::s_nonempty(&block, "name")
                            .unwrap_or_else(|| UNNAMED_TOOL.to_string()),
                        input: jsonl::at(&block, "input").cloned().unwrap_or(Value::Null),
                    });
                }
                // A block type we do not model (`thinking`, `redacted_thinking`,
                // or one from a newer claude) is forwarded rather than dropped:
                // silently skipping blocks is how a future `tool_use` spelling
                // would vanish without anything to contradict it.
                other => events.push(StreamEvent::Other {
                    kind: if other.is_empty() {
                        UNKNOWN_BLOCK.to_string()
                    } else {
                        other.to_string()
                    },
                    raw: block,
                }),
            }
        }

        events
    }

    /// Accumulate and emit one text block. An empty block produces no event.
    fn text_block(&mut self, text: &str) -> Vec<StreamEvent> {
        if text.is_empty() {
            return Vec::new();
        }
        self.text.push_str(text);
        vec![StreamEvent::TextDelta(text.to_string())]
    }

    /// `result` → the terminal event, plus the usage and text it carries.
    fn on_result(&mut self, line: &Value) -> StreamEvent {
        // claude puts the fields at the top level and `result` holds the *answer
        // text*. A future wrapper shape (`"result":{…}`) flattens instead of
        // breaking the parse — the object branch cannot collide with the string.
        let payload = match line.get("result") {
            Some(value @ Value::Object(_)) => value,
            _ => line,
        };

        if let Some(usage) = usage_from(
            jsonl::at(payload, "usage")
                .or_else(|| jsonl::at(line, "usage"))
                .or_else(|| jsonl::at(line, "message.usage")),
        ) {
            self.usage.merge(usage);
        }

        // The first terminal event wins. A stream that emits `success` and then a
        // stray `error_during_execution` (or vice versa) must not have the outcome
        // rewritten by whichever line happened to arrive last.
        if let Some(existing) = &self.terminal {
            return StreamEvent::Done {
                text: existing.text.clone(),
                status: existing.status.clone(),
            };
        }

        let status = string_field(payload, line, "subtype").unwrap_or_default();
        let is_error = bool_field(payload, line, "is_error");
        // The terminal `result` text is the whole answer; the accumulated deltas
        // are the fallback when it is absent or empty.
        let text = string_field(payload, line, "result")
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| self.text.clone());

        self.terminal = Some(Terminal {
            status: status.clone(),
            text: text.clone(),
            is_error,
            // On an error terminal claude puts the *error* in the same `result`
            // field, and it is the most specific explanation available.
            error: is_error
                .then(|| text.clone())
                .filter(|t| !t.trim().is_empty()),
        });

        StreamEvent::Done { text, status }
    }

    /// Assemble the finished turn. `driver::drive` overwrites `duration` with
    /// the measured wall clock.
    fn finish(&self, text: String, status: String) -> TurnOutcome {
        TurnOutcome {
            text,
            usage: self.usage.clone(),
            status,
            conversation_id: self.conversation_id.clone(),
            duration: Duration::ZERO,
        }
    }

    /// Best available explanation for a failed turn.
    ///
    /// claude's own error text is the most specific, so it wins; the bounded
    /// stderr tail is the fallback. Both are redacted — a vendor CLI can echo back
    /// whatever it was given (`planning/ACCEPTANCE.md` invariant I3).
    fn failure_detail(&self, terminal: &Terminal, stderr_tail: &str) -> String {
        if let Some(error) = terminal.error.as_deref().map(str::trim) {
            if !error.is_empty() {
                return truncate(&process::redact(error), 600);
            }
        }
        process::diagnostic(stderr_tail)
    }
}

impl EventRouter for ClaudeRouter {
    fn backend(&self) -> &'static str {
        BACKEND
    }

    fn on_line(&mut self, line: &str) -> Option<StreamEvent> {
        for event in self.absorb(line) {
            self.pending.push_back(event);
        }
        self.pending.pop_front()
    }

    /// **The drain is the trait's, not an extra.** `ClaudeRouter::drain_pending` existed as
    /// an inherent method, so a `drive` calling the *trait's* drain got the empty default
    /// and silently dropped every block after the first — a `tool_use` among them, which
    /// is a conversation that never finishes.
    ///
    /// Found by `drive_drains_the_extra_blocks_a_single_assistant_line_carries`, which
    /// spawns a real stand-in through `drive` and asserts the sink saw all three blocks of
    /// one mixed assistant message. The router's own tests could not have caught it: they
    /// call the inherent method directly, which proves the router is faithful and says
    /// nothing about whether anything ever reaches the drain through the trait.
    fn drain_pending(&mut self) -> Vec<StreamEvent> {
        self.pending.drain(..).collect()
    }

    fn outcome(
        &self,
        exit_code: Option<i32>,
        exit_status: &str,
        stderr_tail: &str,
    ) -> Result<TurnOutcome> {
        if let Some(terminal) = &self.terminal {
            let status = terminal.status.trim();

            if !terminal.is_error && is_success_status(status) {
                return Ok(self.finish(terminal.text.clone(), status.to_string()));
            }

            // A `result` event with neither `subtype` nor `is_error`: the turn did
            // complete, so the exit code is the only signal left. Missing metadata
            // must not sink a turn that otherwise worked, but the status we record
            // says the field was absent, so the drift stays visible downstream.
            if !terminal.is_error && status.is_empty() && exit_code == Some(0) {
                return Ok(self.finish(terminal.text.clone(), UNKNOWN_STATUS.to_string()));
            }

            // `error_max_turns`, `error_during_execution`, `is_error: true` — and
            // any subtype we have never seen. An unrecognised subtype is treated as
            // a failure on purpose: reporting a false success can burn a
            // subscription silently, while failing loudly names the new subtype in
            // the error.
            return Err(PieError::Backend {
                backend: BACKEND.to_string(),
                status: if !status.is_empty() {
                    status.to_string()
                } else if terminal.is_error {
                    ERROR_STATUS.to_string()
                } else {
                    exit_status.to_string()
                },
                detail: self.failure_detail(terminal, stderr_tail),
            });
        }

        // No terminal event at all. Two rules cover this and they disagree on the
        // error variant: `DESIGN.md` §6 rule 7 says `Backend`, the M1 brief says
        // `Protocol`. This follows the brief — `PieError::protocol`, exit 4 — while
        // folding rule 7's diagnostics (exit status + bounded stderr tail) into the
        // detail, so nothing rule 7 would have shown is lost.
        //
        // The exception is also the brief's: stdout that closed with exit code 0
        // *and* captured text is a partial answer, not a protocol failure, so it is
        // returned as a success whose status says the stream was truncated.
        if exit_code == Some(0) && !self.text.trim().is_empty() {
            return Ok(self.finish(self.text.clone(), UNTERMINATED_STATUS.to_string()));
        }

        let mut detail = format!("stdout closed without a terminal `result` event ({exit_status})");
        if !stderr_tail.trim().is_empty() {
            detail.push_str("; stderr: ");
            detail.push_str(&process::diagnostic(stderr_tail));
        }
        Err(PieError::protocol(BACKEND, detail))
    }
}

/// claude's conversation id: `session_id`, with `conversation_id` accepted as an
/// alias.
///
/// The envelope in `knowledge/claude.md` is documented rather than captured, and a
/// renamed key would otherwise disable `--continue` without a single failure.
fn session_id(line: &Value) -> Option<String> {
    jsonl::s_nonempty(line, "session_id").or_else(|| jsonl::s_nonempty(line, "conversation_id"))
}

/// A string field, preferring the nested payload over the top level.
///
/// `jsonl::s` stringifies numbers and booleans rather than dropping them, so a
/// field whose type drifted still yields something instead of nothing.
fn string_field(payload: &Value, line: &Value, key: &str) -> Option<String> {
    jsonl::s(payload, key).or_else(|| jsonl::s(line, key))
}

/// Whether `is_error` is set, on either the nested payload or the line.
///
/// Absent means `false`: the field is documented as always present, and the subtype is
/// still consulted, so a missing flag cannot turn a failure into a success.
fn bool_field(payload: &Value, line: &Value, key: &str) -> bool {
    jsonl::b(payload, key)
        .or_else(|| jsonl::b(line, key))
        .unwrap_or(false)
}

/// Tool names from either `["Bash", …]` or `[{"name":"Bash"}, …]`.
fn name_list(value: Option<&Value>) -> Vec<String> {
    let Some(Value::Array(items)) = value else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| match item {
            Value::String(s) => Some(s.clone()),
            other => jsonl::s_nonempty(other, "name")
                .or_else(|| jsonl::s_nonempty(other, "id"))
                .or_else(|| jsonl::s_nonempty(other, "type")),
        })
        .collect()
}

/// Map claude's `usage` object.
///
/// These are claude's own field names — the Anthropic API's: `cache_read_input_tokens`
/// is the read side and `cache_creation_input_tokens` is the **write** side, which is
/// why the second lands on `cache_write_tokens` and not on the read field. The codex
/// names (`cached_input_tokens`, `reasoning_output_tokens`) are never consulted, and
/// neither is agy's `thinking_tokens`: reading another backend's field is how a mapping
/// silently reports a number the vendor never sent (M1 asserts the same rule in the
/// other direction). `total_tokens` is left at zero for the same reason — claude
/// reports no total, and `Usage::computed_total` sums the parts that do exist.
///
/// `cache_read_tokens`/`cache_write_tokens` are accepted as aliases because the
/// envelope is documented rather than captured; the write side is unambiguously
/// `cache_creation_input_tokens`.
fn usage_from(value: Option<&Value>) -> Option<Usage> {
    let value = value?;
    let usage = Usage {
        input_tokens: jsonl::u(value, "input_tokens").unwrap_or(0),
        output_tokens: jsonl::u(value, "output_tokens").unwrap_or(0),
        thinking_tokens: 0,
        cache_read_tokens: jsonl::u(value, "cache_read_input_tokens")
            .or_else(|| jsonl::u(value, "cache_read_tokens"))
            .unwrap_or(0),
        cache_write_tokens: jsonl::u(value, "cache_creation_input_tokens")
            .or_else(|| jsonl::u(value, "cache_write_tokens"))
            .unwrap_or(0),
        total_tokens: 0,
        // claude is subscription-locked (and there is no subscription on this machine), so
        // it reports no cost. "Not reported" is a zero, per `Usage`.
        cost_usd_micros: 0,
    };
    // `Some` only when at least one recognised field is present, so a stray
    // `usage` key cannot wipe out usage already accumulated.
    (usage != Usage::default()).then_some(usage)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn strings(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|a| a.to_string_lossy().to_string())
            .collect()
    }

    #[test]
    fn the_persona_goes_through_append_system_prompt() {
        let persona = "# Persona\ntone: terse";
        let mut req = ChatRequest::new("hi");
        req.persona = Some(persona.to_string());

        let a = strings(&build_args(&Config::default(), &req).unwrap());
        let at = a
            .iter()
            .position(|x| x == "--append-system-prompt")
            .expect("claude takes the persona as a system prompt");
        assert_eq!(a[at + 1], persona, "the projection verbatim, not folded");
        assert_eq!(a[1], "hi", "the task text is not folded for claude");
        assert!(!a[1].contains(crate::profile::TASK_SEPARATOR));
    }

    #[test]
    fn without_a_persona_no_system_prompt_argument_is_added() {
        let a = strings(&build_args(&Config::default(), &ChatRequest::new("hi")).unwrap());
        assert!(!a.contains(&"--append-system-prompt".to_string()));

        let mut blank = ChatRequest::new("hi");
        blank.persona = Some("   ".into());
        let a = strings(&build_args(&Config::default(), &blank).unwrap());
        assert!(!a.contains(&"--append-system-prompt".to_string()));
    }

    #[test]
    fn argv_matches_the_documented_print_mode_shape() {
        let mut req = ChatRequest::new("hi");
        req.cwd = PathBuf::from("C:/work");
        req.model = Some("claude-sonnet-4-6".into());

        let a = strings(&build_args(&Config::default(), &req).unwrap());
        assert_eq!(a[0], "-p");
        for required in [
            "--output-format",
            "stream-json",
            "--dangerously-skip-permissions",
            "--add-dir",
            "C:/work",
            "--model",
            "claude-sonnet-4-6",
        ] {
            assert!(
                a.contains(&required.to_string()),
                "missing {required} in {a:?}"
            );
        }
    }

    #[test]
    fn does_not_advertise_a_tool_loop_it_cannot_run_a_turn_for() {
        // Inverted twice, and the second inversion is the interesting one. Stage A wrote
        // this asserting `false` "until M9 implements the loop"; stage C flipped it to
        // `true` on that reasoning; wiring it revealed the reasoning was wrong. The loop
        // exists, but `Claude::stream` returns `Unsupported`, so the backend cannot accept
        // an injected result — and a capability flag that says otherwise is a flag with
        // nothing behind it.
        //
        // The assertion is the PAIR, because either half alone can be satisfied by a lie:
        // the flag must be `false` *and* the turn path must be the thing that is missing.
        assert!(
            !Claude.capabilities().tool_loop,
            "claude's `stream` is unimplemented, so it must not advertise a tool loop"
        );
    }

    #[test]
    fn model_catalog_is_static_and_free() {
        let models = futures_lite_block_on(Claude.list_models(&Config::default())).unwrap();
        assert_eq!(models.len(), 2);
        assert!(models.iter().any(|m| m.id == "claude-sonnet-4-6"));
        assert!(models.iter().all(|m| m.efforts.is_empty()));
    }

    #[test]
    fn absent_credential_is_reported_as_no_subscription() {
        // claude is not authenticated on this machine; that must surface as
        // `no-subscription`, not as a crash or a login prompt.
        if auth::claude_credential() == Credential::Absent {
            assert_eq!(Credential::Absent.label(), "absent");
        }
        assert_eq!(AuthState::NoSubscription.label(), "no-subscription");
    }

    #[test]
    fn aliases_are_documented_not_validated_away() {
        assert!(ALIASES.contains(&"sonnet"));
        assert!(ALIASES.contains(&"opus"));
    }

    /// Minimal executor so the async trait method can be asserted without
    /// pulling a runtime into the test.
    fn futures_lite_block_on<F: std::future::Future>(fut: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(fut)
    }
}
