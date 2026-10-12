//! OpenAI Codex (`codex`) backend — delegate mode.
//!
//! `codex exec` is one-shot, so pie runs the whole task and returns the final
//! answer; there is no tool round-trip (`RISKS.md` §1).
//!
//! Verified contract: `knowledge/codex.md`. Everything this module parses was
//! either recorded from a live `codex exec --json` run or read out of the
//! shipped binary — never invented. When a shape is unverified the code is
//! deliberately permissive instead of assuming.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::Value;

use crate::auth::{self, Credential};
use crate::cache;
use crate::config::{CodexQuotaProbe, Config};
use crate::error::{truncate, PieError, Result};
use crate::jsonl::{self, LineRead};
use crate::paths;
use crate::process;
use crate::provider::driver::{cap_for, drive, enforce_argv_cap, EventRouter, TurnPlan};
#[cfg(feature = "http")]
use crate::provider::Bucket;
use crate::provider::{
    AuthState, Capabilities, ChatRequest, Model, Provider, StreamEvent, ToolState, TurnOutcome,
    Usage, UsageReport,
};

/// Path of the npm launcher script, relative to an npm root.
pub const NODE_SCRIPT_RELATIVE: &str = "node_modules/@openai/codex/bin/codex.js";

/// Hint shown when no subscription credential is available.
pub const NO_SUBSCRIPTION_HINT: &str =
    "run `codex login` to use your ChatGPT/Codex plan; pie will not run codex in API-key mode";

/// ChatGPT subscription quota endpoint (`knowledge/codex.md`).
///
/// A **private** vendor endpoint, not a documented API: it is read-only and
/// authenticated with the user's own credential from `~/.codex/auth.json`, and
/// it never spends model quota. Because it is private it is switchable off —
/// see [`crate::config::CodexQuotaProbe`].
pub const USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";

/// codex's own model-cache file inside `~/.codex`
/// (`knowledge/internals.md`: `{fetched_at, models:[{slug, display_name,
/// default_reasoning_level, supported_reasoning_levels:[{effort}]}]}`).
pub const MODELS_CACHE_FILE: &str = "models_cache.json";

/// Status string recorded for a healthy terminal event.
pub const TURN_COMPLETED: &str = "completed";

/// Status string recorded for a failed terminal event.
pub const TURN_FAILED: &str = "failed";

/// Deadline for the quota probe. It is a metadata call, not a turn.
#[cfg(feature = "http")]
const QUOTA_TIMEOUT_SECS: u64 = 30;

/// The reasoning-effort vocabulary codex documents
/// (`knowledge/internals.md`: `low|medium|high|xhigh|max|ultra`).
pub const DEFAULT_EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

/// Item types that are tool activity rather than answer text.
///
/// Extracted from the shipped binary's own string table (`grep -a` over
/// `codex.exe`; zero-token probe) — `command_execution`, `file_change`,
/// `mcp_tool_call`, `web_search`, `local_shell_call`, `function_call`,
/// `patch_apply`, `todo_list`, `plan_update` all appear there. The suffix rules
/// catch a type codex adds later without silently treating it as an answer.
const TOOL_ITEM_TYPES: [&str; 10] = [
    "command_execution",
    "local_shell_call",
    "function_call",
    "tool_call",
    "patch_apply",
    "file_change",
    "mcp_tool_call",
    "web_search",
    "todo_list",
    "plan_update",
];

/// The codex backend.
#[derive(Debug, Default, Clone, Copy)]
pub struct Codex;

#[async_trait]
impl Provider for Codex {
    fn id(&self) -> &'static str {
        "codex"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            // `codex exec` is one-shot (RISKS §1).
            tool_loop: false,
            resume: true,
            // No prompt arg reads stdin; `-` also reads stdin.
            stdin_prompt: true,
            // 0 = no inherent argv limit. The prompt still goes on argv, so the
            // config cap applies; a stdin path for oversized prompts is
            // deferred (the driver spawns children with `stdin(null)`).
            max_prompt_argv: 0,
            // `exec` has no verified system-prompt or instructions flag, and
            // writing an `AGENTS.md` into the user's own workspace is rejected
            // as invasive, so the persona is folded into the prompt (DESIGN §10).
            system_prompt: false,
        }
    }

    fn binary(&self, cfg: &Config) -> Option<PathBuf> {
        program_and_prefix(cfg).map(|(program, _)| program)
    }

    /// codex is launched as `node <codex.js>` on this machine, so the version
    /// probe must carry the script prefix or it reports node's version.
    fn version_command(&self, cfg: &Config) -> Option<(PathBuf, Vec<OsString>)> {
        let (program, mut prefix) = program_and_prefix(cfg)?;
        prefix.push(OsString::from("--version"));
        Some((program, prefix))
    }

    /// Same launcher prefix as the version probe, because the reason is the same: codex
    /// is `node <codex.js>` on Windows, and the `codex.cmd` shim cannot be spawned
    /// without a shell. Without this override M8's drift check would run `--help` against
    /// the wrong program, or fail to spawn at all, and report it as a change.
    fn help_command(&self, cfg: &Config) -> Option<(PathBuf, Vec<OsString>)> {
        let (program, mut prefix) = program_and_prefix(cfg)?;
        prefix.push(OsString::from("--help"));
        Some((program, prefix))
    }

    async fn auth_status(&self, cfg: &Config) -> AuthState {
        if self.binary(cfg).is_none() {
            return AuthState::BinaryMissing {
                path: cfg.codex.bin.clone(),
            };
        }
        match auth::codex_credential() {
            Credential::Subscription { .. } => AuthState::Ready,
            Credential::ApiKeyOnly { evidence } => AuthState::Error(format!(
                "codex credential is API-key only ({evidence}) — {NO_SUBSCRIPTION_HINT}"
            )),
            Credential::Absent => AuthState::NeedsLogin {
                hint: "run `codex login`".to_string(),
            },
            Credential::Unreadable(why) => AuthState::Error(why),
        }
    }

    /// The catalog comes from codex's own `~/.codex/models_cache.json`, so this
    /// spawns nothing and never spends model quota (I7).
    async fn list_models(&self, cfg: &Config) -> Result<Vec<Model>> {
        Ok(model_catalog(cfg))
    }

    async fn stream(
        &self,
        cfg: &Config,
        req: &ChatRequest,
        sink: &mut (dyn FnMut(StreamEvent) + Send),
    ) -> Result<TurnOutcome> {
        // Subscription-only gate (I9): refuse before spawning anything, so an
        // API key can never be billed by accident. Checked before argv is even
        // built, so the refusal does not depend on resolution succeeding.
        require_subscription()?;

        // codex has no `--effort` flag (`codex exec --help`, verified): effort
        // is a `-c` config override, which M2 does not send. Saying so is
        // better than silently running at a different depth than requested.
        if req.effort.is_some() {
            tracing::warn!(
                "--effort is ignored for codex: it has no `--effort` flag and pie sends the \
                 model only"
            );
        }

        let plan = build_plan(cfg, req)?;
        let mut router = CodexRouter::new();
        let started = Instant::now();
        drive(plan, &mut router, sink, started).await
    }

    async fn usage(&self, cfg: &Config) -> Result<UsageReport> {
        // Checked before anything else, so `off` provably touches neither the
        // credential store nor the network.
        if cfg.codex.quota_probe == CodexQuotaProbe::Off {
            return Ok(quota_disabled_report());
        }
        probe_quota(cfg).await
    }
}

/// Resolve how to actually start codex: `(program, leading_args)`.
///
/// The Windows `codex.cmd` shim **cannot be spawned without a shell**, so it is
/// never used (`knowledge/codex.md`). Preference order:
///
/// 1. an explicit path from config (a `.js` path is run through `node`),
/// 2. a native `codex.exe` / `codex` on `PATH`,
/// 3. `node <npm-root>/node_modules/@openai/codex/bin/codex.js`.
pub fn program_and_prefix(cfg: &Config) -> Option<(PathBuf, Vec<OsString>)> {
    let configured = PathBuf::from(&cfg.codex.bin);

    // 1. Explicit / path-like configuration.
    if configured.is_file() {
        if is_javascript(&configured) {
            let node = process::resolve_binary("node")?;
            return Some((node, vec![configured.into_os_string()]));
        }
        return Some((configured, Vec::new()));
    }

    // 2. A real executable on PATH — but never the `.cmd`/`.bat` shim.
    if let Some(found) = resolve_executable(&cfg.codex.bin) {
        return Some((found, Vec::new()));
    }

    // 3. node + the npm launcher, which is what the shim would have run.
    if cfg.codex.use_node_shim {
        if let (Some(script), Some(node)) = (find_node_script(cfg), process::resolve_binary("node"))
        {
            return Some((node, vec![script.into_os_string()]));
        }
    }

    None
}

fn is_javascript(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("js") | Some("mjs") | Some("cjs")
    )
}

/// Resolve a binary on `PATH`, skipping Windows shell shims.
fn resolve_executable(spec: &str) -> Option<PathBuf> {
    let found = process::resolve_binary(spec)?;
    if is_shell_shim(&found) {
        return None;
    }
    Some(found)
}

/// A `.cmd`/`.bat`/`.ps1` script is not directly spawnable.
fn is_shell_shim(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("cmd") | Some("bat") | Some("ps1")
    )
}

/// Locate the npm-installed `codex.js`, if node/npm put it somewhere standard.
fn find_node_script(cfg: &Config) -> Option<PathBuf> {
    for root in npm_root_candidates(cfg) {
        let candidate = root.join(NODE_SCRIPT_RELATIVE);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Places an npm global install may live, per platform.
fn npm_root_candidates(cfg: &Config) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();

    // Next to the configured binary, if that resolves to a shim.
    if let Some(shim) = process::resolve_binary(&cfg.codex.bin) {
        if let Some(dir) = shim.parent() {
            roots.push(dir.to_path_buf());
            // `%APPDATA%\npm\codex.cmd` → `%APPDATA%\npm\node_modules\...`
            if let Some(grandparent) = dir.parent() {
                roots.push(grandparent.to_path_buf());
            }
        }
    }

    if let Some(home) = crate::paths::home_dir() {
        // npm's default per-user global root, and common alternatives.
        roots.push(home.join("node_modules"));
        roots.push(home.join(".npm-global").join("lib"));
        roots.push(home.join(".local").join("lib"));
        // nvm / fnm / volta style layouts.
        roots.push(home.join(".volta").join("tools").join("image"));
        roots.push(home.join(".local").join("share").join("fnm"));
    }

    if let Some(appdata) = std::env::var_os("APPDATA") {
        roots.push(PathBuf::from(appdata).join("npm"));
    }

    // System-wide installs (Linux, WSL, macOS).
    roots.push(PathBuf::from("/usr/lib"));
    roots.push(PathBuf::from("/usr/local/lib"));
    roots.push(PathBuf::from("/opt/homebrew/lib"));

    roots.push(PathBuf::from(NODE_SCRIPT_RELATIVE));

    let _ = cfg;
    roots
}

/// Build the argv for one turn. Pure and unit-tested.
///
/// Verified shape (`knowledge/codex.md`):
/// ```text
/// codex exec --cd <CWD> --approve-for-me --skip-git-repo-check --json
///     [--model <ID>] <PROMPT>
/// ```
pub fn build_args(cfg: &Config, req: &ChatRequest) -> Result<Vec<OsString>> {
    let _ = cfg;
    let mut args: Vec<OsString> = Vec::with_capacity(12);

    args.push(OsString::from("exec"));

    args.push(OsString::from("--cd"));
    args.push(req.cwd.clone().into_os_string());
    // Auto-approve within the workspace-write sandbox. Mutually exclusive with
    // `--sandbox`, so that flag must never be added alongside.
    args.push(OsString::from("--approve-for-me"));
    // pie runs in arbitrary workspaces, including non-repos.
    args.push(OsString::from("--skip-git-repo-check"));
    args.push(OsString::from("--json"));

    if let Some(model) = &req.model {
        args.push(OsString::from("--model"));
        args.push(OsString::from(model));
    }

    // `resume <id> <prompt>` is a **subcommand** of `exec`, so every option
    // above must already be in place: the subcommand's own parser rejects
    // `--cd` and `--approve-for-me`
    // (`codex exec resume --help`, and M0's order failed empirically with
    // `error: unexpected argument '--cd' found`).
    if let Some(id) = &req.resume {
        args.push(OsString::from("resume"));
        args.push(OsString::from(id));
    }

    // No `--effort`: codex takes reasoning effort through `-c` overrides, not a
    // dedicated flag. M2 sends only the model; the catalog still reports the
    // effort axis so a later milestone can wire it up.
    //
    // The prompt carries the profile projection folded in front of the task
    // text, through the same rule agy uses, so the two payloads are the
    // identical bytes (M3's Done condition, DESIGN §10).
    args.push(OsString::from(req.folded_prompt()));
    Ok(args)
}

/// Build a runnable plan.
pub fn build_plan(cfg: &Config, req: &ChatRequest) -> Result<TurnPlan> {
    let (program, prefix) = program_and_prefix(cfg).ok_or_else(|| PieError::BinaryMissing {
        backend: "codex".into(),
        path: cfg.codex.bin.clone(),
    })?;

    let mut args = prefix;
    args.extend(build_args(cfg, req)?);

    // The prompt goes on argv, so the config cap applies (codex can read stdin,
    // but the shared driver spawns children with a null stdin).
    let cap = cap_for(&Codex.capabilities(), cfg);
    enforce_argv_cap(
        "codex",
        &program,
        &args,
        cap,
        if req.has_persona() {
            crate::provider::PROJECTION_CAP_HINT
        } else {
            "shorten the prompt or split the task"
        },
    )?;

    Ok(TurnPlan {
        program,
        args,
        cwd: req.cwd.clone(),
        scrub: cfg.scrub_keys(cfg.scrub_enabled()),
        timeout: req.timeout.unwrap_or_else(|| cfg.timeout()),
    })
}

// ---------------------------------------------------------------------------
// Auth gate
// ---------------------------------------------------------------------------

/// Refuse to run codex without a **subscription** credential (I9).
///
/// An API-key-only store must never reach a spawn: `codex exec` with
/// `OPENAI_API_KEY` present would silently bill pay-as-you-go
/// (`knowledge/internals.md` "Critical do-nots"). Only the presence of a
/// credential is reported; no token value is read, kept, or rendered.
pub fn require_subscription() -> Result<()> {
    match auth::codex_credential() {
        Credential::Subscription { .. } => Ok(()),
        Credential::ApiKeyOnly { evidence } => Err(PieError::NoSubscription {
            backend: "codex".into(),
            hint: format!("{NO_SUBSCRIPTION_HINT} (saw {evidence})"),
        }),
        Credential::Absent => Err(PieError::AuthRequired {
            backend: "codex".into(),
            hint: "run `codex login`".to_string(),
        }),
        Credential::Unreadable(why) => Err(PieError::AuthRequired {
            backend: "codex".into(),
            hint: format!("cannot classify the codex credential: {why}"),
        }),
    }
}

// ---------------------------------------------------------------------------
// JSONL → StreamEvent
// ---------------------------------------------------------------------------

/// A fatal event, stored as plain strings.
///
/// Kept separate from [`PieError`] because `PieError::Spawn` holds an
/// `io::Error` and the router must stay cheap to hold across the turn.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Failure {
    status: String,
    detail: String,
}

/// Interprets one codex turn's JSONL stream.
///
/// ```jsonl
/// {"type":"thread.started","thread_id":"…"}
/// {"type":"turn.started"}
/// {"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"OK"}}
/// {"type":"turn.completed","usage":{"input_tokens":13768,"cached_input_tokens":11008,
///   "cache_write_input_tokens":0,"output_tokens":5,"reasoning_output_tokens":0}}
/// ```
///
/// Rules that matter (`DESIGN.md` §6 rule 4, invariant I5): a line we cannot
/// parse and an event we do not model are both forwarded, never fatal.
#[derive(Debug, Default)]
pub struct CodexRouter {
    /// Thread id from `thread.started`, for a later resume.
    thread_id: Option<String>,
    /// Answer text accumulated from agent items.
    text: String,
    /// The answer frozen at `turn.completed`.
    ///
    /// Kept separately from `text` because `text` keeps accumulating: any agent
    /// item arriving *after* the terminal event would otherwise be appended into
    /// the returned answer.
    final_text: Option<String>,
    /// Usage from `turn.completed`.
    usage: Usage,
    /// Whether a healthy terminal event was seen.
    finished: bool,
    /// Last fatal event, when the stream reported one.
    failure: Option<Failure>,
    /// How many lines parsed as JSON — used in the "no terminal event" message.
    events: usize,
}

impl CodexRouter {
    /// A router for one fresh turn.
    pub fn new() -> Self {
        Self::default()
    }

    /// Answer text accumulated so far.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Interpret an `item.*` event.
    ///
    /// Only a **completed agent message** becomes [`StreamEvent::TextDelta`].
    /// Reasoning, tool calls, and anything else become a tool step or a
    /// forwarded `Other` — a tool's output must never be mistaken for the
    /// answer.
    fn on_item(&mut self, event: &Value, is_completion: bool, fallback: ToolState) -> StreamEvent {
        let item_type = jsonl::s_nonempty(event, "item.type").unwrap_or_else(|| "unknown".into());

        if is_completion && is_answer_item(&item_type) {
            let text = item_text(event).unwrap_or_default();
            self.text.push_str(&text);
            return StreamEvent::TextDelta(text);
        }

        if is_tool_item(&item_type) {
            return StreamEvent::ToolStep {
                name: jsonl::s_nonempty(event, "item.name").unwrap_or_else(|| item_type.clone()),
                state: tool_state(event, fallback),
                info: tool_info(event),
            };
        }

        StreamEvent::Other {
            kind: item_type,
            raw: event.clone(),
        }
    }
}

impl EventRouter for CodexRouter {
    fn backend(&self) -> &'static str {
        "codex"
    }

    fn on_line(&mut self, line: &str) -> Option<StreamEvent> {
        let value = match jsonl::parse_line(line) {
            LineRead::Blank => return None,
            LineRead::Unparsed(raw) => return Some(StreamEvent::Unparsed(raw)),
            LineRead::Value(value) => value,
        };

        self.events += 1;
        let kind = jsonl::s_nonempty(&value, "type").unwrap_or_else(|| "unknown".to_string());

        match kind.as_str() {
            "thread.started" => {
                let thread = jsonl::s_nonempty(&value, "thread_id");
                self.thread_id = thread.clone();
                Some(StreamEvent::Init {
                    conversation_id: thread,
                    cwd: jsonl::s_nonempty(&value, "cwd").map(PathBuf::from),
                    tools: jsonl::strings(&value, "tools"),
                })
            }
            // No user-visible state: the turn is already implied by the answer.
            "turn.started" => None,
            "item.started" | "item.updated" => Some(self.on_item(&value, false, ToolState::Active)),
            "item.completed" => Some(self.on_item(&value, true, ToolState::Done)),
            "turn.completed" => {
                // First terminal wins. A duplicated or trailing `turn.completed`
                // must not be able to change the answer.
                if self.finished {
                    return None;
                }
                self.usage = usage_from_event(&value);
                // Freeze the answer here. Reading the live accumulator in
                // `outcome` would let any `item.completed` after this line be
                // appended into the answer.
                self.final_text = Some(self.text.clone());
                self.finished = true;
                // A healthy terminal event wins over an earlier transient error.
                self.failure = None;
                Some(StreamEvent::Done {
                    text: self.final_text.clone().unwrap_or_default(),
                    status: TURN_COMPLETED.to_string(),
                })
            }
            "turn.failed" => {
                // A failure after a healthy terminal event must not discard a
                // delivered answer: codex emits `turn.failed` *instead of*
                // `turn.completed`, so both appearing means a contradictory
                // stream, and the plausible cause is a close-after-delivery. The
                // anomaly is surfaced rather than swallowed.
                if self.finished {
                    tracing::warn!(
                        backend = "codex",
                        "ignoring `turn.failed` after a healthy `turn.completed`; the answer is \
                         kept"
                    );
                    return None;
                }
                self.failure = Some(failure_from(&value, TURN_FAILED));
                Some(StreamEvent::Other {
                    kind: kind.clone(),
                    raw: value,
                })
            }
            "error" => {
                if self.finished {
                    tracing::warn!(
                        backend = "codex",
                        "ignoring an `error` event after a healthy `turn.completed`"
                    );
                    return None;
                }
                self.failure = Some(failure_from(&value, "error"));
                Some(StreamEvent::Other {
                    kind: kind.clone(),
                    raw: value,
                })
            }
            // Protocol drift must not be fatal (I5): forward and continue.
            other => Some(StreamEvent::Other {
                kind: other.to_string(),
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
        // A failure the stream reported wins over everything else.
        if let Some(failure) = &self.failure {
            return Err(PieError::Backend {
                backend: "codex".into(),
                status: failure.status.clone(),
                detail: failure.detail.clone(),
            });
        }

        // Success is decided by the terminal event, not by the exit status
        // (`DESIGN.md` §6 rule 7) — a chatty stderr or a non-zero exit after a
        // healthy `turn.completed` is not an error.
        if self.finished {
            let text = self.final_text.clone().unwrap_or_default();
            // A terminal event with no answer at all means the stream did not
            // honour its contract — a renamed item type, or a turn that only ran
            // tools. Returning `ok: true` with empty text would log a successful
            // turn that produced nothing, and `pie -p` would print a blank line
            // as though that were the answer.
            if text.trim().is_empty() {
                return Err(PieError::protocol(
                    "codex",
                    "`turn.completed` arrived but no agent message was captured",
                ));
            }
            return Ok(TurnOutcome {
                text,
                usage: self.usage.clone(),
                status: TURN_COMPLETED.to_string(),
                conversation_id: self.thread_id.clone(),
                duration: Duration::ZERO,
            });
        }

        if exit_code != Some(0) {
            return Err(PieError::Backend {
                backend: "codex".into(),
                status: exit_status.to_string(),
                detail: process::diagnostic(stderr_tail),
            });
        }

        // Exit 0 with no terminal event: the stream did not honour its own
        // contract, so we cannot claim the turn succeeded.
        Err(PieError::protocol(
            "codex",
            format!(
                "stream closed after {} event(s) without a `turn.completed` (exit 0). {}",
                self.events,
                process::diagnostic(stderr_tail)
            ),
        ))
    }
}

/// Item types that carry the answer.
fn is_answer_item(item_type: &str) -> bool {
    matches!(item_type, "agent_message" | "assistant_message")
}

/// Item types that represent tool activity rather than answer text.
fn is_tool_item(item_type: &str) -> bool {
    TOOL_ITEM_TYPES.contains(&item_type)
        || item_type.ends_with("_call")
        || item_type.ends_with("_execution")
        || item_type.ends_with("_change")
}

/// Text of an agent item.
///
/// Verified shapes carry `item.text`; codex also models an item as content
/// blocks (the on-disk rollout records `item.content[0].text`), so both are
/// accepted rather than dropping an answer.
fn item_text(event: &Value) -> Option<String> {
    if let Some(text) = jsonl::s(event, "item.text") {
        return Some(text);
    }
    let blocks: Vec<String> = jsonl::objects(event, "item.content")
        .iter()
        .filter_map(|block| jsonl::s(block, "text"))
        .collect();
    if blocks.is_empty() {
        None
    } else {
        Some(blocks.join(""))
    }
}

/// Lifecycle state of a tool item.
///
/// `item.status` is authoritative when present; `item.exit_code` is the next
/// best evidence; otherwise the event kind decides (`item.completed` ⇒ `Done`,
/// `item.started`/`item.updated` ⇒ `Active`).
fn tool_state(event: &Value, fallback: ToolState) -> ToolState {
    // Checked **before** the status string: an item can report
    // `status: "completed"` while its command exited non-zero, and the exit code
    // is the truth about whether the step worked. Previously the status won, so a
    // failed command rendered as a completed tool step.
    if matches!(jsonl::u(event, "item.exit_code"), Some(code) if code != 0) {
        return ToolState::Error;
    }

    if let Some(status) = jsonl::s_nonempty(event, "item.status") {
        let status = status.to_ascii_lowercase();
        if status.contains("fail") || status.contains("error") || status.contains("cancel") {
            return ToolState::Error;
        }
        if status.contains("progress") || status.contains("running") || status.contains("pending") {
            return ToolState::Active;
        }
        if status.contains("complete") || status.contains("success") || status.contains("done") {
            return ToolState::Done;
        }
    }

    fallback
}

/// Short, redacted description of what a tool item did.
fn tool_info(event: &Value) -> Option<String> {
    for path in [
        "item.command",
        "item.query",
        "item.path",
        "item.description",
    ] {
        if let Some(text) = jsonl::s_nonempty(event, path) {
            return Some(truncate(&process::redact(&text), 160));
        }
    }
    None
}

/// Usage carried by `turn.completed`.
///
/// The renames are the point: codex's `cached_input_tokens` is pie's
/// `cache_read_tokens`, and `reasoning_output_tokens` is `thinking_tokens`.
pub fn usage_from_event(event: &Value) -> Usage {
    Usage {
        input_tokens: jsonl::u(event, "usage.input_tokens").unwrap_or(0),
        output_tokens: jsonl::u(event, "usage.output_tokens").unwrap_or(0),
        thinking_tokens: jsonl::u(event, "usage.reasoning_output_tokens").unwrap_or(0),
        cache_read_tokens: jsonl::u(event, "usage.cached_input_tokens").unwrap_or(0),
        cache_write_tokens: jsonl::u(event, "usage.cache_write_input_tokens").unwrap_or(0),
        // codex reports no total; `Usage::computed_total` derives it.
        total_tokens: jsonl::u(event, "usage.total_tokens").unwrap_or(0),
    }
}

/// Build a [`Failure`] from `turn.failed` or `error`, redacted at the boundary.
fn failure_from(event: &Value, status: &str) -> Failure {
    let detail = jsonl::s_nonempty(event, "error.message")
        .or_else(|| jsonl::s_nonempty(event, "message"))
        .or_else(|| jsonl::s_nonempty(event, "error"))
        .unwrap_or_else(|| jsonl::brief(event, 300));

    Failure {
        status: status.to_string(),
        detail: truncate(&process::redact(&detail), 600),
    }
}

// ---------------------------------------------------------------------------
// Model catalog
// ---------------------------------------------------------------------------

/// Read the catalog for a backend. Never spawns a process and never spends
/// quota (I7): it is codex's own on-disk cache plus a local TTL cache.
///
/// A fresh [`crate::paths::codex_model_cache`] short-circuits the source file,
/// which is what makes `pie models` cheap on repeat calls.
pub fn model_catalog(cfg: &Config) -> Vec<Model> {
    let path = paths::codex_model_cache();
    let ttl = Duration::from_secs(cfg.codex.model_cache_ttl_secs);

    if let Some(models) =
        cache::read_fresh::<Vec<Model>>(&path, ttl, crate::session_log::now_millis())
    {
        if !models.is_empty() {
            return models;
        }
    }

    let Some(models) = read_source_catalog() else {
        // Fallback is deliberately **not** cached: it is a placeholder until
        // codex writes its own cache, and caching it would delay picking that
        // up by a whole TTL.
        return fallback_models();
    };

    // A cache write failing must not fail `pie models`.
    if let Err(e) = cache::write_now(&path, &models) {
        tracing::debug!(
            error = %e,
            path = %path.display(),
            "could not cache the codex model catalog"
        );
    }
    models
}

/// `~/.codex/models_cache.json`, shaped. Any read or parse problem is a miss.
fn read_source_catalog() -> Option<Vec<Model>> {
    let path = auth::codex_dir().join(MODELS_CACHE_FILE);
    let text = std::fs::read_to_string(&path).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    let models = shape_models(&value);
    if models.is_empty() {
        return None;
    }
    Some(models)
}

/// Map codex's `models_cache.json` into pie's [`Model`].
///
/// `slug` → id, `display_name` → display, `supported_reasoning_levels[].effort`
/// → efforts, `default_reasoning_level` → default_effort. Entries without a slug
/// are dropped; a missing display name falls back to the slug.
///
/// Internal entries (`memory4-dream`, `gpt-reserve`, `codex-auto-review`) are
/// listed as reported: the catalog is codex's own, and pie does not second-guess
/// which of them accept `--model`.
pub fn shape_models(value: &Value) -> Vec<Model> {
    let mut models = Vec::new();

    for entry in jsonl::objects(value, "models") {
        let Some(id) = jsonl::s_nonempty(&entry, "slug") else {
            continue;
        };
        let display = jsonl::s_nonempty(&entry, "display_name").unwrap_or_else(|| id.clone());

        let mut efforts: Vec<String> = Vec::new();
        for level in jsonl::objects(&entry, "supported_reasoning_levels") {
            if let Some(effort) = jsonl::s_nonempty(&level, "effort") {
                if !efforts.contains(&effort) {
                    efforts.push(effort);
                }
            }
        }
        // Tolerate a plain string array if the shape ever simplifies.
        for effort in jsonl::strings(&entry, "supported_reasoning_levels") {
            if !effort.trim().is_empty() && !efforts.contains(&effort) {
                efforts.push(effort);
            }
        }

        models.push(Model {
            id,
            display,
            efforts,
            default_effort: jsonl::s_nonempty(&entry, "default_reasoning_level"),
        });
    }

    models
}

/// Last-resort catalog, used only when `~/.codex/models_cache.json` is absent.
///
/// The ids and display names are the ones recorded in `knowledge/codex.md` /
/// `knowledge/internals.md` from this machine; the effort sets are the
/// documented common vocabulary (`DEFAULT_EFFORTS`), not per-model truth. This
/// list exists so `pie models` says something useful on a fresh machine — it is
/// never used to validate a `--model`, which codex itself does.
pub fn fallback_models() -> Vec<Model> {
    [
        ("gpt-6-luna", "GPT-6-Luna"),
        ("gpt-6-sol", "GPT-6-Sol"),
        ("gpt-5.6-luna", "GPT-5.6-Luna"),
        ("gpt-5.5", "GPT-5.5"),
    ]
    .iter()
    .map(|(id, display)| Model {
        id: (*id).to_string(),
        display: (*display).to_string(),
        efforts: DEFAULT_EFFORTS.iter().map(|e| (*e).to_string()).collect(),
        default_effort: Some("medium".to_string()),
    })
    .collect()
}

// ---------------------------------------------------------------------------
// Quota probe
// ---------------------------------------------------------------------------

/// The report returned when the probe is switched off.
///
/// Not an error: `pie usage codex` still prints the session-log totals, only the
/// bucket table is empty (`knowledge/9router-findings.md` §4 — the endpoint is
/// private, so it needs a switch that does not require a code change).
pub fn quota_disabled_report() -> UsageReport {
    UsageReport {
        backend: "codex".into(),
        buckets: Vec::new(),
        source: "disabled by config".into(),
        notes: vec![
            "codex quota_probe = \"off\" — set it to \"http\" to query the usage endpoint"
                .to_string(),
        ],
    }
}

/// Query the ChatGPT usage endpoint.
#[cfg(feature = "http")]
async fn probe_quota(cfg: &Config) -> Result<UsageReport> {
    let _ = cfg;
    let (token, account) = usage_credential()?;

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(QUOTA_TIMEOUT_SECS))
        .build()
        .map_err(|e| probe_error(&e.to_string()))?;

    let mut request = client
        .get(USAGE_URL)
        .header(reqwest::header::AUTHORIZATION, format!("Bearer {token}"))
        .header(reqwest::header::ACCEPT, "application/json");
    if let Some(account) = account {
        request = request.header("ChatGPT-Account-Id", account);
    }

    let response = request
        .send()
        .await
        .map_err(|e| probe_error(&e.to_string()))?;
    let status = response.status();
    // Read the body before branching so a failure can report what came back.
    let body = response.text().await.unwrap_or_default();

    if !status.is_success() {
        return Err(describe_http_failure(status.as_u16(), &body));
    }

    let value: Value = serde_json::from_str(&body).map_err(|e| {
        PieError::protocol("codex", format!("usage endpoint returned non-JSON: {e}"))
    })?;

    Ok(UsageReport {
        backend: "codex".into(),
        buckets: buckets_from_usage(&value),
        source: "chatgpt backend-api/wham/usage".to_string(),
        notes: quota_notes(&value),
    })
}

/// Without the `http` feature there is no TLS client to probe with.
#[cfg(not(feature = "http"))]
async fn probe_quota(_cfg: &Config) -> Result<UsageReport> {
    Err(PieError::Unsupported {
        backend: "codex".into(),
        feature: "quota probe (built without the `http` feature)".into(),
    })
}

/// Read the credential, requiring a subscription first (I9).
///
/// The token is returned only to be placed in a request header. It is never
/// logged, never rendered into an error, and never stored.
#[cfg(feature = "http")]
fn usage_credential() -> Result<(String, Option<String>)> {
    require_subscription()?;

    let path = auth::codex_auth_file();
    let text = std::fs::read_to_string(&path).map_err(|e| PieError::AuthRequired {
        backend: "codex".into(),
        hint: format!("could not read {}: {e}", path.display()),
    })?;
    let value: Value = serde_json::from_str(&text).map_err(|e| PieError::AuthRequired {
        backend: "codex".into(),
        hint: format!("{} is not valid JSON: {e}", path.display()),
    })?;

    let token =
        jsonl::s_nonempty(&value, "tokens.access_token").ok_or_else(|| PieError::AuthRequired {
            backend: "codex".into(),
            hint: "no access token in ~/.codex/auth.json — run `codex login`".to_string(),
        })?;

    Ok((token, jsonl::s_nonempty(&value, "tokens.account_id")))
}

/// A probe transport failure, with the error text redacted.
#[cfg(feature = "http")]
fn probe_error(detail: &str) -> PieError {
    PieError::Backend {
        backend: "codex".into(),
        status: "usage probe failed".into(),
        detail: truncate(&process::redact(detail), 600),
    }
}

/// Turn a non-2xx response into an error that names only the status and a
/// redacted body — never a header, and never the token.
#[cfg(feature = "http")]
pub fn describe_http_failure(code: u16, body: &str) -> PieError {
    let detail = format!("HTTP {code}: {}", process::diagnostic(body));

    if code == 401 || code == 403 {
        return PieError::AuthRequired {
            backend: "codex".into(),
            hint: format!("the codex credential was rejected — run `codex login` ({detail})"),
        };
    }

    PieError::Backend {
        backend: "codex".into(),
        status: format!("HTTP {code}"),
        detail,
    }
}

/// Map the usage payload's rate-limit windows into [`Bucket`]s.
///
/// Verified shape (`chatgpt.com/backend-api/wham/usage`): the windows live under
/// `rate_limit.primary_window` / `rate_limit.secondary_window`, each with
/// `used_percent`, `limit_window_seconds` and `reset_at` (epoch seconds). A window
/// this plan does not report is simply absent (only `primary_window` was present
/// on the development machine, as a 30-day window).
///
/// `limit_window_seconds` has a `window_minutes` fallback and `reset_at` has a
/// `resets_at` fallback, both inside the same window object, because those
/// spellings appear in codex's own on-disk rollout records.
///
/// Note: this reads **only** the `rate_limit.*_window` keys. An earlier version of
/// this comment claimed the rollout's `primary`/`secondary` top-level spelling was
/// also accepted; it never was, and a test asserted it produced nothing, so the
/// claim was removed rather than left to mislead.
#[cfg(feature = "http")]
pub fn buckets_from_usage(value: &Value) -> Vec<Bucket> {
    let mut buckets = Vec::new();

    for key in ["primary_window", "secondary_window"] {
        let Some(window) = jsonl::at(value, &format!("rate_limit.{key}")) else {
            continue;
        };
        if !window.is_object() {
            continue;
        }
        buckets.push(window_bucket(key, window));
    }

    buckets
}

/// One window → one bucket.
#[cfg(feature = "http")]
fn window_bucket(key: &str, window: &Value) -> Bucket {
    let seconds = jsonl::u(window, "limit_window_seconds")
        .or_else(|| jsonl::u(window, "window_minutes").map(|m| m.saturating_mul(60)));

    let remaining_fraction = jsonl::u(window, "used_percent")
        .map(|percent| (100u64.saturating_sub(percent.min(100)) as f64) / 100.0);

    Bucket {
        label: window_label(key, seconds),
        remaining_fraction,
        reset_at: reset_at_string(window),
        // The endpoint reports percentages, not unit counts.
        used: None,
        limit: None,
    }
}

/// `reset_at` in whichever form the endpoint used.
///
/// `jsonl::u` alone silently dropped the timestamp when the field arrived as an
/// ISO-8601 string (which is how `knowledge/codex.md` documents it), rendering the
/// row with no reset time and looking like a parser bug. Epoch seconds and an
/// already-formatted timestamp are both accepted.
#[cfg(feature = "http")]
fn reset_at_string(window: &Value) -> Option<String> {
    for key in ["reset_at", "resets_at"] {
        if let Some(seconds) = jsonl::u(window, key) {
            return Some(iso8601_utc(seconds));
        }
        // Not a number: pass a timestamp through unchanged rather than dropping it.
        if let Some(text) = jsonl::s_nonempty(window, key) {
            return Some(text);
        }
    }
    None
}

/// Name a window after its real duration, so a plan that reports a 30-day
/// window is not mislabelled `5h`.
#[cfg(feature = "http")]
fn window_label(key: &str, seconds: Option<u64>) -> String {
    match seconds {
        Some(18_000) => "5h".to_string(),
        Some(604_800) => "weekly".to_string(),
        Some(2_592_000) => "30d".to_string(),
        Some(s) if s >= 3_600 && s % 3_600 == 0 => format!("{}h", s / 3_600),
        Some(s) if s >= 60 => format!("{}m", s / 60),
        Some(s) => format!("{s}s"),
        None => key.trim_end_matches("_window").to_string(),
    }
}

/// Caveats worth showing next to the buckets.
#[cfg(feature = "http")]
pub fn quota_notes(value: &Value) -> Vec<String> {
    let mut notes = Vec::new();

    if jsonl::b(value, "rate_limit.limit_reached") == Some(true) {
        notes.push("the endpoint reports the rate limit is reached".to_string());
    }
    if jsonl::b(value, "rate_limit.allowed") == Some(false) {
        notes.push("the endpoint reports this plan may not make requests right now".to_string());
    }
    let no_credits = jsonl::b(value, "credits.has_credits") == Some(false)
        && jsonl::b(value, "credits.unlimited") == Some(false);
    if no_credits {
        notes.push("no usage credits on this plan — the windows above are the quota".to_string());
    }
    if let Some(count) = jsonl::u(value, "rate_limit_reset_credits.available_count") {
        if count > 0 {
            notes.push(format!("{count} rate-limit reset credit(s) available"));
        }
    }
    if let Some(extra) = jsonl::at(value, "additional_rate_limits") {
        if extra.as_array().is_some_and(|items| !items.is_empty()) {
            notes.push(
                "the endpoint reported additional per-scope rate limits that pie does not map"
                    .to_string(),
            );
        }
    }

    notes
}

/// Render epoch seconds as an RFC 3339 UTC timestamp.
///
/// Hand-rolled because pie has no date library: the quota table needs a readable
/// reset time and one formatting function is cheaper than a dependency.
#[cfg(feature = "http")]
pub fn iso8601_utc(epoch_secs: u64) -> String {
    let secs = epoch_secs as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);

    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3_600,
        (rem % 3_600) / 60,
        rem % 60
    )
}

/// Days since the Unix epoch → civil date (Howard Hinnant's algorithm).
#[cfg(feature = "http")]
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let year = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]

    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn req() -> ChatRequest {
        let mut r = ChatRequest::new("hi");
        r.cwd = PathBuf::from("C:/work");
        r
    }

    fn strings(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|a| a.to_string_lossy().to_string())
            .collect()
    }

    #[test]
    fn argv_matches_the_verified_contract() {
        let a = strings(&build_args(&Config::default(), &req()).unwrap());
        assert_eq!(a[0], "exec");
        assert_eq!(a[1], "--cd");
        assert_eq!(a[2], "C:/work");
        assert!(a.contains(&"--approve-for-me".to_string()));
        assert!(a.contains(&"--skip-git-repo-check".to_string()));
        assert!(a.contains(&"--json".to_string()));
        assert_eq!(a.last().unwrap(), "hi", "prompt is last");
    }

    #[test]
    fn sandbox_is_never_combined_with_approve_for_me() {
        // They are mutually exclusive in codex.
        let a = strings(&build_args(&Config::default(), &req()).unwrap());
        assert!(!a.contains(&"--sandbox".to_string()));
        assert!(a.contains(&"--approve-for-me".to_string()));
    }

    #[test]
    fn model_is_passed_only_when_chosen() {
        let mut r = req();
        let a = strings(&build_args(&Config::default(), &r).unwrap());
        assert!(!a.contains(&"--model".to_string()));

        r.model = Some("gpt-6-luna".into());
        let a = strings(&build_args(&Config::default(), &r).unwrap());
        let i = a.iter().position(|x| x == "--model").unwrap();
        assert_eq!(a[i + 1], "gpt-6-luna");
    }

    #[test]
    fn resume_uses_the_resume_subcommand() {
        let mut r = req();
        r.resume = Some("thread-9".into());
        let a = strings(&build_args(&Config::default(), &r).unwrap());
        let at = a.iter().position(|x| x == "resume").unwrap();
        assert_eq!(a[0], "exec");
        assert_eq!(a[at + 1], "thread-9");
        assert_eq!(a.last().unwrap(), "hi", "the prompt follows the session id");
    }

    /// M3: codex has no system-prompt flag, so the projection is folded into
    /// the prompt — and the prompt is still the last argument, after even the
    /// `resume` subcommand.
    #[test]
    fn a_folded_persona_precedes_the_prompt_and_stays_last() {
        let persona = "# Persona\ntone: terse";
        let mut r = req();
        r.persona = Some(persona.to_string());
        let a = strings(&build_args(&Config::default(), &r).unwrap());
        assert_eq!(
            a.last().unwrap(),
            &format!("{persona}{}hi", crate::profile::TASK_SEPARATOR)
        );
        assert!(!a.contains(&"--append-system-prompt".to_string()));

        // Still last when the turn resumes a thread.
        r.resume = Some("thread-9".into());
        let a = strings(&build_args(&Config::default(), &r).unwrap());
        assert!(a.last().unwrap().starts_with(persona));
        assert_eq!(a[a.len() - 2], "thread-9");
    }

    /// Regression: `resume` is a **subcommand** of `exec`, so an option written
    /// after it is parsed by `resume`, which accepts neither `--cd` nor
    /// `--approve-for-me`. M0 emitted `exec resume <id> --cd …` and codex
    /// rejected it:
    ///
    /// ```text
    /// $ codex exec resume <id> --cd . --approve-for-me --skip-git-repo-check --json --help
    /// error: unexpected argument '--cd' found
    /// ```
    ///
    /// Every `exec`-level option must precede the subcommand.
    #[test]
    fn every_exec_option_precedes_the_resume_subcommand() {
        let mut r = req();
        r.resume = Some("thread-9".into());
        let a = strings(&build_args(&Config::default(), &r).unwrap());

        let resume_at = a.iter().position(|x| x == "resume").unwrap();
        for flag in [
            "--cd",
            "--approve-for-me",
            "--skip-git-repo-check",
            "--json",
        ] {
            let at = a
                .iter()
                .position(|x| x == flag)
                .unwrap_or_else(|| panic!("missing {flag} in {a:?}"));
            assert!(
                at < resume_at,
                "{flag} must precede `resume` in {a:?}, or codex rejects the argv"
            );
        }
        assert!(
            !a[resume_at..].contains(&"--cd".to_string()),
            "nothing exec-level may follow the subcommand: {a:?}"
        );
    }

    #[test]
    fn capabilities_declare_stdin_and_no_tool_loop() {
        let caps = Codex.capabilities();
        assert!(caps.stdin_prompt, "codex reads stdin when given no prompt");
        assert!(!caps.tool_loop, "exec is one-shot");
        assert_eq!(caps.max_prompt_argv, 0, "no inherent argv limit");
    }

    #[test]
    fn shell_shims_are_recognised_and_refused() {
        assert!(is_shell_shim(Path::new(
            r"C:\Users\x\AppData\Roaming\npm\codex.cmd"
        )));
        assert!(is_shell_shim(Path::new("codex.bat")));
        assert!(is_shell_shim(Path::new("codex.ps1")));
        assert!(!is_shell_shim(Path::new("codex.exe")));
        assert!(!is_shell_shim(Path::new("codex")));
    }

    #[test]
    fn javascript_paths_are_recognised() {
        assert!(is_javascript(Path::new("codex.js")));
        assert!(is_javascript(Path::new("codex.MJS")));
        assert!(!is_javascript(Path::new("codex.exe")));
        assert!(!is_javascript(Path::new("codex")));
    }

    #[test]
    fn node_root_candidates_include_the_relative_script_and_system_paths() {
        let roots = npm_root_candidates(&Config::default());
        assert!(roots.contains(&PathBuf::from(NODE_SCRIPT_RELATIVE)));
        assert!(roots
            .iter()
            .any(|r| r.to_string_lossy().contains("node_modules")));
    }

    #[test]
    fn oversized_prompt_is_refused_before_spawning() {
        let mut r = req();
        r.prompt = "p".repeat(30_000);
        let err = build_plan(&Config::default(), &r).unwrap_err();
        assert!(matches!(err, PieError::PromptTooLong { .. }));
    }

    #[test]
    fn missing_binary_reports_binary_missing() {
        let mut cfg = Config::default();
        cfg.codex.bin = "definitely-not-a-real-codex-binary-xyz".into();
        cfg.codex.use_node_shim = false;
        let err = build_plan(&cfg, &req()).unwrap_err();
        assert!(matches!(err, PieError::BinaryMissing { .. }));
        assert_eq!(err.exit_code(), 6);
    }

    /// Regression: the version probe must include the launcher prefix, or
    /// `doctor` records the launcher's version instead of codex's.
    #[test]
    fn version_probe_carries_the_node_script_prefix() {
        use crate::provider::Provider;

        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("codex.js");
        std::fs::write(&script, "// stub").unwrap();

        let mut cfg = Config::default();
        cfg.codex.bin = script.to_string_lossy().to_string();

        // Only assert when node is actually resolvable; otherwise the probe
        // legitimately yields None and there is nothing to check.
        if let Some((program, args)) = Codex.version_command(&cfg) {
            assert!(is_javascript(&program) || program.to_string_lossy().contains("node"));
            let a = strings(&args);
            assert_eq!(a.last().unwrap(), "--version");
            assert!(
                a.iter().any(|x| x.ends_with("codex.js")),
                "the script must precede --version: {a:?}"
            );
        }
    }

    /// The same requirement for M8's drift probe.
    ///
    /// `CliSnapshot::help` is what drift diffs, so a help probe that ran `--help`
    /// against the launcher would compare node's help text between runs and report the
    /// result as a change to codex. The override exists for exactly this, and nothing
    /// else would notice if it were dropped.
    #[test]
    fn the_help_probe_carries_the_node_script_prefix_too() {
        use crate::provider::Provider;

        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("codex.js");
        std::fs::write(&script, "// stub").unwrap();

        let mut cfg = Config::default();
        cfg.codex.bin = script.to_string_lossy().to_string();

        if let Some((program, args)) = Codex.help_command(&cfg) {
            assert!(is_javascript(&program) || program.to_string_lossy().contains("node"));
            let a = strings(&args);
            assert_eq!(a.last().unwrap(), "--help");
            assert!(
                a.iter().any(|x| x.ends_with("codex.js")),
                "the script must precede --help, or drift diffs the launcher: {a:?}"
            );
        }

        // And the default really is `--help`, so a backend that overrides nothing still
        // gets a working probe rather than an empty argv.
        assert_eq!(Codex.help_args(), &["--help"]);
    }
}
