//! Google Antigravity (`agy`) backend — delegate mode.
//!
//! `agy --print` is one-shot: it runs its own tools and returns a final answer,
//! so pie can never inject a `tool_result` and the whole task goes to the CLI.
//! [`AgyRouter`] normalises the `stream-json` NDJSON it prints, [`Agy::stream`]
//! drives it through the shared driver, and the two zero-token surfaces
//! (`agy models`, `/usage`) are parsed and cached here as well so they never
//! spend model quota.
//!
//! Verified contract: `knowledge/agy.md`.

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::Value;

use crate::auth::{self, Credential};
use crate::cache;
use crate::config::Config;
use crate::error::{truncate, PieError, Result};
use crate::jsonl::{self, LineRead};
use crate::paths;
use crate::process;
use crate::provider::driver::{self, cap_for, enforce_argv_cap, EventRouter, TurnPlan};
use crate::provider::{
    AuthState, Bucket, Capabilities, ChatRequest, Model, Provider, StreamEvent, ToolState,
    TurnOutcome, Usage, UsageReport,
};
use crate::session_log;

/// The documented effort vocabulary, and the ranking used to pick a substitute.
///
/// This is a **fallback**, not a global truth. `agy models` is the source of
/// truth and the real sets are per-model and non-uniform: `gemini-3.1-pro`
/// offers no `medium`, `gpt-oss-120b` only `medium`, and the gemini/claude
/// families differ from each other (`tests/fixtures/agy_models.txt`). This list
/// is used only when no usable catalog entry is on disk, and it defines the
/// cheapest-first order that "closest effort" substitution ranks by.
pub const EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

/// agy has no stdin prompt path, so the prompt must fit in argv.
pub const PROMPT_ARGV_CAP: usize = 20_000;

/// Hint shown when a prompt exceeds the argv cap.
pub const PROMPT_CAP_HINT: &str =
    "agy has no stdin prompt path; shorten the prompt or split the task";

/// Provider id, also the `--model` prefix.
pub const BACKEND: &str = "agy";

/// Statuses agy reports for a turn that worked. Anything else is a failure.
pub const SUCCESS_STATUSES: [&str; 2] = ["SUCCESS", "OK"];

/// Status recorded when a terminal `result` event arrived without a `status`.
pub const UNKNOWN_STATUS: &str = "UNKNOWN";

/// Status recorded when stdout closed with no terminal event, but the process
/// exited 0 and answer text was captured.
pub const UNTERMINATED_STATUS: &str = "UNTERMINATED";

/// Kind forwarded for a JSON line that carries no `event` field at all.
pub const UNKNOWN_EVENT: &str = "unknown";

/// Name used for a tool step the CLI reported without a tool name.
pub const UNNAMED_TOOL: &str = "unknown";

/// Deadline for the two zero-token probes (`models`, `/usage`).
///
/// `usage_args()` passes `--print-timeout 0`, which tells agy to wait for the
/// turn to complete rather than cut it short (`knowledge/agy.md`), so agy places
/// no deadline of its own and this outer bound is the only one. It exists for a
/// CLI that hangs before the turn can finish. A probe must not inherit the
/// whole-turn budget.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(120);

/// Whether a terminal status means the turn succeeded.
pub fn is_success_status(status: &str) -> bool {
    SUCCESS_STATUSES
        .iter()
        .any(|ok| status.trim().eq_ignore_ascii_case(ok))
}

/// The terminal `result` event, kept so [`AgyRouter::outcome`] can decide.
#[derive(Debug, Clone, Default)]
struct Terminal {
    status: String,
    text: String,
    error: Option<String>,
}

/// Accumulating parser for `agy --output-format stream-json`.
///
/// Tolerant by contract (`planning/RISKS.md` §3, invariant I5): a line that is
/// not JSON, an event kind from a newer agy, or a field that is missing or of
/// the wrong type is forwarded or skipped — never fatal, because the
/// alternative is throwing away a turn that actually succeeded.
#[derive(Debug, Default)]
pub struct AgyRouter {
    text: String,
    usage: Usage,
    conversation_id: Option<String>,
    terminal: Option<Terminal>,
}

impl AgyRouter {
    /// A router with no accumulated state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Answer text accumulated from `text_delta` chunks so far.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Usage accumulated from step updates and the terminal event.
    pub fn usage(&self) -> &Usage {
        &self.usage
    }

    /// Whether a terminal `result` event has been seen.
    pub fn saw_terminal(&self) -> bool {
        self.terminal.is_some()
    }

    /// `init` → the conversation id we can resume, its cwd, and its tools.
    fn on_init(&mut self, line: &Value) -> StreamEvent {
        let init = payload(line, "init");
        let conversation_id = string_field(init, line, "conversation_id");
        let cwd = string_field(init, line, "cwd").map(PathBuf::from);
        let tools = {
            let nested = name_list(jsonl::at(init, "tools"));
            if nested.is_empty() {
                name_list(jsonl::at(line, "tools"))
            } else {
                nested
            }
        };

        if let Some(id) = &conversation_id {
            self.conversation_id = Some(id.clone());
        }

        StreamEvent::Init {
            conversation_id,
            cwd,
            tools,
        }
    }

    /// `step_update` → a text delta, a tool step, or nothing at all.
    fn on_step_update(&mut self, line: &Value) -> Option<StreamEvent> {
        let step = payload(line, "step_update");

        if let Some(usage) =
            usage_from(jsonl::at(step, "usage").or_else(|| jsonl::at(line, "usage")))
        {
            self.usage = usage;
        }

        let step_type = string_field(step, line, "step_type")
            .unwrap_or_default()
            .to_ascii_lowercase();

        if step_type == "tool" {
            return Some(StreamEvent::ToolStep {
                name: string_field(step, line, "tool_name")
                    .or_else(|| string_field(step, line, "name"))
                    .unwrap_or_else(|| UNNAMED_TOOL.to_string()),
                state: tool_state(string_field(step, line, "state").as_deref()),
                info: text_field(step, line, "tool_info")
                    .or_else(|| text_field(step, line, "info")),
            });
        }

        match string_field(step, line, "text_delta") {
            Some(delta) if !delta.is_empty() => {
                self.text.push_str(&delta);
                Some(StreamEvent::TextDelta(delta))
            }
            // A bare progress tick — `state: DONE`, no delta, no tool — carries
            // no user-visible state, so it produces no event at all.
            _ => None,
        }
    }

    /// `result` → the terminal event, plus the usage and text it carries.
    fn on_result(&mut self, line: &Value) -> StreamEvent {
        let result = payload(line, "result");

        if let Some(usage) =
            usage_from(jsonl::at(result, "usage").or_else(|| jsonl::at(line, "usage")))
        {
            self.usage = usage;
        }
        if let Some(id) = string_field(result, line, "conversation_id") {
            self.conversation_id = Some(id);
        }

        let status = string_field(result, line, "status").unwrap_or_default();
        // The terminal `response` is the whole answer; the accumulated deltas
        // are the fallback when it is absent or empty.
        let text = string_field(result, line, "response")
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| self.text.clone());

        self.terminal = Some(Terminal {
            status: status.clone(),
            text: text.clone(),
            error: string_field(result, line, "error"),
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
    /// agy's own `error` field is the most specific, so it wins; the bounded
    /// stderr tail is the fallback. Both are redacted — a vendor CLI can echo
    /// back whatever it was given (`planning/ACCEPTANCE.md` invariant I3).
    fn failure_detail(&self, terminal: &Terminal, stderr_tail: &str) -> String {
        if let Some(error) = terminal.error.as_deref().map(str::trim) {
            if !error.is_empty() {
                return truncate(&process::redact(error), 600);
            }
        }
        process::diagnostic(stderr_tail)
    }
}

impl EventRouter for AgyRouter {
    fn backend(&self) -> &'static str {
        BACKEND
    }

    fn on_line(&mut self, line: &str) -> Option<StreamEvent> {
        let value = match jsonl::parse_line(line) {
            LineRead::Blank => return None,
            LineRead::Unparsed(raw) => return Some(StreamEvent::Unparsed(raw)),
            LineRead::Value(value) => value,
        };

        let raw_kind = jsonl::s_nonempty(&value, "event").unwrap_or_default();
        let kind = raw_kind.to_ascii_lowercase();

        match kind.as_str() {
            "init" => Some(self.on_init(&value)),
            "step_update" => self.on_step_update(&value),
            "result" => Some(self.on_result(&value)),
            // Protocol drift is forwarded, never fatal (`planning/RISKS.md` §3).
            _ => Some(StreamEvent::Other {
                kind: if raw_kind.is_empty() {
                    UNKNOWN_EVENT.to_string()
                } else {
                    raw_kind
                },
                raw: value,
            }),
        }
    }

    fn outcome(
        &self,
        exit_code: Option<i32>,
        exit_status: &str,
        stderr_tail: &str,
    ) -> Result<TurnOutcome> {
        if let Some(terminal) = &self.terminal {
            let status = terminal.status.trim();

            if is_success_status(status) {
                return Ok(self.finish(terminal.text.clone(), status.to_string()));
            }

            // A `result` event with no status field: the turn did complete, so
            // the exit code is the only signal left. Missing metadata must not
            // sink a turn that otherwise worked, but the status we record says
            // the field was absent, so the drift stays visible downstream.
            if status.is_empty() && exit_code == Some(0) {
                return Ok(self.finish(terminal.text.clone(), UNKNOWN_STATUS.to_string()));
            }

            // `ERROR`/`FAILURE`/`CANCELLED`/`TIMEOUT` — and any status we have
            // never seen. An unrecognised status is treated as a failure on
            // purpose: reporting a false success can burn quota silently,
            // while failing loudly names the new status in the error.
            return Err(PieError::Backend {
                backend: BACKEND.to_string(),
                status: if status.is_empty() {
                    exit_status.to_string()
                } else {
                    status.to_string()
                },
                detail: self.failure_detail(terminal, stderr_tail),
            });
        }

        // No terminal event at all. Two rules cover this and they disagree on
        // the error variant: `DESIGN.md` §6 rule 7 says `Backend`, the M1 brief
        // says `Protocol`. This follows the brief — `PieError::protocol`, exit 4
        // — while folding rule 7's diagnostics (exit status + bounded stderr
        // tail) into the detail, so nothing rule 7 would have shown is lost.
        //
        // The exception is also the brief's: stdout that closed with exit code 0
        // *and* captured text is a partial answer, not a protocol failure, so it
        // is returned as a success whose status says the stream was truncated.
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

/// The nested payload of `{"event":"result","result":{…}}`.
///
/// Falls back to the whole line when the CLI inlines the fields, so a schema
/// change flattens instead of breaking the parse.
fn payload<'a>(line: &'a Value, key: &str) -> &'a Value {
    match line.get(key) {
        Some(value @ Value::Object(_)) => value,
        _ => line,
    }
}

/// A string field, preferring the nested payload over the top level.
///
/// `jsonl::s` stringifies numbers and booleans rather than dropping them, so a
/// field whose type drifted still yields something instead of nothing.
fn string_field(payload: &Value, line: &Value, key: &str) -> Option<String> {
    jsonl::s(payload, key).or_else(|| jsonl::s(line, key))
}

/// A string field that renders an object/array as a short JSON snippet.
fn text_field(payload: &Value, line: &Value, key: &str) -> Option<String> {
    match jsonl::at(payload, key).or_else(|| jsonl::at(line, key)) {
        Some(Value::String(s)) if !s.trim().is_empty() => Some(s.clone()),
        Some(Value::Null) | None => None,
        Some(other) => Some(jsonl::brief(other, 300)),
    }
}

/// Tool names from either `["view_file", …]` or `[{"name":"view_file"}, …]`.
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

/// Map an agy `usage` object.
///
/// These are agy's own field names and deliberately **not** the codex ones:
/// agy reports `thinking_tokens` and `cache_read_tokens` (`DESIGN.md` §9), so
/// `reasoning_output_tokens` and `cached_input_tokens` are never consulted.
/// `Some` only when at least one recognised field is present, so a stray
/// `usage` key cannot wipe out usage we already accumulated.
fn usage_from(value: Option<&Value>) -> Option<Usage> {
    let value = value?;
    let usage = Usage {
        input_tokens: jsonl::u(value, "input_tokens").unwrap_or(0),
        output_tokens: jsonl::u(value, "output_tokens").unwrap_or(0),
        thinking_tokens: jsonl::u(value, "thinking_tokens").unwrap_or(0),
        cache_read_tokens: jsonl::u(value, "cache_read_tokens").unwrap_or(0),
        cache_write_tokens: jsonl::u(value, "cache_write_tokens").unwrap_or(0),
        total_tokens: jsonl::u(value, "total_tokens").unwrap_or(0),
    };
    (usage != Usage::default()).then_some(usage)
}

/// agy's tool lifecycle state (`ACTIVE|DONE|ERROR`).
fn tool_state(raw: Option<&str>) -> ToolState {
    match raw.unwrap_or_default().trim().to_ascii_uppercase().as_str() {
        "DONE" | "COMPLETE" | "COMPLETED" | "SUCCESS" | "SUCCEEDED" | "FINISHED" => ToolState::Done,
        "ERROR" | "FAILED" | "FAILURE" | "CANCELLED" | "CANCELED" => ToolState::Error,
        // `ACTIVE` and anything we have never seen: a step we cannot classify is
        // reported as still running rather than dropped or claimed finished.
        // Tool progress is cosmetic; being wrong about "done" is not.
        _ => ToolState::Active,
    }
}

/// The agy backend.
#[derive(Debug, Default, Clone, Copy)]
pub struct Agy;

#[async_trait]
impl Provider for Agy {
    fn id(&self) -> &'static str {
        "agy"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            // `agy --print` is one-shot: it runs its own tools and returns a
            // final answer, so pie can never inject a tool_result (RISKS §1).
            tool_loop: false,
            resume: true,
            stdin_prompt: false,
            max_prompt_argv: PROMPT_ARGV_CAP,
        }
    }

    fn binary(&self, cfg: &Config) -> Option<PathBuf> {
        process::resolve_binary(&cfg.agy.bin)
    }

    async fn auth_status(&self, cfg: &Config) -> AuthState {
        if self.binary(cfg).is_none() {
            return AuthState::BinaryMissing {
                path: cfg.agy.bin.clone(),
            };
        }
        match auth::agy_credential() {
            Credential::Subscription { .. } => AuthState::Ready,
            Credential::ApiKeyOnly { evidence } => AuthState::Error(format!(
                "agy reports an API-key credential ({evidence}); pie drives the subscription login only"
            )),
            Credential::Absent => AuthState::NeedsLogin {
                hint: "run `agy` interactively and complete the Google login".to_string(),
            },
            Credential::Unreadable(why) => AuthState::Error(why),
        }
    }

    async fn list_models(&self, cfg: &Config) -> Result<Vec<Model>> {
        let cache_path = paths::agy_model_cache();
        let ttl = Duration::from_secs(cfg.agy.model_cache_ttl_secs);

        // A fresh catalog is served from disk: `pie models` must never spawn a
        // CLI it does not have to. This happens *before* resolving the binary,
        // so a cache hit works even while agy is missing (invariant I7).
        if let Some(models) =
            cache::read_fresh::<Vec<Model>>(&cache_path, ttl, session_log::now_millis())
        {
            return Ok(models);
        }

        let plan = self.probe_plan(cfg, models_args(), PROBE_TIMEOUT)?;
        let lines = driver::collect_lines(BACKEND, &plan).await?;
        let mut models = parse_models(&lines);

        if models.is_empty() {
            // Never cache an empty catalog: that would serve "no models" for
            // the whole TTL.
            return Err(PieError::protocol(
                BACKEND,
                "`agy models` printed no model rows",
            ));
        }

        // The per-model default is advisory: `build_args` recomputes it from the
        // live config, so a config change is honoured even on a cached catalog.
        // Here it lets `pie models` say which effort a bare `--model` will use.
        for model in &mut models {
            model.default_effort = cfg
                .agy
                .default_effort
                .as_deref()
                .and_then(|want| closest_effort(want, &model.efforts));
        }

        // Caching is a nicety, never a reason to fail the command.
        if let Err(e) = cache::write_now(&cache_path, &models) {
            tracing::debug!(error = %e, "could not write the agy model cache");
        }

        Ok(models)
    }

    async fn stream(
        &self,
        cfg: &Config,
        req: &ChatRequest,
        sink: &mut (dyn FnMut(StreamEvent) + Send),
    ) -> Result<TurnOutcome> {
        let mut plan = build_plan(cfg, req)?;
        // `build_plan` carries the configured bin *name*; spawning the resolved
        // path instead keeps the child identical on every platform (and turns a
        // missing binary into a clear error rather than an opaque spawn one).
        plan.program = self.binary(cfg).ok_or_else(|| PieError::BinaryMissing {
            backend: BACKEND.to_string(),
            path: cfg.agy.bin.clone(),
        })?;

        let started = Instant::now();
        let mut router = AgyRouter::new();
        driver::drive(plan, &mut router, sink, started).await
    }

    async fn usage(&self, cfg: &Config) -> Result<UsageReport> {
        // `usage_args()` deliberately omits `--disable-slash-commands`: `/usage`
        // is a slash expansion and must stay one (`knowledge/agy.md`).
        let plan = self.probe_plan(cfg, usage_args(), PROBE_TIMEOUT)?;
        let lines = driver::collect_lines(BACKEND, &plan).await?;
        Ok(parse_usage(&lines))
    }
}

impl Agy {
    /// A runnable plan for a non-turn probe (`models`, `/usage`).
    fn probe_plan(&self, cfg: &Config, args: Vec<OsString>, timeout: Duration) -> Result<TurnPlan> {
        let program = self.binary(cfg).ok_or_else(|| PieError::BinaryMissing {
            backend: BACKEND.to_string(),
            path: cfg.agy.bin.clone(),
        })?;

        Ok(TurnPlan {
            program,
            args,
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            scrub: cfg.scrub_keys(cfg.scrub_enabled()),
            timeout,
        })
    }
}

/// Build the argv for one turn. Pure and unit-tested — no process involved.
///
/// Verified shape (`knowledge/agy.md`):
/// ```text
/// agy --print <PROMPT> --dangerously-skip-permissions --disable-slash-commands
///     --output-format stream-json --print-timeout <N>s --add-dir <CWD>
///     [--model <ID> --effort <E>] [--conversation <ID>]
/// ```
pub fn build_args(cfg: &Config, req: &ChatRequest) -> Result<Vec<OsString>> {
    let mut args: Vec<OsString> = Vec::with_capacity(14);

    // `--print` consumes the next token as the prompt.
    args.push(OsString::from("--print"));
    args.push(OsString::from(&req.prompt));

    args.push(OsString::from("--dangerously-skip-permissions"));
    // Keeps a leading `/` in a prompt from being expanded as a slash command.
    args.push(OsString::from("--disable-slash-commands"));
    args.push(OsString::from("--output-format"));
    args.push(OsString::from("stream-json"));
    args.push(OsString::from("--print-timeout"));
    args.push(OsString::from(format!("{}s", cfg.agy.print_timeout_secs)));
    args.push(OsString::from("--add-dir"));
    args.push(req.cwd.clone().into_os_string());

    if let Some(model) = &req.model {
        // agy rejects `--model` without `--effort`, so one must be supplied —
        // and it has to be an effort *this* model offers, because the catalog is
        // per-model and non-uniform (`gemini-3.1-pro` has no `medium`).
        //
        // Validation runs against the on-disk catalog (a stale entry still beats
        // none). A model agy added since that entry was written is therefore
        // validated against `EFFORTS`, the documented vocabulary.
        let supported = supported_efforts(model);
        let valid = supported.join("|");

        let effort = match req
            .effort
            .as_deref()
            .map(str::trim)
            .filter(|e| !e.is_empty())
        {
            // An explicit `--effort` is an instruction: refuse a value the model
            // does not offer rather than build an argv agy will reject.
            Some(wanted) => {
                if !supported
                    .iter()
                    .any(|known| known.eq_ignore_ascii_case(wanted))
                {
                    return Err(PieError::Config(format!(
                        "agy model `{model}` does not offer effort `{wanted}` — \
                         valid efforts: {valid}"
                    )));
                }
                wanted.to_string()
            }
            // A configured default is a preference, not an instruction: pick the
            // closest effort the model actually offers rather than a doomed
            // value. Any substitution is announced on stderr, never silent.
            None => {
                match cfg
                    .agy
                    .default_effort
                    .as_deref()
                    .map(str::trim)
                    .filter(|e| !e.is_empty())
                {
                    Some(configured) => match closest_effort(configured, &supported) {
                        Some(chosen) => {
                            if !chosen.eq_ignore_ascii_case(configured) {
                                tracing::warn!(
                                    model = %model,
                                    configured = %configured,
                                    using = %chosen,
                                    "agy does not offer the configured effort for this model; \
                                     using the closest supported one"
                                );
                            }
                            chosen
                        }
                        None => {
                            return Err(PieError::Config(format!(
                                "agy model `{model}` reports no efforts — pass `--effort <{valid}>`"
                            )))
                        }
                    },
                    None => {
                        return Err(PieError::Config(format!(
                            "agy requires an effort whenever a model is chosen, but no `--effort` \
                             was given and `agy.default_effort` is unset — pass \
                             `--effort <{valid}>`"
                        )))
                    }
                }
            }
        };

        args.push(OsString::from("--model"));
        args.push(OsString::from(model));
        args.push(OsString::from("--effort"));
        args.push(OsString::from(effort));
    }

    if let Some(conversation) = &req.resume {
        args.push(OsString::from("--conversation"));
        args.push(OsString::from(conversation));
    }

    Ok(args)
}

/// argv for the quota probe.
///
/// Deliberately **without** `--disable-slash-commands`: `/usage` is a slash
/// expansion, and disabling expansion would make it a literal prompt
/// (`knowledge/agy.md`).
///
/// `--print-timeout 0` is passed even though `0` means "wait for the turn to
/// complete" — the flag is here to *pin* that behaviour rather than inherit
/// `agy.print_timeout_secs`, and it is not cosmetic: a shorter timeout is
/// actively wrong. At `30s` the slash expansion did not finish inside the window
/// and the capture came back as an empty `response` with no quota payload; at `0`
/// it returns at once with the full table (`tests/fixtures/agy_usage.json`). Do
/// not "protect" this probe with a timeout.
pub fn usage_args() -> Vec<OsString> {
    [
        "--print",
        "/usage",
        "--output-format",
        "json",
        "--print-timeout",
        "0",
    ]
    .iter()
    .map(OsString::from)
    .collect()
}

/// argv for the zero-token model catalog.
pub fn models_args() -> Vec<OsString> {
    vec![OsString::from("models")]
}

/// Parse `agy models` output into one [`Model`] per **base** id.
///
/// The live listing is one row per *(model, effort)* pair —
/// `id<TAB>Display Name (Effort)` — behind a `Fetching available models...`
/// banner, and the sets are per-model and non-uniform (`gemini-3.1-pro` offers
/// low|high, `gpt-oss-120b` only medium; `tests/fixtures/agy_models.txt`). Rows
/// are therefore **grouped by base id**, so one [`Model`] carries the real effort
/// set that `--model` + `--effort` needs. A row is accepted only when its first
/// field looks like a model id, so banner noise, and any decoration a later
/// build adds, is skipped rather than turned into a phantom model.
pub fn parse_models(lines: &[String]) -> Vec<Model> {
    let mut models: Vec<Model> = Vec::new();

    for line in lines {
        let Some(row) = parse_catalog_row(line) else {
            continue;
        };

        match models.iter_mut().find(|seen| seen.id == row.base_id) {
            Some(existing) => {
                if let Some(effort) = row.effort {
                    if !existing
                        .efforts
                        .iter()
                        .any(|known| known.eq_ignore_ascii_case(&effort))
                    {
                        existing.efforts.push(effort);
                    }
                }
            }
            None => models.push(Model {
                id: row.base_id,
                display: row.base_display,
                efforts: row.effort.into_iter().collect(),
                default_effort: None,
            }),
        }
    }

    // Canonical order, cheapest first, so `efforts` reads the same however the
    // CLI happened to order its rows (`agy models` prints high → low).
    for model in &mut models {
        model.efforts.sort_by_key(|effort| effort_rank(effort));
    }

    models
}

/// One `agy models` row, split into base id, base display name, and effort.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CatalogRow {
    base_id: String,
    base_display: String,
    effort: Option<String>,
}

fn parse_catalog_row(line: &str) -> Option<CatalogRow> {
    let row = line.trim();
    let (id, display) = row.split_once(char::is_whitespace)?;
    let id = id.trim();
    if !looks_like_model_id(id) {
        return None;
    }
    let display = display.trim();
    let display = if display.is_empty() { id } else { display };

    // The id encodes the effort as a suffix, the display as a parenthetical.
    // Only vocabulary words count as efforts, so a base id such as
    // `gpt-oss-120b` or `claude-opus-5-5` is never split by guesswork.
    let (base_display, display_effort) = split_display_effort(display);
    let (base_id, id_effort) = match id.rsplit_once('-') {
        Some((base, suffix)) if is_effort_word(suffix) => {
            (base.to_string(), Some(suffix.trim().to_ascii_lowercase()))
        }
        _ => (id.to_string(), None),
    };

    Some(CatalogRow {
        base_id,
        base_display,
        // The display is preferred; the id suffix is the fallback. On the
        // recorded catalog the two always agree (`tests/parse_agy.rs`).
        effort: display_effort.or(id_effort),
    })
}

/// Split `"Gemini 3.1 Pro (High)"` into `("Gemini 3.1 Pro", Some("high"))`.
///
/// A parenthetical that is not an effort word (`(Preview)`) stays part of the
/// display name.
fn split_display_effort(display: &str) -> (String, Option<String>) {
    let display = display.trim();
    if let Some(open) = display.rfind('(') {
        if let Some(inner) = display[open + 1..].strip_suffix(')') {
            let effort = inner.trim().to_ascii_lowercase();
            if is_effort_word(&effort) {
                return (display[..open].trim().to_string(), Some(effort));
            }
        }
    }
    (display.to_string(), None)
}

/// Conservative: lowercase, no spaces, and carrying a digit or a dash.
///
/// The last clause is what stops a lowercase notice (`fetching models`) from
/// being read as a model id.
fn looks_like_model_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 80
        && id.chars().any(|c| c.is_ascii_digit() || c == '-')
        && id.chars().all(|c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.' | '/' | ':')
        })
}

/// Whether a word is one of the documented efforts.
fn is_effort_word(word: &str) -> bool {
    EFFORTS
        .iter()
        .any(|known| known.eq_ignore_ascii_case(word.trim()))
}

/// Rank of an effort in the canonical vocabulary, cheapest first.
///
/// A name outside the vocabulary ranks after every known one, so it can still be
/// grouped but never displaces a known effort as "closest".
fn effort_rank(effort: &str) -> usize {
    let wanted = effort.trim();
    EFFORTS
        .iter()
        .position(|known| known.eq_ignore_ascii_case(wanted))
        .unwrap_or(EFFORTS.len())
}

/// The closest effort a model actually offers, or `None` when it offers none.
///
/// Exact match first (case-insensitive), otherwise the smallest distance in the
/// [`EFFORTS`] ranking. **On a tie the lower effort wins**: substituting upward
/// would spend more of the user's subscription than they asked for
/// (`planning/RISKS.md` §7).
pub fn closest_effort(want: &str, supported: &[String]) -> Option<String> {
    if supported.is_empty() {
        return None;
    }
    if let Some(exact) = supported
        .iter()
        .find(|known| known.eq_ignore_ascii_case(want.trim()))
    {
        return Some(exact.clone());
    }

    let target = effort_rank(want);
    let mut best: Option<(&String, usize)> = None;

    for candidate in supported {
        let distance = effort_rank(candidate).abs_diff(target);
        let better = match best {
            None => true,
            Some((current, best_distance)) => {
                distance < best_distance
                    || (distance == best_distance && effort_rank(candidate) < effort_rank(current))
            }
        };
        if better {
            best = Some((candidate, distance));
        }
    }

    best.map(|(candidate, _)| candidate.clone())
}

/// The effort set to validate a model against.
///
/// The on-disk catalog is the source of truth, and a **stale** entry still beats
/// no entry. `build_args` stays pure and synchronous, so nothing is spawned: a
/// cache miss simply falls back to [`EFFORTS`], the documented vocabulary.
fn supported_efforts(model: &str) -> Vec<String> {
    if let Some(cached) = cache::read::<Vec<Model>>(&paths::agy_model_cache()) {
        if let Some(found) = cached.value.iter().find(|m| m.id == model) {
            if !found.efforts.is_empty() {
                return found.efforts.clone();
            }
        }
    }
    EFFORTS.iter().map(|effort| (*effort).to_string()).collect()
}

/// Shown when `/usage` returns no bucket data at all.
///
/// This is **not** "zero quota". The note fires only for a genuinely
/// payload-less envelope — one carrying no `command.data` quota groups at all —
/// so the buckets are **missing**, not zero. Saying so stops an empty table being
/// read as either "you have quota" or "you are out of quota". It deliberately
/// does not claim the CLI cannot report quota: the recorded capture disproves
/// that (`tests/fixtures/agy_usage.json`).
pub const MISSING_QUOTA_NOTE: &str =
    "no quota buckets: the `/usage` envelope carried no `command.data` quota groups — the \
     buckets are MISSING, not zero; run `/usage` in agy itself to confirm";

/// Parse the `agy --print /usage --output-format json` payload.
///
/// The verified payload (`tests/fixtures/agy_usage.json`, agy 1.3.3) carries the
/// quota table at `command.data.groups[].buckets[]`. That explicit path is tried
/// first because only it knows which group a bucket belongs to. When it is absent
/// — a different envelope, or a newer agy — the shape-based [`collect_buckets`]
/// fallback runs instead, so schema drift degrades to a partial map instead of an
/// empty table (`planning/RISKS.md` §3). Anything neither path can map becomes a
/// note, never an error.
pub fn parse_usage(lines: &[String]) -> UsageReport {
    let mut report = UsageReport {
        backend: BACKEND.to_string(),
        buckets: Vec::new(),
        source: "agy --print /usage".to_string(),
        notes: Vec::new(),
    };

    let payloads = json_payloads(lines);
    if payloads.is_empty() {
        report
            .notes
            .push("`agy --print /usage` printed no JSON payload".to_string());
        return report;
    }

    for payload in &payloads {
        if let Some(status) = jsonl::s_nonempty(payload, "status") {
            if !is_success_status(&status) {
                report
                    .notes
                    .push(format!("`/usage` reported status `{status}`"));
            }
        }
        if let Some(error) = jsonl::s_nonempty(payload, "error") {
            report.notes.push(format!(
                "`/usage` error: {}",
                truncate(&process::redact(&error), 200)
            ));
        }

        let before = report.buckets.len();
        collect_from(payload, &mut report.buckets);

        // A slash expansion may hand the quota JSON back as a *string*, so the
        // `response` is followed — but only when the payload itself yielded
        // nothing. On the verified capture `response` is a human-readable table
        // of the same numbers, so following it unconditionally would add a
        // spurious "unmapped response" note to a payload that parsed perfectly.
        if report.buckets.len() == before {
            if let Some(response) = jsonl::s_nonempty(payload, "response") {
                match serde_json::from_str::<Value>(&response) {
                    Ok(inner) => collect_from(&inner, &mut report.buckets),
                    Err(_) => report.notes.push(format!(
                        "unmapped /usage response: {}",
                        truncate(&process::redact(&response), 200)
                    )),
                }
            }
        }
    }

    if report.buckets.is_empty() {
        // Distinguish a genuinely payload-less envelope from one that carried a
        // quota payload we could not map, so the note names the real problem.
        if payloads
            .iter()
            .any(|payload| jsonl::at(payload, "command.data").is_some())
        {
            report.notes.push(
                "`/usage` returned a quota payload but no buckets could be mapped".to_string(),
            );
        } else {
            report.notes.push(MISSING_QUOTA_NOTE.to_string());
        }
    }

    report
}

/// Every JSON value in the capture — or the whole capture as one value, for a
/// pretty-printed payload.
fn json_payloads(lines: &[String]) -> Vec<Value> {
    let mut values: Vec<Value> = lines
        .iter()
        .filter_map(|line| match jsonl::parse_line(line) {
            LineRead::Value(value) => Some(value),
            _ => None,
        })
        .collect();

    if values.is_empty() {
        let joined = lines.join("\n");
        if let Ok(value) = serde_json::from_str::<Value>(joined.trim()) {
            values.push(value);
        }
    }

    values
}

/// Map whatever quota the payload carries, preferring the verified path.
///
/// [`explicit_buckets`] runs first and short-circuits: when the verified path
/// produced buckets, the shape-based scan is skipped, because it does not know
/// group membership and would mint duplicate, indistinguishable rows.
fn collect_from(value: &Value, out: &mut Vec<Bucket>) {
    if explicit_buckets(value, out) == 0 {
        collect_buckets(value, out);
    }
}

/// The verified `/usage` path: `command.data.groups[].buckets[]`.
///
/// Returns the number of buckets mapped; `0` means the envelope is not the
/// verified one — a flat or renamed payload — and the caller should fall back to
/// the shape-based collector (`planning/RISKS.md` §3).
fn explicit_buckets(payload: &Value, out: &mut Vec<Bucket>) -> usize {
    let Some(Value::Array(groups)) = jsonl::at(payload, "command.data.groups") else {
        return 0;
    };

    let before = out.len();
    for (index, group) in groups.iter().enumerate() {
        let name =
            jsonl::s_nonempty(group, "name").unwrap_or_else(|| format!("group {}", index + 1));
        let Some(Value::Array(buckets)) = jsonl::at(group, "buckets") else {
            continue;
        };
        for (position, bucket) in buckets.iter().enumerate() {
            out.push(explicit_bucket(bucket, &name, position));
        }
    }
    out.len() - before
}

/// One entry of the verified `buckets[]` array, labelled with its group.
///
/// The **group** name leads the label: on the verified payload both groups
/// contain a bucket whose `window` is `weekly`, and both are named "Weekly Limit
/// Remaining", so a label built from the bucket alone renders two rows a reader
/// cannot tell apart (`knowledge/agy.md`). `window` stays visible because that is
/// the part users reason about.
fn explicit_bucket(bucket: &Value, group: &str, index: usize) -> Bucket {
    let window = jsonl::s_nonempty(bucket, "window")
        .or_else(|| jsonl::s_nonempty(bucket, "name"))
        .unwrap_or_else(|| format!("bucket {}", index + 1));
    let (remaining_fraction, used, limit) = measures(bucket);

    Bucket {
        label: format!("{group} / {window}"),
        remaining_fraction,
        reset_at: jsonl::s_nonempty(bucket, "reset_time"),
        used,
        limit,
    }
}

/// Fallback collector: find bucket-shaped arrays anywhere in the payload.
///
/// Kept deliberately. It is what still maps a payload from a build whose envelope
/// is not the verified one, so schema drift degrades coverage instead of emptying
/// the table (`planning/RISKS.md` §3). [`explicit_buckets`] is tried first and is
/// preferred, because only it knows group membership.
fn collect_buckets(value: &Value, out: &mut Vec<Bucket>) {
    match value {
        Value::Array(items) if !items.is_empty() && items.iter().all(looks_like_bucket) => {
            for (index, item) in items.iter().enumerate() {
                out.push(bucket_from(item, index));
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_buckets(item, out);
            }
        }
        Value::Object(map) => {
            for nested in map.values() {
                collect_buckets(nested, out);
            }
        }
        _ => {}
    }
}

/// Keys that make an object plausibly a quota bucket.
const BUCKET_KEYS: &[&str] = &[
    "label",
    "name",
    "bucket",
    "window",
    "period",
    "group",
    "title",
    "remaining",
    "remaining_fraction",
    "remainingFraction",
    "remaining_percent",
    "remainingPercent",
    "reset_at",
    "resetAt",
    "resets_at",
    "resetsAt",
    "used",
    "limit",
    "total",
    "quota",
];

fn looks_like_bucket(value: &Value) -> bool {
    match value {
        Value::Object(map) => BUCKET_KEYS.iter().any(|key| map.contains_key(*key)),
        _ => false,
    }
}

/// Keys carrying the consumed count, shared by both collectors.
const USED_KEYS: &[&str] = &["used", "used_units", "consumed", "count"];

/// Keys carrying the limit count, shared by both collectors.
const LIMIT_KEYS: &[&str] = &["limit", "total", "quota", "max", "capacity", "allowance"];

/// The measurable fields of one bucket object: fraction remaining, used, limit.
///
/// Shared so the verified path and the shape-based fallback read the numbers the
/// same way; only the label differs between them.
fn measures(item: &Value) -> (Option<f64>, Option<u64>, Option<u64>) {
    let used = first_u64(item, USED_KEYS);
    let limit = first_u64(item, LIMIT_KEYS);
    (remaining_fraction(item, used, limit), used, limit)
}

/// Map one bucket-looking object, tolerating every field it may not have.
fn bucket_from(item: &Value, index: usize) -> Bucket {
    let label = [
        "label", "name", "bucket", "window", "period", "group", "title", "id",
    ]
    .iter()
    .find_map(|key| jsonl::s_nonempty(item, key))
    .unwrap_or_else(|| format!("bucket {}", index + 1));

    let (remaining_fraction, used, limit) = measures(item);

    Bucket {
        label,
        remaining_fraction,
        reset_at: [
            "reset_at",
            "resetAt",
            "resets_at",
            "resetsAt",
            "reset",
            "reset_time",
            "resetTime",
        ]
        .iter()
        .find_map(|key| jsonl::s_nonempty(item, key)),
        used,
        limit,
    }
}

fn first_u64(value: &Value, keys: &[&str]) -> Option<u64> {
    keys.iter().find_map(|key| jsonl::u(value, key))
}

fn first_f64(value: &Value, keys: &[&str]) -> Option<f64> {
    keys.iter().find_map(|key| match jsonl::at(value, key)? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    })
}

/// Remaining share of a bucket, 0.0–1.0, however the CLI chose to express it.
fn remaining_fraction(value: &Value, used: Option<u64>, limit: Option<u64>) -> Option<f64> {
    let fraction = first_f64(
        value,
        &["remaining_fraction", "remainingFraction", "remaining"],
    );
    // A `remaining` above 1 is a count, not a share — fall through to
    // `used`/`limit` rather than report 4237%.
    if let Some(fraction) = fraction.filter(|f| (0.0..=1.0).contains(f)) {
        return Some(fraction);
    }

    let percent = first_f64(
        value,
        &[
            "remaining_percent",
            "remainingPercent",
            "percent_remaining",
            "percent",
        ],
    );
    if let Some(percent) = percent {
        return Some((percent / 100.0).clamp(0.0, 1.0));
    }

    let limit = limit.filter(|l| *l > 0)?;
    let used = used.unwrap_or(0).min(limit);
    Some((limit - used) as f64 / limit as f64)
}

/// Build a runnable plan, enforcing the argv cap.
pub fn build_plan(cfg: &Config, req: &ChatRequest) -> Result<TurnPlan> {
    let program = cfg.agy.bin.clone();
    let program_path = PathBuf::from(&program);
    let args = build_args(cfg, req)?;

    let cap = cap_for(&self_caps(), cfg);
    enforce_argv_cap("agy", &program_path, &args, cap, PROMPT_CAP_HINT)?;

    Ok(TurnPlan {
        program: program_path,
        args,
        cwd: req.cwd.clone(),
        scrub: cfg.scrub_keys(cfg.scrub_enabled()),
        timeout: req.timeout.unwrap_or_else(|| cfg.timeout()),
    })
}

fn self_caps() -> Capabilities {
    Agy.capabilities()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn cfg() -> Config {
        Config::default()
    }

    fn req() -> ChatRequest {
        let mut r = ChatRequest::new("hi");
        r.cwd = PathBuf::from("C:/work");
        r
    }

    fn args_as_strings(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|a| a.to_string_lossy().to_string())
            .collect()
    }

    #[test]
    fn argv_matches_the_verified_contract() {
        let a = args_as_strings(&build_args(&cfg(), &req()).unwrap());
        assert_eq!(a[0], "--print");
        assert_eq!(a[1], "hi", "prompt immediately follows --print");
        for required in [
            "--dangerously-skip-permissions",
            "--disable-slash-commands",
            "--output-format",
            "stream-json",
            "--print-timeout",
            "--add-dir",
            "C:/work",
        ] {
            assert!(
                a.contains(&required.to_string()),
                "missing {required} in {a:?}"
            );
        }
        assert!(
            !a.iter().any(|x| x == "--model"),
            "no model means no --model"
        );
    }

    #[test]
    fn print_timeout_is_always_passed_with_a_seconds_suffix() {
        let a = args_as_strings(&build_args(&cfg(), &req()).unwrap());
        let i = a.iter().position(|x| x == "--print-timeout").unwrap();
        assert_eq!(a[i + 1], "0s", "0 means wait for completion by default");

        let mut c = cfg();
        c.agy.print_timeout_secs = 45;
        let a = args_as_strings(&build_args(&c, &req()).unwrap());
        let i = a.iter().position(|x| x == "--print-timeout").unwrap();
        assert_eq!(a[i + 1], "45s");
    }

    #[test]
    fn a_model_always_gets_an_effort() {
        let mut r = req();
        r.model = Some("gemini-3.8-flash".into());
        let a = args_as_strings(&build_args(&cfg(), &r).unwrap());
        let m = a.iter().position(|x| x == "--model").unwrap();
        assert_eq!(a[m + 1], "gemini-3.8-flash");
        let e = a.iter().position(|x| x == "--effort").unwrap();
        assert_eq!(a[e + 1], "medium", "config default_effort is injected");
    }

    #[test]
    fn explicit_effort_beats_the_default() {
        let mut r = req();
        r.model = Some("gemini-3.1-pro".into());
        r.effort = Some("high".into());
        let a = args_as_strings(&build_args(&cfg(), &r).unwrap());
        let e = a.iter().position(|x| x == "--effort").unwrap();
        assert_eq!(a[e + 1], "high");
    }

    #[test]
    fn model_without_any_effort_is_a_config_error_not_a_broken_command() {
        let mut c = cfg();
        c.agy.default_effort = None;
        let mut r = req();
        r.model = Some("gemini-3.8-flash".into());

        let err = build_args(&c, &r).unwrap_err();
        assert!(matches!(err, PieError::Config(_)));
        assert_eq!(err.exit_code(), 2);
        assert!(
            err.to_string().contains("effort"),
            "message explains the fix"
        );
    }

    #[test]
    fn usage_argv_does_not_disable_slash_commands() {
        let a = args_as_strings(&usage_args());
        assert!(a.contains(&"/usage".to_string()));
        assert!(
            !a.contains(&"--disable-slash-commands".to_string()),
            "/usage needs slash expansion — this asymmetry is deliberate"
        );
        assert!(a.contains(&"--print".to_string()));
    }

    #[test]
    fn resume_uses_conversation_flag() {
        let mut r = req();
        r.resume = Some("conv-123".into());
        let a = args_as_strings(&build_args(&cfg(), &r).unwrap());
        let i = a.iter().position(|x| x == "--conversation").unwrap();
        assert_eq!(a[i + 1], "conv-123");
    }

    #[test]
    fn oversized_prompt_is_refused_before_spawning() {
        let mut r = req();
        r.prompt = "p".repeat(30_000);
        let err = build_plan(&cfg(), &r).unwrap_err();
        assert!(matches!(err, PieError::PromptTooLong { .. }));
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn plan_carries_the_scrub_list_and_deadline() {
        let plan = build_plan(&cfg(), &req()).unwrap();
        assert!(plan.scrub.contains(&"OPENAI_API_KEY".to_string()));
        assert_eq!(plan.timeout.as_secs(), 600);

        let mut r = req();
        r.timeout = Some(std::time::Duration::from_secs(3));
        assert_eq!(build_plan(&cfg(), &r).unwrap().timeout.as_secs(), 3);
    }

    // ---- M1: stream parsing -----------------------------------------------

    fn json(text: &str) -> Value {
        serde_json::from_str(text).unwrap()
    }

    #[test]
    fn tool_states_cover_the_documented_vocabulary_and_degrade_to_active() {
        assert_eq!(tool_state(Some("ACTIVE")), ToolState::Active);
        assert_eq!(tool_state(Some("DONE")), ToolState::Done);
        assert_eq!(tool_state(Some("ERROR")), ToolState::Error);
        assert_eq!(tool_state(Some(" done ")), ToolState::Done);
        // A state from a newer agy must not abort the run; "still running" is
        // the safe reading because tool progress is cosmetic either way.
        assert_eq!(tool_state(Some("TELEPORTED")), ToolState::Active);
        assert_eq!(tool_state(None), ToolState::Active);
    }

    #[test]
    fn only_success_and_ok_count_as_successes() {
        assert!(is_success_status("SUCCESS"));
        assert!(is_success_status("ok"));
        assert!(is_success_status(" Success "));
        for bad in ["ERROR", "FAILURE", "CANCELLED", "TIMEOUT", ""] {
            assert!(!is_success_status(bad), "{bad} must not be a success");
        }
    }

    #[test]
    fn usage_uses_agy_field_names_not_codex_ones() {
        let agy_shape = json(
            r#"{"input_tokens":20704,"output_tokens":20,"thinking_tokens":19,"cache_read_tokens":7,"total_tokens":20724}"#,
        );
        let usage = usage_from(Some(&agy_shape)).unwrap();
        assert_eq!(usage.input_tokens, 20704);
        assert_eq!(usage.output_tokens, 20);
        assert_eq!(
            usage.thinking_tokens, 19,
            "thinking_tokens → thinking_tokens"
        );
        assert_eq!(
            usage.cache_read_tokens, 7,
            "cache_read_tokens → cache_read_tokens"
        );
        assert_eq!(usage.total_tokens, 20724);

        // The codex vocabulary names *different* fields; reading it here would
        // report wrong numbers rather than none.
        let codex_shape =
            json(r#"{"input_tokens":10,"cached_input_tokens":5,"reasoning_output_tokens":3}"#);
        let usage = usage_from(Some(&codex_shape)).unwrap();
        assert_eq!(usage.cache_read_tokens, 0);
        assert_eq!(usage.thinking_tokens, 0);

        // Nothing recognised is not a usage payload, so previously accumulated
        // usage is not wiped out.
        assert!(usage_from(Some(&json(r#"{"other":1}"#))).is_none());
        assert!(usage_from(None).is_none());
    }

    #[test]
    fn the_router_accumulates_text_and_usage_across_step_updates() {
        let mut router = AgyRouter::new();
        router.on_line(
            r#"{"event":"step_update","step_update":{"step_type":"agent_response","text_delta":"OK\n","usage":{"input_tokens":20704,"output_tokens":20,"thinking_tokens":19,"cache_read_tokens":0,"total_tokens":20724}}}"#,
        );
        assert_eq!(router.text(), "OK\n");
        assert!(!router.saw_terminal());
        assert_eq!(router.usage().thinking_tokens, 19);
        assert_eq!(router.usage().input_tokens, 20704);
    }

    #[test]
    fn a_tool_step_survives_a_damaged_payload() {
        let mut router = AgyRouter::new();
        let event = router
            .on_line(
                r#"{"event":"step_update","step_update":{"step_type":"tool","state":17,"tool_name":null,"tool_info":{"cmd":"ls"}}}"#,
            )
            .unwrap();

        match event {
            StreamEvent::ToolStep { name, state, info } => {
                assert_eq!(name, UNNAMED_TOOL);
                assert_eq!(state, ToolState::Active);
                assert!(info.unwrap_or_default().contains("ls"));
            }
            other => panic!("wrong event: {other:?}"),
        }
    }

    #[test]
    fn models_parse_groups_rows_by_base_id_and_skips_the_banner() {
        let lines = vec![
            "Fetching available models...".to_string(),
            String::new(),
            "gemini-3.8-flash-high\tGemini 3.8 Flash (High)".to_string(),
            "gemini-3.8-flash-medium\tGemini 3.8 Flash (Medium)".to_string(),
            "gemini-3.8-flash-low\tGemini 3.8 Flash (Low)".to_string(),
            "gpt-oss-120b-medium\tGPT-OSS 120B (Medium)".to_string(),
            "not a model row".to_string(),
        ];

        let models = parse_models(&lines);
        assert_eq!(models.len(), 2, "two base ids, not five rows: {models:?}");

        let flash = &models[0];
        assert_eq!(flash.id, "gemini-3.8-flash", "the effort is not in the id");
        assert_eq!(flash.display, "Gemini 3.8 Flash", "nor in the display");
        assert_eq!(
            flash.efforts,
            vec!["low", "medium", "high"],
            "canonical cheapest-first order, however the CLI ordered its rows"
        );
        assert_eq!(flash.default_effort, None, "`list_models` fills this in");

        let oss = &models[1];
        assert_eq!(oss.id, "gpt-oss-120b");
        assert_eq!(
            oss.efforts,
            vec!["medium"],
            "a single-effort model stays so"
        );
    }

    #[test]
    fn a_non_uniform_catalog_keeps_each_models_own_set() {
        // The shape of the real capture: `gemini-3.1-pro` has no medium and
        // `gpt-oss-120b` only medium. Grouping must not widen one model's set to
        // the union of all of them.
        let lines = vec![
            "gemini-3.1-pro-high\tGemini 3.1 Pro (High)".to_string(),
            "gemini-3.1-pro-low\tGemini 3.1 Pro (Low)".to_string(),
            "gpt-oss-120b-medium\tGPT-OSS 120B (Medium)".to_string(),
        ];

        let models = parse_models(&lines);
        assert_eq!(models[0].efforts, vec!["low", "high"]);
        assert_eq!(models[1].efforts, vec!["medium"]);
    }

    #[test]
    fn models_parse_prefers_the_display_parenthetical_over_the_id_suffix() {
        // When the two encodings disagree the display wins, because that is what
        // the CLI labels the row with. Only vocabulary words count as efforts,
        // so a parenthetical such as `(Preview)` stays in the display name.
        let lines = vec![
            "gemini-3.9-flash-high\tGemini 3.9 Flash (Low)".to_string(),
            "gemini-4-pro\tGemini 4 Pro (Preview)".to_string(),
        ];

        let models = parse_models(&lines);
        assert_eq!(models[0].id, "gemini-3.9-flash");
        assert_eq!(models[0].efforts, vec!["low"], "the display's effort wins");
        assert_eq!(models[1].id, "gemini-4-pro");
        assert_eq!(models[1].display, "Gemini 4 Pro (Preview)");
        assert!(models[1].efforts.is_empty());
    }

    #[test]
    fn models_parse_accepts_space_aligned_rows_and_rejects_junk() {
        let lines = vec![
            "gemini-3.1-pro-low    Gemini 3.1 Pro (Low)".to_string(),
            "fetching available models".to_string(),
            "gemini-3.1-pro-low\tGemini 3.1 Pro (Low)".to_string(),
            "   ".to_string(),
        ];

        let models = parse_models(&lines);
        assert_eq!(models.len(), 1, "one base id: {models:?}");
        assert_eq!(models[0].id, "gemini-3.1-pro");
        assert_eq!(models[0].efforts, vec!["low"], "a repeat is not duplicated");
        assert_eq!(models[0].display, "Gemini 3.1 Pro");
    }

    #[test]
    fn usage_parse_maps_a_bucket_payload_by_shape() {
        // Synthetic, and labelled as such: the live `/usage` capture on this
        // machine returned no quota payload at all (see
        // `tests/fixtures/agy_usage.json`), so the bucket shape is exercised
        // from the documented contract (5h + weekly, remaining fraction, reset).
        let lines = vec![
            r#"{"groups":[{"label":"5h","remaining_fraction":0.87,"reset_at":"2026-05-01T00:00:00Z","used":13,"limit":100},{"name":"weekly","remaining":42.5,"resets_at":"2026-05-07T00:00:00Z"}]}"#
                .to_string(),
        ];

        let report = parse_usage(&lines);
        assert_eq!(report.backend, "agy");
        assert_eq!(report.buckets.len(), 2, "got {:?}", report.buckets);

        let five_hour = &report.buckets[0];
        assert_eq!(five_hour.label, "5h");
        assert_eq!(five_hour.remaining_fraction, Some(0.87));
        assert_eq!(five_hour.used, Some(13));
        assert_eq!(five_hour.limit, Some(100));
        assert_eq!(five_hour.reset_at.as_deref(), Some("2026-05-01T00:00:00Z"));

        let weekly = &report.buckets[1];
        assert_eq!(weekly.label, "weekly");
        assert_eq!(weekly.remaining_fraction, None, "a count is not a share");
        assert_eq!(weekly.reset_at.as_deref(), Some("2026-05-07T00:00:00Z"));
    }

    #[test]
    fn usage_parse_derives_a_fraction_from_used_and_limit() {
        let lines = vec![r#"{"buckets":[{"label":"weekly","used":25,"limit":100}]}"#.to_string()];
        let report = parse_usage(&lines);
        assert_eq!(report.buckets.len(), 1);
        assert_eq!(report.buckets[0].remaining_fraction, Some(0.75));
    }

    #[test]
    fn usage_parse_reads_a_percentage_and_a_json_response_string() {
        let inner = r#"{"buckets":[{"label":"5h","remaining_percent":25}]}"#;
        let line = json(&format!(
            r#"{{"status":"SUCCESS","response":{}}}"#,
            serde_json::to_string(inner).unwrap()
        ))
        .to_string();

        let report = parse_usage(&[line]);
        assert_eq!(report.buckets.len(), 1, "got {:?}", report.buckets);
        assert_eq!(report.buckets[0].remaining_fraction, Some(0.25));
    }

    #[test]
    fn usage_parse_turns_a_payload_with_no_quota_groups_into_a_specific_note() {
        // A genuinely payload-less envelope: no `command` key at all. A free
        // probe that returns `num_turns: 0` and all-zero `usage` is expected, so
        // the zeros must not be what triggers the note.
        let lines = vec![
            r#"{"conversation_id":"c","status":"SUCCESS","response":"","duration_seconds":0,"num_turns":0,"usage":{"input_tokens":0,"output_tokens":0,"thinking_tokens":0,"cache_read_tokens":0,"total_tokens":0}}"#
                .to_string(),
        ];

        let report = parse_usage(&lines);
        assert!(report.buckets.is_empty());
        assert_eq!(report.source, "agy --print /usage");

        // The wording matters: an empty table must not read as "zero quota".
        let note = report.notes.join(" ");
        assert!(
            note.contains("no quota buckets"),
            "notes: {:?}",
            report.notes
        );
        assert!(
            note.contains("MISSING, not zero"),
            "notes: {:?}",
            report.notes
        );
        assert!(
            note.contains("command.data"),
            "the note names the absent payload, not a CLI limitation: {:?}",
            report.notes
        );
    }

    #[test]
    fn usage_parse_prefers_the_verified_group_path_over_the_shape_fallback() {
        // The same buckets expressed both ways: a flat `buckets` array the shape
        // scan would also find, and the verified `command.data.groups` path. The
        // verified path must win — only it knows which group a bucket is in, so a
        // shape-only map would render two indistinguishable `weekly` rows.
        let lines = vec![
            r#"{"status":"SUCCESS","buckets":[{"label":"drifted","remaining_fraction":0.1}],"command":{"name":"usage","data":{"groups":[{"name":"Gemini Models","buckets":[{"id":"gemini-weekly","name":"Weekly Limit Remaining","window":"weekly","remaining_fraction":0.5,"reset_time":"2026-10-15T01:32:32Z"}]},{"name":"Claude and GPT models","buckets":[{"id":"3p-5h","name":"Five Hour Limit Remaining","window":"5h","remaining_fraction":1}]}]}}}"#
                .to_string(),
        ];

        let report = parse_usage(&lines);
        assert_eq!(report.buckets.len(), 2, "got {:?}", report.buckets);
        assert_eq!(report.buckets[0].label, "Gemini Models / weekly");
        assert_eq!(report.buckets[0].remaining_fraction, Some(0.5));
        assert_eq!(
            report.buckets[0].reset_at.as_deref(),
            Some("2026-10-15T01:32:32Z")
        );
        assert_eq!(report.buckets[1].label, "Claude and GPT models / 5h");
        assert_eq!(
            report.buckets[1].reset_at, None,
            "an absent reset_time is not an error"
        );
    }

    #[test]
    fn usage_parse_flags_a_failure_envelope() {
        let lines =
            vec![r#"{"status":"ERROR","error":"unknown command","response":""}"#.to_string()];
        let report = parse_usage(&lines);
        assert!(report.buckets.is_empty());
        assert!(report.notes.iter().any(|n| n.contains("status `ERROR`")));
        assert!(report.notes.iter().any(|n| n.contains("unknown command")));
    }

    #[test]
    fn a_capture_that_is_not_json_is_one_note() {
        let report = parse_usage(&["Exception in thread main".to_string()]);
        assert!(report.buckets.is_empty());
        assert!(report.notes.iter().any(|n| n.contains("no JSON payload")));
    }

    // ---- M1: the catalog is the source of truth for effort -----------------

    fn model(id: &str, efforts: &[&str]) -> Model {
        Model {
            id: id.to_string(),
            display: id.to_string(),
            efforts: efforts.iter().map(|e| (*e).to_string()).collect(),
            default_effort: None,
        }
    }

    /// Seed a catalog into a scratch `PIE_HOME` so the validation path is tested
    /// against a known effort set, never against whatever is on this machine.
    fn seed_catalog(models: Vec<Model>) -> (tempfile::TempDir, crate::testing::EnvGuard) {
        let dir = tempfile::tempdir().unwrap();
        let env =
            crate::testing::EnvGuard::set(&[("PIE_HOME", Some(dir.path().to_str().unwrap()))]);
        cache::write_now(&paths::agy_model_cache(), &models).unwrap();
        (dir, env)
    }

    #[test]
    fn closest_effort_prefers_the_exact_match_then_the_nearest() {
        let pro = model("gemini-3.1-pro", &["low", "high"]);
        let oss = model("gpt-oss-120b", &["medium"]);

        assert_eq!(
            closest_effort("high", &pro.efforts).as_deref(),
            Some("high")
        );
        assert_eq!(
            closest_effort(" HIGH ", &pro.efforts).as_deref(),
            Some("high")
        );
        // A tie goes to the cheaper effort: substituting upward would spend more
        // of the subscription than the user asked for.
        assert_eq!(
            closest_effort("medium", &pro.efforts).as_deref(),
            Some("low")
        );
        assert_eq!(closest_effort("max", &pro.efforts).as_deref(), Some("high"));
        assert_eq!(
            closest_effort("medium", &oss.efforts).as_deref(),
            Some("medium")
        );
        assert_eq!(closest_effort("xhigh", &[]).as_deref(), None);
    }

    #[test]
    fn only_documented_words_count_as_efforts() {
        assert!(is_effort_word("High"));
        assert!(is_effort_word(" xhigh "));
        assert!(!is_effort_word("preview"));
        assert!(!is_effort_word("120b"));
        assert_eq!(effort_rank("low"), 0);
        assert!(
            effort_rank("ultra") > effort_rank("max"),
            "an unknown effort ranks after every known one"
        );
    }

    #[test]
    fn an_explicit_effort_the_model_does_not_offer_is_a_config_error() {
        let (_dir, _env) = seed_catalog(vec![
            model("gemini-3.1-pro", &["low", "high"]),
            model("gpt-oss-120b", &["medium"]),
        ]);

        let mut r = req();
        r.model = Some("gemini-3.1-pro".to_string());
        r.effort = Some("medium".to_string());

        let err = build_args(&cfg(), &r).unwrap_err();
        match &err {
            PieError::Config(message) => {
                assert!(message.contains("gemini-3.1-pro"), "{message}");
                assert!(message.contains("`medium`"), "{message}");
                assert!(
                    message.contains("low|high"),
                    "the error names this model's valid set: {message}"
                );
            }
            other => panic!("wrong error: {other:?}"),
        }
        assert_eq!(err.exit_code(), 2, "a usage/config problem");

        // …while an effort the model does offer is accepted.
        r.effort = Some("high".to_string());
        let a = args_as_strings(&build_args(&cfg(), &r).unwrap());
        let e = a.iter().position(|x| x == "--effort").unwrap();
        assert_eq!(a[e + 1], "high");
    }

    #[test]
    fn a_configured_default_the_model_lacks_is_replaced_by_the_closest() {
        let (_dir, _env) = seed_catalog(vec![
            model("gemini-3.1-pro", &["low", "high"]),
            model("gpt-oss-120b", &["medium"]),
        ]);

        // `default_effort` is `medium`, which `gemini-3.1-pro` does not offer.
        let mut r = req();
        r.model = Some("gemini-3.1-pro".to_string());
        let a = args_as_strings(&build_args(&cfg(), &r).unwrap());
        let e = a.iter().position(|x| x == "--effort").unwrap();
        assert_eq!(a[e + 1], "low", "a tie between low and high picks low");

        // A model whose only effort is medium keeps it.
        r.model = Some("gpt-oss-120b".to_string());
        let a = args_as_strings(&build_args(&cfg(), &r).unwrap());
        let e = a.iter().position(|x| x == "--effort").unwrap();
        assert_eq!(a[e + 1], "medium");
    }

    #[test]
    fn an_unknown_model_falls_back_to_the_documented_vocabulary() {
        let (_dir, _env) = seed_catalog(vec![model("gemini-3.1-pro", &["low", "high"])]);

        let mut r = req();
        r.model = Some("gemini-9-ultra".to_string());
        r.effort = Some("xhigh".to_string());
        assert!(
            build_args(&cfg(), &r).is_ok(),
            "a model agy added since the cache was written still gets a valid effort"
        );

        r.effort = Some("ultra".to_string());
        let err = build_args(&cfg(), &r).unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert!(
            err.to_string().contains("low|medium|high|xhigh|max"),
            "the documented vocabulary is the fallback: {err}"
        );
    }

    #[test]
    fn without_a_catalog_at_all_the_documented_default_is_used() {
        let dir = tempfile::tempdir().unwrap();
        let _env =
            crate::testing::EnvGuard::set(&[("PIE_HOME", Some(dir.path().to_str().unwrap()))]);

        let mut r = req();
        r.model = Some("gemini-3.8-flash".to_string());
        let a = args_as_strings(&build_args(&cfg(), &r).unwrap());
        let e = a.iter().position(|x| x == "--effort").unwrap();
        assert_eq!(a[e + 1], "medium");
    }
}
