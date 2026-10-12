# pie — Design (implementation contract)

Companion to `PLAN.md` (what/why) and `ROADMAP.md` (order). This file is the
**frozen contract** the implementation is written against: file map, types, trait,
errors, config, process/streaming rules, backend argv, on-disk state, tests.

**Scope of this design: M0–M2 only.** Non-goals here (owned by later milestones,
do not build early): TUI, agent tool-loop, memory/profile projection, scheduling,
watchtower, OCR/MCP, HTTP backends.

## 1. Platform facts

| Fact | Value |
|---|---|
| Target | `x86_64-pc-windows-msvc` (rustc 1.95.0) |
| Shell used for dev | MSYS/git-bash (`uname` → `MINGW64_NT-10.0-26200`) |
| Host OS | Windows 10.0.26200 |
| Subprocess visibility | must be **hidden** (`CREATE_NO_WINDOW` = `0x0800_0000`) |
| argv limit | Windows `CreateProcess` ~32 767 chars → pie caps prompt argv at 20 000 |
| Process tree kill | `taskkill /PID <pid> /T /F` |

Enable `#[cfg(windows)]` paths with portable fallbacks so the crate still
compiles/tests on Linux (the roadmap only requires Windows to work).

> `addendum` in the agent harness claims "OS: CachyOS (Arch Linux)". That is
> **wrong** for this machine — live evidence (`rustc -vV`, `uname`, `C:\Users\…`,
> `AppData\Local\agy\bin\agy.exe`) is Windows. Design for Windows; keep the code
> portable but do not spend effort on Linux-only packaging.

## 2. Crate layout (locked)

The tree follows plain Rust conventions: `snake_case` file names, no
abbreviations, one concern per file, and a `commands/` directory holding one
file per subcommand so each is reviewable in isolation. There are deliberately
**no `mod.rs` files** — the modern form is a sibling `foo.rs` next to a `foo/`
directory. There are also no commented-out source files: deferred designs live
in documents (`DEFERRED-BACKENDS.md`), because a file of comments is not
compiled, not linted, and not tested.

```
pie/
├── Cargo.toml · Cargo.lock · CHANGELOG.md · README.md · .gitignore
├── planning/            # PLAN, ROADMAP, RISKS, VERSIONING, DESIGN, ACCEPTANCE,
│                        # PLATFORMS, DEFERRED-BACKENDS
├── knowledge/           # agy.md, codex.md, claude.md, internals.md,
│                        # token_optimization_and_agent_engineering.md
├── scripts/             # qc.sh (unix/WSL) · qc.ps1 (Windows) — run gates G1–G3
├── src/
│   ├── main.rs          # ~45 lines: parse → resolve config → dispatch → exit code
│   ├── lib.rs           # module list + VERSION/NAME
│   ├── cli.rs           # clap definitions only, no behaviour
│   ├── commands.rs      # dispatch table; declares the command modules
│   ├── commands/
│   │   ├── backends.rs  # `--list-backends` and `doctor`
│   │   ├── auth.rs      # `auth`
│   │   ├── models.rs    # `models`
│   │   ├── usage.rs     # `usage`
│   │   └── print.rs     # `-p/--print` delegate turn
│   ├── config.rs        # ~/.pie/config.toml + env + flag overrides, env scrub list
│   ├── error.rs         # PieError + exit-code mapping + `kind()`
│   ├── paths.rs         # PIE_HOME, ~/.pie layout, cache paths
│   ├── logging.rs       # tracing init (stderr only — stdout is the protocol)
│   ├── process.rs       # hidden spawn, bounded stderr, deadline, kill-tree, redaction
│   ├── jsonl.rs         # tolerant line → Value reader + dotted-path helpers
│   ├── cache.rs         # TTL'd JSON caches under ~/.pie/cache
│   ├── auth.rs          # credential classification per backend
│   ├── session_log.rs   # append/read ~/.pie/usage.jsonl (frozen schema §7)
│   ├── report.rs        # quota/Bucket rendering + session-log Totals
│   └── provider/
│       ├── (provider.rs)      # Provider trait, Capabilities, registry, types
│       ├── driver.rs          # shared turn driver: EventRouter + process run
│       ├── agy.rs             # M1 (live)
│       ├── codex.rs           # M2 (live)
│       └── claude.rs          # stub now, real in M9 (future-proof, disabled)
└── tests/
    ├── process.rs       # real child-process tests: lines, timeout, kill, scrub
    ├── no_secrets.rs    # static + behavioural no-hardcoded-key guards
    ├── fixtures/        # recorded NDJSON/JSONL samples (M1/M2)
    ├── parse_agy.rs · parse_codex.rs          (M1/M2)
    └── live_smoke.rs    # #[ignore] by default, requires PIE_LIVE=1
```

Note the two deliberately distinct names: `cli.rs` is *argument parsing*,
`provider/driver.rs` is *running a provider subprocess*. `provider.rs` sits
beside `provider/` rather than inside it as `mod.rs`.

`PLAN.md` §2 sketches the **full target tree**, including modules for later
milestones (`agent/`, `watch/`, `tui/`). Where it and this file disagree about
M0–M2, this file wins.

Rule: `main.rs` contains no business logic; each module owns one concern.

## 3. Core types (`provider.rs`)

```rust
pub struct Model {
    pub id: String,            // "gemini-3.8-flash", "gpt-6-luna"
    pub display: String,       // "Gemini 3.8 Flash"
    pub efforts: Vec<String>,  // [] when the backend has no effort axis
    pub default_effort: Option<String>,
}

pub struct Capabilities {
    pub tool_loop: bool,       // true only for claude SDK / HTTP backends
    pub resume: bool,
    pub stdin_prompt: bool,    // agy = false (argv only)
    pub max_prompt_argv: usize,// 20_000 for agy, 0 = unlimited
}

pub struct ChatRequest {
    pub prompt: String,
    pub model: Option<String>,  // bare id, backend already selected
    pub effort: Option<String>,
    pub cwd: PathBuf,
    pub resume: Option<String>,
    pub timeout: Option<Duration>,
}

#[derive(Default, Clone, PartialEq)]
pub struct Usage {               // all fields optional in practice → 0 default
    pub input_tokens: u64, pub output_tokens: u64, pub thinking_tokens: u64,
    pub cache_read_tokens: u64, pub cache_write_tokens: u64, pub total_tokens: u64,
}

pub enum ToolState { Active, Done, Error }

pub enum StreamEvent {
    Init { conversation_id: Option<String>, cwd: Option<PathBuf>, tools: Vec<String> },
    TextDelta(String),
    ToolStep { name: String, state: ToolState, info: Option<String> },
    Usage(Usage),
    Done { text: String, status: String },
    /// Unknown/new event kinds are FORWARDED, never fatal (RISKS §3).
    Other { kind: String, raw: serde_json::Value },
    /// stdout line that is not JSON at all (banner noise, warnings).
    Unparsed(String),
}

pub struct TurnOutcome {
    pub text: String,
    pub usage: Usage,
    pub status: String,
    pub conversation_id: Option<String>,
    pub duration: Duration,
}

pub enum AuthState {
    Ready,
    NeedsLogin { hint: String },
    NoSubscription,             // CLI installed, but no plan on this machine
    BinaryMissing { path: String },
    Error(String),
}

pub struct Bucket { pub label: String, pub remaining_fraction: Option<f64>,
                    pub reset_at: Option<String>, pub used: Option<u64>, pub limit: Option<u64> }
pub struct UsageReport { pub backend: String, pub buckets: Vec<Bucket>, pub source: String, pub notes: Vec<String> }
```

## 4. Provider trait (`provider.rs`)

Object-safe via `#[async_trait]` (per `PLAN.md` §3). Note: native `async fn` in
traits is **not** dyn-compatible, so `async-trait` is required for `Box<dyn Provider>`.

```rust
#[async_trait]
pub trait Provider: Send + Sync {
    fn id(&self) -> &'static str;                      // "agy" | "codex" | "claude"
    fn capabilities(&self) -> Capabilities;
    fn binary(&self, cfg: &Config) -> Option<PathBuf>;  // resolved path or None
    /// argv used for the drift-check version snapshot (RISKS §3)
    fn version_args(&self) -> &'static [&'static str] { &["--version"] }

    async fn auth_status(&self, cfg: &Config) -> AuthState;
    async fn list_models(&self, cfg: &Config) -> Result<Vec<Model>>;
    async fn stream(
        &self, cfg: &Config, req: &ChatRequest,
        sink: &mut (dyn FnMut(StreamEvent) + Send),
    ) -> Result<TurnOutcome>;
    async fn usage(&self, cfg: &Config) -> Result<UsageReport>;
}

pub fn registry() -> Vec<Box<dyn Provider>>;           // order: agy, codex, claude
pub fn by_id(id: &str) -> Option<Box<dyn Provider>>;
pub fn parse_model_spec(s: &str) -> Result<(String, String)>; // "agy/x" → ("agy","x")
```

`stream()` **must** invoke `sink` as events arrive (streaming, not buffered) and
**must** return `TurnOutcome` built from the terminal event. A non-zero child exit
with no terminal event is an error carrying bounded stderr (§6).

## 5. Error taxonomy + exit codes (`error.rs`)

```rust
#[derive(thiserror::Error, Debug)]
pub enum PieError {
    #[error("config: {0}")]                       Config(String),
    #[error("unknown backend `{backend}`")]        UnknownBackend { backend: String, known: Vec<String> },
    #[error("`{backend}` binary not found at {path}")] BinaryMissing { backend: String, path: String },
    #[error("`{backend}` needs login: {hint}")]    AuthRequired { backend: String, hint: String },
    #[error("`{backend}` has no subscription credential — refusing API-key mode")] NoSubscription { backend: String },
    #[error("failed to spawn `{backend}`: {source}")] Spawn { backend: String, source: std::io::Error },
    #[error("`{backend}` timed out after {secs}s")]  Timeout { backend: String, secs: u64 },
    #[error("`{backend}` protocol error: {detail}")] Protocol { backend: String, detail: String },
    #[error("prompt too long for `{backend}`: {len} chars > {max}")] PromptTooLong { backend: String, len: usize, max: usize },
    #[error("`{backend}` failed ({status}): {detail}")] Backend { backend: String, status: String, detail: String },
    #[error("path not reachable: {0}")]             NotReachable(String),
    #[error(transparent)]                           Io(#[from] std::io::Error),
    #[error(transparent)]                           Json(#[from] serde_json::Error),
    #[error(transparent)]                           Http(#[from] reqwest::Error),
}
```

Exit codes: `0` ok · `1` generic/backend · `2` usage+config · `3` auth ·
`4` protocol · `5` timeout · `6` binary missing. `main.rs` maps via
`PieError::exit_code()`.

**Never panic on external input.** No `unwrap()`/`expect()` outside tests;
parsing external JSON returns `Result` or degrades to `StreamEvent::Unparsed`/`Other`.

## 6. Process + streaming contract (`process.rs`, `provider/driver.rs`)

Non-negotiable for all three CLIs:

1. **Hidden spawn.** `CREATE_NO_WINDOW` on Windows. No console flash, ever.
2. **Clean env.** Child env = parent env minus the API-key deny-list (§8).
   Never set an API key for a sub-locked backend.
3. **stdout = protocol, stderr = diagnostics.** `tracing` writes to stderr only.
   stderr is drained concurrently and retained bounded (last 8 KiB) for errors.
4. **Tolerant line parse.** For each stdout line: skip blank → try
   `serde_json::from_str::<Value>` → on failure emit `Unparsed`, log at debug,
   **continue**. Unknown `event`/`type` → `Other { kind, raw }`. A parse problem
   must never abort a run that could otherwise succeed (RISKS §3).
5. **Deadline + kill-tree.** Whole-turn deadline (default from config, per-call
   override). On expiry: kill the process **tree** (Windows
   `taskkill /PID <pid> /T /F`), then return `PieError::Timeout`. Also
   `kill_on_drop(true)` so an early return cannot leak a child.
6. **Bounded prompt argv.** If `capabilities().max_prompt_argv > 0` and the built
   argv would exceed it → `PieError::PromptTooLong` (word the hint: agy has no
   stdin prompt path, RISKS §5).
7. **Exit status is not the answer.** Success is decided by the terminal event.
   Non-zero exit + no terminal event ⇒ `Backend { status, detail: last stderr }`.

## 7. On-disk state (`paths.rs`, `session_log.rs`)

`PIE_HOME` overrides the root; default `~/.pie`.

```
~/.pie/
├── config.toml
├── cache/
│   ├── agy-models.json       # CacheEntry: { fetched_at, value:[…] }  TTL 24h
│   └── codex-models.json     # same shape, shaped from ~/.codex/models_cache.json
├── usage.jsonl               # FROZEN session log — one row per turn
├── reports/                  # M8
└── memory/                   # M3
```

### `usage.jsonl` row schema (FROZEN — matches the existing JS pie on this machine)

Adopt verbatim so M4's totals and the JS pi remain cross-readable:

```jsonc
{ "ts": 1776000000000,          // epoch millis
  "provider": "agy",            // "agy" | "codex" | "claude"
  "model": "gemini-3.8-flash",
  "input_tokens": 20704, "cached_input_tokens": 0, "output_tokens": 20,
  "reasoning_tokens": 19,       // agy thinking_tokens / codex reasoning_output_tokens
  "cost_usd_micros": 0, "credits_consumed": 0, "credits_remaining": 0,
  "duration_ms": 35900,
  "ok": true, "error_kind": null, "stop_reason": "SUCCESS" }
```

Append-only, one JSON object per line, flushed immediately, **never** rewritten.
Writing a row must never fail a turn (log and continue).

### `~/.pie/auth.json` — legacy, deliberately unused

The pre-existing JS pi left `~/.pie/auth.json` holding extracted codex
`access_token`/`refresh_token`/`client_id`/`client_secret`. **pie must not read,
write, or extend it** (`PLAN.md` §5, `RISKS.md` §10: token extraction is the
pattern that triggered the vendor suspension sweeps). The Rust pie derives auth
state from each CLI's *own* store (`~/.codex/auth.json`, `~/.gemini/…`,
`~/.claude/.credentials.json`). Recorded here so nobody "helpfully" reuses it.
M0–M2 do not delete the file — that is a user decision.

## 8. Config (`config.rs`)

`~/.pie/config.toml`, all keys optional:

```toml
[general]
default_model    = "agy/gemini-3.8-flash"
timeout_secs     = 600
prompt_argv_cap  = 20000
color            = "auto"          # auto | always | never

[agy]
bin               = "agy"
default_effort    = "medium"
print_timeout_secs = 0             # 0 = wait for completion

[codex]
bin      = "codex"
use_node_shim = true               # Windows: spawn node codex.js, not codex.cmd
quota_probe   = "off"              # http | off — see the note below

[claude]
enabled  = false                   # no subscription on this machine
```

**Precedence (highest wins):** CLI flag → `PIE_*` env → `config.toml` → built-in default.
Malformed TOML ⇒ `PieError::Config` with the parse location; never silently ignore.
A missing file is not an error (all defaults).

### API-key env deny-list (scrubbed from every child, `RISKS` §10)

`ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN`, `OPENAI_API_KEY`, `GOOGLE_API_KEY`.
Config-extendable (`general.extra_scrub_env`), and `PIE_STRIP_ENV=0` disables the
scrub **only** for debugging (prints a loud warning to stderr).

Also refuse to run a sub backend when the only credential present is an API key
(`NoSubscription`). `GOOGLE_APPLICATION_CREDENTIALS` **is** scrubbed: a GCP
service-account credential is one of the API-key paths
`knowledge/internals.md` says not to use, while agy's subscription login is its
own internal OAuth, so a service account in the environment is a billing switch
rather than a login. `CLOUD_CODE` is preserved because it is a mode selector, not
a credential. `PIE_STRIP_ENV=0` is the escape hatch for a setup that genuinely
needs the service-account path.

## 9. Backend argv contracts

### agy (M1, verified in `knowledge/agy.md`)

```
agy --print <PROMPT> --dangerously-skip-permissions --disable-slash-commands
    --output-format stream-json --print-timeout <N>s --add-dir <CWD>
    [--model <ID> --effort <E>]
```

- `--model` **requires** `--effort` → if a model is given without effort, inject
  `default_effort` (else `PieError::Config`). Effort set: `low|medium|high|xhigh|max`.
- No stdin path → `stdin_prompt = false`, `max_prompt_argv = 20000`.
- NDJSON: `init` → `step_update`(`step_type:"agent_response"` → `text_delta`,
  `step_type:"tool"` → tool step) → `result`(`status`,`response`,`usage`).
- `usage` mapping: `input_tokens`, `output_tokens`, `thinking_tokens`,
  `cache_read_tokens`, `total_tokens`.
- Status is `SUCCESS` on success; anything else (`ERROR|FAILURE|CANCELLED|TIMEOUT`)
  ⇒ `PieError::Backend` including the `error` field when present.
- Models: `agy models` → cache TTL 24h. `usage`: `agy --print /usage
  --output-format json --print-timeout 0` **without** `--disable-slash-commands`
  (it needs slash expansion — note this asymmetry; it is deliberate).

### codex (M2, verified in `knowledge/codex.md`)

```
codex exec --cd <CWD> --approve-for-me --skip-git-repo-check --json
    [--model <ID>] <PROMPT>
```

- Windows: never spawn `codex.cmd` — spawn `node <…>/@openai/codex/bin/codex.js`
  (or the native `codex.exe`). Resolve once, cache in memory.
- JSONL: `thread.started`(`thread_id`) → `turn.started` → `item.completed`
  (`item.type` ∈ `agent_message`|`assistant_message` → `text`) → `turn.completed`
  (`usage`). Non-agent `item.type`s (reasoning, tool calls) → `ToolStep`/`Other`.
- Usage mapping: `input_tokens`, `cached_input_tokens` → `cache_read_tokens`,
  `output_tokens`, `reasoning_output_tokens` → `thinking_tokens`.
- **Auth gate:** require `~/.codex/auth.json` with `auth_mode` denoting
  subscription. API-key-only ⇒ `PieError::NoSubscription`. Never echo the token.
- Models: shape from `~/.codex/models_cache.json` (`slug`, `display_name`,
  `default_reasoning_level`, `supported_reasoning_levels[].effort`); inject
  `gpt-6-luna`/`gpt-5.5` etc. only if the cache is absent.
- Usage: `GET https://chatgpt.com/backend-api/wham/usage` with
  `Authorization: Bearer <token>` + `ChatGPT-Account-Id: <account>`, both read
  from `~/.codex/auth.json`. Token never logged (redact in any error path).
- **This is the only place pie reaches a vendor endpoint directly**, and it is
  therefore gated: `codex.quota_probe = "off" | "http"` (env
  `PIE_CODEX_QUOTA_PROBE`). **`off` is the default** (user decision, 2026-10-12):
  it is kept reachable rather than deleted because the call is read-only using the
  user's own credential, but no private endpoint is reached unless the user asks
  for it — see `knowledge/9router-findings.md` §0 and §4. `codex` exposes no CLI
  subcommand for quota, so `PLAN.md` §6 specifies the endpoint; with `off`,
  `Codex::usage` returns an empty report with a `notes` entry naming the switch
  and makes no request at all, and `pie usage codex` still prints session-log
  totals.

### claude (M0 stub)

`binary()` resolves, `auth_status()` reports `NoSubscription`/`NeedsLogin` using
the `~/.claude/.credentials.json` + `~/.claude.json` facts from
`knowledge/claude.md`. `list_models()` returns a static catalog. `stream()`
returns `PieError::Unsupported`-style config error. `enabled = false` by default.

## 10. Profile projection and routing (M3)

`PLAN.md` §8: one config, one memory, one persona across all three CLIs. The
vendor's own configuration (`~/.codex/config.toml`, `~/.claude/settings.json`, agy's
agents) is **ignored** — pie never reads it and never writes it.

### Sources, under `$PIE_HOME`

| Path | Role |
|---|---|
| `profile.toml` | structured persona: name, tone, rules, output conventions |
| `AGENTS.md` | freeform rules, appended after the structured part |
| `memory/*.md` | durable notes, concatenated in **filename order** for determinism |

All three are optional. A missing file is an empty projection, never an error — a
fresh install must work with no configuration at all.

### Ordering: static first, volatile last

The projection is assembled in a fixed order, and this is a correctness rule rather
than tidiness (`TOKEN-ECONOMICS.md` §4):

1. the structured persona from `profile.toml`
2. `AGENTS.md`
3. `memory/*.md`, sorted by filename
4. *(the caller's job, after this)* the user's task text

**No volatile value may appear anywhere in the projection**: no timestamp, no session
id, no cwd, no model id, no quota figure. A volatile head invalidates whatever stable
prefix the vendor caches, on every single invocation — and the vendor prefix cache is
real and measured (`knowledge/agy.md`), so this is the one prompt-side cost pie
actually controls. A test asserts the projection is byte-identical when built from
different working directories and at different times.

### Projection per backend

`Capabilities` gains `system_prompt: bool` to say which mechanism a backend supports:

| Backend | Mechanism | Why this one |
|---|---|---|
| agy | folded **before** the prompt text in `--print <prompt>` | agy has no system role (`PLAN.md` §8) |
| codex | folded **before** the prompt text | `exec` has no verified system-prompt or instructions flag, and writing `AGENTS.md` into the user's own workspace is rejected as invasive |
| claude | `--append-system-prompt <text>` | documented in `knowledge/claude.md`; unverified live because this machine has no subscription, so it is implemented to spec and labelled |

The roadmap's M3 Done condition is *"same persona on agy and codex"*. Folding the
identical block into both prompts satisfies that literally, and it is verifiable at
the payload level **without spending quota** — which matters, because a persona test
that required two live turns would be run rarely.

The folded prefix spends argv budget on agy, whose prompt is capped at 20 000
characters (§9). A projection that pushes the argv past the cap must be a
`PromptTooLong` naming the projection as the cause, never a silently truncated
prompt — truncating a persona is a correctness failure, not a degradation.

### Routing

`general.default_backend`, plus a `route` command, choose a backend without changing
the persona. The first rule is measured rather than guessed: **codex answers a turn in
~3.6 s and agy in ~40 s**, so codex is the default unless agy is named explicitly or
the task class asks for it. Routing must never silently spend a premium sub's quota to
answer a trivial question.

### Deferred to M5, deliberately

Roadmap M3 also lists *"backend switch mid-session re-bootstraps from pie's
transcript"*. That clause needs a stored transcript, which is M5's session work, so it
is deferred rather than faked — recorded here because the two milestones depend on
each other in the opposite order to the one they are listed in.

## 11. Sessions, compaction and OCR (M5)

Contract for M5. Roadmap: session JSONL persist/resume, compaction, `pie ocr` plus an
MCP server so the delegate CLIs can reach it, and the "backend switch mid-session"
clause M3 deferred into here.

### Session store — a NEW file, never the frozen log

`usage.jsonl` is frozen and shared with the earlier JS implementation (§7), so
transcripts live beside it rather than inside it:

| Path | Contents |
|---|---|
| `$PIE_HOME/sessions/<id>.jsonl` | the transcript: one row per turn, append-only |
| `$PIE_HOME/sessions/index.jsonl` | `{id, provider, cwd, started_at, updated_at, turns, preview}` for lookup without scanning every file |

A turn row: `{ts, provider, model, prompt, response, conversation_id, usage, ok, stop_reason}`. A session is identified by **pie's own id**, not the vendor's, because the vendor id differs per backend and is lost on a switch; the vendor id is recorded per row so a resume can still pass it back.

Two things the fixed key set does *not* say, pinned here after stage A raised them:

- **A failed turn's reason lives in `stop_reason`**, paired with `ok: false`. The nine keys have no separate error field, and inventing a tenth would fork the schema that `usage.jsonl` already froze.
- **The index is append-only frames, last-wins by id.** A crash mid-write therefore loses at most the newest frame, and the per-session transcript files stay the source of truth — the index is a lookup convenience that can always be rebuilt from them.

**Nothing here spends quota** — a session write is local I/O — and a write failure
must never fail a turn that otherwise succeeded.

### Resume, deliberately not automatic

`--resume <ID>` is already present and explicit. `--continue` picks the most recent
session for the current provider and working directory; `--session <ID>` targets one.
Auto-resume is **not** the default: `knowledge/agy.md` measured resume as a
context-continuity feature rather than a latency one (41.2 s resumed against 40.5 s
fresh), and silently reusing a conversation can answer a question in the wrong
context. The user opts in.

### Compaction: deterministic and quota-free by default

Past `agent.compact_after_turns` turns, older turns collapse into a state block. **The key is not yet wired to config**: the threshold is currently a constant of 40, and stage F uses it directly. Recorded as S6 rather than left as a promise the code does not keep.

- **Extractive, not summarised.** The head and a tail window are kept; the middle is
  replaced by a structured block of what was asked and what changed. No model call,
  so compaction can never surprise the user with spent quota (`RISKS.md` §13).
- **Stable position.** The block sits *after* the static persona and *before* the
  volatile task text, so compacting does not invalidate the vendor's cached prefix
  (`TOKEN-ECONOMICS.md` §4).
- **Non-destructive.** Stored rows are never deleted. Compaction changes only what is
  *sent*, never what is *kept*, so it is reversible by inspection.

Model-based summarisation is out of scope: M7's `learn` job already owns "spend a
budgeted model call on my own history" and is the right home for it.

### OCR is an MCP server, not a pie tool

`pie ocr <file>` POSTs `{file, lang?, output}` to itsmini and caches the result under
`$PIE_HOME/ocr-cache/`, keyed by content hash. The token argument is ~1–2k vision
tokens becoming ~200–500 text tokens.

In delegate mode agy and codex run their **own** tools, so a pie-native tool never
reaches them (`RISKS.md` §11). MCP is the only route that serves all three: `pie mcp
ocr` runs a stdio server exposing exactly one tool, and the user points each CLI at
it through that CLI's own MCP config.

itsmini is a separate, unbuilt project. Until it exists, `pie ocr` and the server
return a clear *itsmini not reachable* error — never a silent empty result.

MCP overhead is real: an advertised tool raises the vendor's per-turn input tokens on
**every** turn, including ones that never call it. So the server is opt-in, exposes
exactly one tool, and must not bundle a second.

### Backend switch — the clause M3 deferred

`--switch <backend>` continues the current session on a different backend by
re-bootstrapping from pie's own transcript: the state block plus the tail window
becomes the prompt for the new backend, and the new vendor conversation id is
recorded. This is a payload transformation and therefore testable without quota. It is
what "no per-CLI config" means in practice — the persona and the history travel with
the user, not with the CLI.

Three properties the implementation must hold, each with a test:

- the **old vendor id is never handed to the new backend** — otherwise the switch
  silently becomes a resume against a conversation the new CLI has never seen
- the persona still comes first and the task text last, so switching cannot break
  the prefix cache any more than compaction can
- switching while `--resume` is set is refused: `--resume` names a vendor
  conversation on one backend, and leaving it is the entire point of switching

Composing `compact::assemble` drops the per-turn framing stage B wrote, so on a
switched turn a failed turn's `stop_reason` reaches the payload only through the
state block. `compact.rs` is frozen by design, so this is recorded as S7 rather than
patched around.

### Stages

Each stage is verifiable on its own, so an incomplete milestone shows up at the stage
that is incomplete rather than at the end: **A** persistence · **B**
`--continue`/`--session` · **C** compaction · **D** `pie ocr` · **E** MCP server ·
**F** backend switch · **G5** live resume across two turns.

## 12. Interactive TUI (M6)

Roadmap: *model picker, streaming output, keybindings*; Done: *interactive parity across
agy + codex*. New dependencies, the first since M0: `ratatui 0.29` + `crossterm 0.29`,
both verified to leave the G6 cross-target check clean.

### Testability is the design driver

A TUI resists automated testing, so the one part that cannot be tested is kept as
small as possible and everything else is pure:

| Layer | Testable how |
|---|---|
| `tui::app` — reducer, state | unit tests: a neutral `Input` reaches *no* crossterm type, so `key → Action → state` is asserted without a terminal |
| `tui::render` | ratatui's `TestBackend`: render into an in-memory buffer and assert its contents |
| `tui::run` — the terminal loop | **not** automatable. Keep it thin: only crossterm setup, a select loop, and teardown |

**The reducer must never see a crossterm type.** That single rule is what makes M6
testable at all; a `KeyEvent` parameter would drag the terminal into every test.

### Frozen interfaces (so the layers can be built in parallel)

**These are frozen as compiling code, not as prose.** `src/tui/{app,render,mod}.rs`
landed first as a scaffold: the types, the field lists and the signatures below are
committed, and the three stage bodies are `SCAFFOLD` stubs that compile and return
truthfully (`reduce` → `Action::None`, `render` → nothing drawn, `run_session` →
an explicit `Unsupported` error, never `Ok(())`).

That ordering is deliberate and is `ACCEPTANCE.md` §1's first rule applied properly:
with only the *prose* frozen, stage B's gate cannot compile until stage A's file
exists, so "three parallel stages" would in fact be one stage followed by two — and
the tempting workaround, letting each agent write its own copy of `App`, is how
types drift and one stage's rewrite deletes another's work. Freezing the contract as
code removes the race rather than writing a rule against it.

```rust
pub enum Input { Char(char), Enter, Esc, Backspace, Up, Down, Left, Right, Tab, Ctrl(char) }
pub enum Action { None, Quit, Submit, Cancel, ScrollUp, ScrollDown, OpenPicker,
                  PickerNext, PickerPrev, PickerAccept }
pub enum Status { Idle, Running, Done, Failed(String) }
pub enum Entry { User(String), Assistant(String), Tool { name: String, state: ToolState }, Notice(String) }

pub struct PickerItem { pub backend: String, pub model: String, pub label: String }
pub struct Picker { pub items: Vec<PickerItem>, pub selected: usize }

pub struct App {
    pub prompt: String,    pub cursor: usize,
    pub backend: String,   pub model: Option<String>,
    pub session: String,   pub vendor_id: Option<String>,
    pub status: Status,    pub transcript: Vec<Entry>,
    pub tool_events: Vec<(String, ToolState)>,
    pub scroll: u16,       pub last_usage: Option<Usage>,
    pub picker: Option<Picker>, pub should_quit: bool,
}

pub const VENDOR_TOOL_NOTE: &str;   // the sentence that says the tools are the vendor's

pub fn reduce(app: &mut App, input: Input) -> Action;
pub fn apply_event(app: &mut App, event: &StreamEvent);
pub fn render(frame: &mut Frame, app: &App);
pub async fn run_session(app: &mut App, cfg: &Config) -> Result<()>;   // the thin loop
```

`reduce` and `apply_event` take and return only pie's own types. `Frame` is ratatui's
and appears in exactly one signature. `render` takes `&App`, never `&mut App`, so
drawing cannot change state.

Four fields are named here that an earlier draft left to prose, and each is load
bearing: `cursor`, because `Char`/`Backspace`/`Left`/`Right` are defined by the
position they edit; `vendor_id`, because the terminal event carries the vendor's
conversation id and a TUI that discarded it would lose the id `--resume` needs;
`last_usage`, because render must show what a turn cost and a partial terminal event
must not blank it; and `Picker`/`PickerItem`, because picking a model has to produce a
*backend and a model*, so a `Vec<String>` of labels would be unable to set `App::model`.

### The keybinding table is the specification

It is exposed as **data** so the help and the reducer cannot drift: `app::KEYBINDINGS`
is a `const` slice of `KeyBinding { keys, action, description, input }`, `Action::label()`
gives the footer's word, and `app::help_text()` renders the table. `render.rs` draws it
between the prompt and the status line, so the status line stays the frame's last row.

**Measured, and part of the criterion:** the footer is pre-wrapped by `render.rs`
(`help_lines(width)`, greedy at whitespace) and drawn **without** `Wrap`, so the pane
reserves a height it has counted rather than guessed, and `render::help_rows(width,
height)` is the single function the test reads the region from. The whole keymap is
visible from **35 columns x 24 rows**; at **80x24** — an ordinary terminal — it uses 5
rows and leaves 15 for the transcript. Below 35 columns it clips.

That measurement is the criterion, not a footnote. The first version reserved two rows
and fitted whole only from about **200 columns**, so `ACCEPTANCE.md` §9's "every
keybinding is documented in help" was true only on a terminal nobody has: at 80 columns
the buffer omitted `tab` entirely. A test at a generous width would have kept that
hidden, which is the same defect one level down.

`KeyBinding::action` holds the `Action` **variant** rather than a display string on
purpose: the keymap test then compares `reduce`'s return mechanically, so a documented
row cannot agree with a string while disagreeing with the code.

The frozen `Input → Action` mapping. Stage D asserts it row by row, and the help text
is generated from it, so the table cannot drift from the behaviour:

| Input | Action | State change |
|---|---|---|
| `Char(c)` | `None` | insert `c` at the cursor |
| `Backspace` | `None` | delete the character before the cursor |
| `Left` / `Right` | `None` | move the cursor, clamped to the prompt |
| `Enter`, picker open | `PickerAccept` | highlight taken as the model; picker closed |
| `Enter`, prompt submittable, **status is not `Running`** | `Submit` | `Entry::User` pushed; prompt cleared; `Status::Running` |
| `Enter`, otherwise | `None` | nothing |
| `Tab`, picker closed | `OpenPicker` | picker opened **empty**, for the caller to fill |
| `Tab`, picker already open | `None` | nothing — re-opening must not discard the catalog the caller filled in |
| `Up` / `Down`, picker open | `PickerPrev` / `PickerNext` | highlight moved, clamped |
| `Up` / `Down`, otherwise | `ScrollUp` / `ScrollDown` | scroll moved, never below zero |
| `Esc`, a turn is running or a picker is open | `Cancel` | picker closed; `Idle` **only if the turn was running** |
| `Esc`, nothing to cancel | `Quit` | `should_quit` |
| `Ctrl('c')` | `Quit` | `should_quit` |
| any other `Ctrl(..)` | `None` | nothing |

Two deliberate choices. **`Enter` is refused unless the prompt has non-whitespace
text**, so a turn is never spent on an empty prompt — the one keystroke a user can hit
by accident that costs real subscription quota. And **`Up`/`Down` mean different
things depending on the picker**, because the picker is modal; overloading them is what
keeps the keymap small enough to document in full.

### The turn lifecycle, and the bug freezing it too tightly caused

The row above reads **"status is not `Running`"**, not "`Idle`", and that distinction was
a real defect in the first freeze rather than a wording preference. The original table
sent `Submit` only from `Idle` and had `Done` set `Status::Done`, with nothing anywhere
returning to `Idle` — so **the TUI could send exactly one turn and then never
submitted again.** Stage A implemented the table faithfully and reported the
contradiction instead of quietly patching it, which is the behaviour that rule exists
to produce; the contract was wrong, not the code.

The lifecycle is now stated once, here:

| From | Event | To |
|---|---|---|
| `Idle` | `Submit` | `Running` |
| `Running` | `apply_event(Done)` | `Done` |
| `Running` | the loop's `Finished(Err(e))` | `Failed(e)` |
| `Running` | `Esc` (`Cancel`) | `Idle` |
| `Done` / `Failed` | `Submit` | `Running` |

Two consequences worth naming:

* **The loop's `Finished` is the authoritative terminal status.** `apply_event(Done)`
sets `Done` from the *stream*, but the turn's real outcome arrives on the channel, and a
`Done` whose vendor status is an error must end as `Failed` rather than as `Done`.
`Finished` therefore overwrites, and a test asserts exactly that ordering (a `Done`
followed by `Finished(Err(..))` ends `Failed`). The residual case — `Finished(Ok)` whose
`Done` carried a non-success vendor string — is logged as an open suggestion, because
recognising success vocabulary is backend knowledge and pie's agy parser already turns
`status: "ERROR"` into an error, so the TUI should not guess a second time.
* **`Esc` while the picker is open does not touch the status.** Closing an overlay must
not erase a failure reason the user has not read yet; `Cancel` therefore returns to
`Idle` only when a turn was actually running.

`Action::Submit` and `Action::PickerAccept` carry no payload, so the state change has to
happen inside `reduce`: `Submit` pushes `Entry::User` with the submitted text, and
`PickerAccept` sets **both** `App::backend` and `App::model` from the highlighted
`PickerItem`. The loop must not push a second `User` entry — it sees the same submission
and would otherwise double it.

`App::scroll` has **no ceiling in the reducer**, deliberately: how far down is meaningful
depends on the transcript's rendered height, which only `render` knows. `reduce` clamps
at zero from below and `render` clamps what it draws, so a `scroll` past the end is an
empty window rather than a panic or an out-of-range draw.

### `apply_event`'s frozen behaviour

| Event | Effect |
|---|---|
| `Init` | `conversation_id` → `App::vendor_id`; a `Notice` naming the conversation and its tool count |
| `TextDelta` | appended to the last entry when it is already `Entry::Assistant`, else to a new one |
| `ToolStep` | pushed to `App::tool_events` **and** the transcript, where the row carries `VENDOR_TOOL_NOTE` |
| `Usage` | merged into `App::last_usage` via `Usage::merge` |
| `Done` | `Status::Done`; pushes `text` as an assistant entry **only when nothing was streamed** |
| `Other`, `Unparsed` | **ignored**: protocol noise, already recorded under `PIE_LOG`, and a `Notice` per line would fill the transcript with vendor chatter |

Appending deltas rather than replacing is what makes streaming visible; one entry per
delta would grow the transcript without bound on a long answer. The `Done` rule is the
twin of `B21`'s fix on the backend side — a terminal event must not duplicate what has
already been delivered.

"**nothing was streamed**" means *since the last `Entry::User`*, and an empty delta does
**not** count as streaming. The first draft left this open and both halves bit:
scoping to the whole transcript suppresses the answer on **every turn after the first**,
and counting an empty delta as streaming means a turn that ends on a tool step renders
as having no answer at all. Both are asserted.

### Stages, and who owns which file

| Stage | Owns | Gate |
|---|---|---|
| A state + reducer | `src/tui/app.rs`, `tests/tui_app.rs` | `cargo test --test tui_app` |
| B render | `src/tui/render.rs`, `tests/tui_render.rs` | `cargo test --test tui_render` |
| C loop + wiring | `src/tui.rs`, `src/commands/tui.rs`, `tests/tui_loop.rs` | `cargo test --test tui_loop` |
| D picker + keys | `src/tui/app.rs`, `tests/tui_keys.rs` | `cargo test --test tui_keys` |
| G5 | a **real turn's** events through `apply_event`, asserting the rendered buffer | live, by hand |

### Correcting a contract mid-flight invalidates the artefacts written from it

B26 is the case that made this rule explicit. The keybinding table was corrected while
stage A and stage D were both writing against it, and stage A had already encoded the
**old** `Tab` row into a test helper (`tests/tui_app.rs`'s
`documented_action_with_picker_open`). Stage D implemented the corrected row, and that
helper then failed — correctly, and for the right reason.

Two things made this recoverable rather than a mess. Stage D **reported the conflict
instead of editing another stage's file**, which is rule 1 doing its job; and the
corrected row had its own named regression test in stage D's file, so the stale helper
was identifiable as stale rather than as the code being wrong. The maintainer fixed the
one-line helper.

The generalisable rule: **when a frozen table is corrected after a stage has written
against it, grep for the old row before declaring the milestone green.** A contract is
not just read once; anything derived from its earlier version is a derived artefact, and
the session's other lesson — every claim needs something that would fail if it were
false — applies to those too.

Each stage gate names the stage's **own** test binary. That is not boilerplate: in M5
two of three stages shipped with a gate that could not fail them, because the gate ran
a different stage's tests (`ACCEPTANCE.md` §8).

Stage D refines A's own subject matter, so the tempting choice is to add D's tests to
`tests/tui_app.rs` and let one gate cover both. It is rejected on purpose: a red gate
has to say **which** stage broke, and "the reducer suite failed" cannot distinguish A's
invariants from D's keymap. D therefore gets its own binary, and the small amount of
fixture duplication that costs is worth the attribution.

`cli.rs`, `commands.rs` and `lib.rs` stay with the maintainer; stage C adds
`src/commands/tui.rs` and the maintainer wires it, so no agent edits a shared file.

### Two constraints that shape the behaviour

- **Delegate mode means the TUI shows the vendor's tool steps, not pie's own.**
  `agy --print` and `codex exec` run their own tools (`RISKS.md` §1), so this is
  architecturally unlike the original pi TUI, which displayed its own tool cards.
  Saying so in the UI is better than implying a capability that does not exist.
- **The UI never blocks on a turn.** The turn runs as a task and pushes
  `StreamEvent`s down an `mpsc` channel; the loop selects over that channel and
  crossterm's event stream. A synchronous turn would freeze the interface until the
  vendor CLI returned — up to 40 s on agy.

### Entry point

`pie tui`, opt-in. A bare `pie` keeps printing help, so CI and every existing test
are unaffected, and the TUI can never be entered accidentally by a script.

### Stages

A state + reducer · B render (TestBackend) · C the thin terminal loop and channel
wiring · D model picker + keybindings · G5 a **real turn's** events driven through the
reducer, asserting the rendered buffer shows the answer.

G5's shape is deliberate: keystrokes cannot be automated, but "the TUI renders a real
vendor turn" is the substance of M6's Done condition and *is* checkable — the same
discipline that made M3's persona gate provable rather than asserted.

## 13. CLI surface for M0–M3

Additions since M0–M2: `--resume <ID>` (M5-preview, see §10's note on ordering) and
`profile` (M3). `profile` prints the assembled projection and the provenance of each
contribution, spawns nothing, spends no quota and writes no session row — it exists
so the persona is *inspectable*, because a persona you cannot read is one you cannot
debug.

```
pie --list-backends                 # M0 gate
pie models [--backend B] [--refresh]
pie auth                            # thin in M0–M2, unified table lands in M4
pie usage [backend]
pie -p <PROMPT> --model <prov/model> [--effort E] [--timeout S] [--cwd DIR] [--json]
pie doctor                          # binary presence + version + auth, no token spend
pie --version
```

`--json` emits one JSON object per line (same `StreamEvent` serde shape) so the
parser is tested by the CLI itself, not only by unit tests. Human mode prints
deltas as they arrive. Zero-token guarantee: `models`/`auth`/`doctor`/`--list-backends`
must never spend model quota (catalogs come from disk/help, not a model turn).

## 14. Test strategy

| Layer | What | Runs |
|---|---|---|
| Unit (inline `#[cfg(test)]`) | pure parsers fed recorded lines: agy event→`StreamEvent`, codex event→`StreamEvent`, usage mapping, argv builder, config precedence, env scrub, prompt cap, error→exit code | `cargo test` |
| Integration `tests/` | registry lists agy/codex/claude; config file+env+flag precedence; scrub removes exactly the deny-list; prompt cap fires at >20 000; tolerant parse survives garbage/unknown events | `cargo test` |
| Live smoke `tests/live_smoke.rs` | real `agy`/`codex` turns and usage probes | `#[ignore]`, only with `PIE_LIVE=1` — **spends subscription quota** |

Rules: no test spawns a real CLI unless `PIE_LIVE=1`. Fixtures live in
`tests/fixtures/*.jsonl` and are copied from the verified samples in
`knowledge/agy.md` / `knowledge/codex.md` — not invented. Every milestone gate
runs `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test`.

## 15. Explicitly deferred

M3 profile/memory projection · M4 unified auth/usage tables · M5 agent loop +
sessions + OCR MCP · M6 ratatui TUI · M7 learn · M8 watchtower · M9 claude +
tool-loop · M10 HTTP backends. `tool_loop` stays `false` for every M0–M2 backend;
the flag exists now so the loop can branch on it later (RISKS §1, §2).

---

## 16. Daily learn + schedule (M7)

Roadmap: `schedule.rs` + `learn.rs`: mine pie + CLI sessions → profile/memory updates;
consolidation + provenance + decay; local-only; cheap/local model budget. Done:
*after a few sessions, `profile.toml`/`memory/*.md` reflect real habits; `/memory`
shows what pie learned and lets you revert it.*

> Appended as §16 rather than inserted after §11, to avoid renumbering the
> cross-references these documents already make to existing sections.

### The design driver: the default path spends nothing

This is the whole shape of M7. `RISKS.md` §13 is the governing risk — "an
LLM-summarizing job can silently spend premium subscription quota" — and this machine
has **no cheap backend**: every backend pie drives is subscription-locked, so PLAN
§11b's "cheap/local model by default" has nothing to name today. The honest reading is
therefore stronger, not weaker: **the default path makes no model call at all.**

`learn` is *extractive by rule*, exactly as M5's compaction is (`DESIGN.md` §11 —
"extractive: head + state block + tail; spawns nothing"). It counts recurrences,
recognises explicit preference statements (`always`, `never`, `I prefer`, `don't`), and
derives stack and terminology from prompt text and tool names. No model is consulted to
produce a candidate, so `pie learn` is quota-free, deterministic and testable, and
model-based consolidation becomes an **opt-in extra** rather than the mechanism.

The second reason matters more: a rule-based extractor has no failure mode that can
spend money silently. A model-based one has exactly one, and it is the risk `RISKS.md`
§13 already names.

### Decisions

| Decision | Choice | Why |
|---|---|---|
| Default model use | **none** | No cheap backend exists, so `RISKS.md` §13's budget becomes a gate rather than a note |
| Model consolidation | opt-in, budgeted, **off** by default | Requires config naming a backend **and** a token budget; refuses *before* spawning if the budget cannot cover the job |
| Scheduling | opt-in cadence; `due()` is a **pure function of `(last_run, cadence, now)`** | A job runner that reads the clock inside its own decision cannot be tested |
| Artifacts | every run writes a dated report listing candidates, what was written, and what was skipped | "never silent" (PLAN §11) — a job whose effect you cannot read is one you cannot revert |
| Provenance | every learned line carries its source session id and turn index | a fact you cannot trace is one you cannot judge |
| Decay | a candidate not re-observed within the window loses confidence and is demoted, **never** silently deleted | memory that rots invisibly is `RISKS.md` §16 |
| Revert | `pie memory revert <ID>` restores the pre-apply state from a journal | "reviewable and revertible" (PLAN §11b) is a testable claim, so it gets a test |
| Privacy | pie's own sessions by default; vendor stores (`~/.gemini`, `~/.codex`, `~/.claude`) are **opt-in, read-only, never written** | `RISKS.md` §15. No M7 path sends anything off-machine |
| Stable prefix | consolidation **appends** to a later section and never reorders the head of the projection | `TOKEN-ECONOMICS.md` §3: rewriting the head invalidates the vendor's cached prefix once a day |

### Frozen interfaces

M7's stages are less parallel than M6's — consolidation needs extraction's output, and
`/memory` reads what consolidation wrote — so only the cross-stage types are frozen,
and each stage owns its own files.

```rust
// src/learn.rs
pub enum CandidateKind { Preference, Stack, Convention, Terminology, Habit }
pub enum Source { PieSession, AgyHistory, CodexHistory, ClaudeHistory }
pub struct Provenance { pub session_id: String, pub turn_index: usize, pub source: Source }
pub struct Candidate {
    pub kind: CandidateKind, pub text: String, pub provenance: Provenance,
    pub hits: u32, pub last_seen_ms: u64,
}

pub struct MinedTurn {
    pub session_id: String, pub turn_index: usize, pub source: Source,
    pub prompt: String, pub answer: String, pub ok: bool,
    pub ts: u64,          // epoch millis, read from the stored row
}

pub fn mine_sessions(sessions_root: &Path) -> Result<Vec<MinedTurn>>;   // pie's own store only
pub fn extract(turns: &[MinedTurn]) -> Vec<Candidate>;          // pure: no spawn, no clock
pub fn consolidate(existing: &MemoryDoc, candidates: &[Candidate], now_ms: u64) -> Consolidation;
pub fn apply(consolidation: &Consolidation, home: &Path) -> Result<Journal>;  // the revert handle
pub fn revert(journal: &Journal, home: &Path) -> Result<()>;

// src/schedule.rs
pub fn due(last_run_ms: Option<u64>, cadence: Cadence, now_ms: u64) -> bool;   // pure
pub struct Budget { pub max_tokens: u64, pub spent: u64 }
pub fn check_budget(budget: &Budget, estimate: u64) -> Result<()>;            // refuses BEFORE spawning
```

Three things the first freeze left implicit and stage A had to decide. They are contract
now, because each changes what a *report* means (`DESIGN.md` §16's own reason for writing
facts down rather than deciding them silently):

* **`MinedTurn` carries `ts`.** The first version had no timestamp, while requiring
  `last_seen_ms` to come "from the turn, never from a clock read" — an unsatisfiable
  pair. The only honest options were a clock read (`extract` must stay pure), a constant
  (which would make every candidate look ancient to decay), or a timestamp on the turn.
  Stage A added the field and **reported it rather than quietly reading a clock**, which
  is the behaviour the "report, never silently deviate" rule exists to produce.
* **`hits` counts turns, not occurrences.** One mention repeated three times in one turn
  is one observation. "A habit seen five times" has to mean five separate turns, or a
  single verbose prompt outranks a real pattern.
* **`Provenance` names one observation, and it is the newest one** consistent with
  `last_seen_ms`, ties broken by `(session_id, turn_index)`. A merged candidate has many
  observations and one provenance field, so which one it names has to be a rule rather
  than an accident of iteration order — the same reason the output is a total order.

**Ranking is a total order**, so output is independent of input order and two runs are
byte-identical: `(confirmed desc, hits desc, kind, text)`, where `confirmed` counts the
`ok` turns. Without the `confirmed` term a candidate seen only in failures would outrank
one seen in successes, contradicting `MinedTurn::ok`'s own documentation.

Two rules carried over from M6 because they are what made M6 testable: **`extract` and
`due` take the clock as a parameter and never read it**, and **nothing in `learn.rs` or
`schedule.rs` calls `provider::stream` except behind `check_budget`.**

### `/memory` in the TUI does not extend M6's `Input`

The TUI reaches `/memory` through the ordinary `Submit` path: the loop inspects the
submitted prompt for a leading `/` and dispatches a *pie-local* command in-process.
M6's `Input` enum is frozen and stays frozen — a slash command is a property of the
submitted text, not a new keystroke. That is what `Action::Submit` was specified as
"the caller decides what a submission means" for.

### The cross-cutting gate, as a gate

`ROADMAP.md`'s cross-cutting list requires background jobs to be *budgeted* and
*opt-in*. Prose cannot enforce that, so M7 ships it as assertions with teeth, using the
counting-stand-in pattern `tests/cache.rs` already established:

* with the **default** config, a scheduled run performs **zero** provider spawns — and
the *count* is asserted, not merely the absence of an error;
* a job whose estimate exceeds the budget is **refused before any spawn**, so a budget
of `0` cannot be "exceeded by one call";
* `due()` is asserted at and around its boundary, not only in the middle;
* the ledger records `spent`, so a second run inside the same window cannot re-spend it.

### CLI surface

```
pie learn [--dry-run] [--report]      # extract + consolidate; --dry-run writes nothing
pie memory [show|revert <ID>]         # what was learned, with provenance; revert it
pie schedule [status|run <JOB>]       # what is due, what last ran, and the budget state
```

All three are quota-free under the default config. `pie learn --model b/m` is the opt-in
model consolidation, and is refused without a configured budget.

### What stage B had to decide, now contract rather than memory

* **On disk:** `$PIE_HOME/memory/learned.md`, the undo handle at
  `$PIE_HOME/memory/learned.journal`, and a scratch `learned.md.tmp`. Two choices carry
  their own justification. The journal is deliberately **not** `.md`, because
  `profile.rs` includes only files whose extension is `md` (`MEMORY_EXTENSION`, asserted
  with a `notes.txt` control) — a JSON undo handle named `.md` would be concatenated
  into the persona and sent to a vendor on every turn. And the write is
  scratch-then-**rename**, so a crash leaves a stale `.tmp` that the projection ignores
  rather than a half-written persona.
* **`Consolidation`'s three vectors hold rendered *lines*, not candidates.** `apply` is
  handed no document, so only text carries the merged `hits`/confidence. That is a
  shape consequence, not a preference.
* **`Journal` has no `existed` flag.** An empty document therefore restores as absent.
  The two are indistinguishable in the projection, so the ambiguity is harmless — but it
  is stated rather than left for someone to discover from a `revert` that recreated a
  file they had deleted.
* **`MemoryDoc::head` is the learned document's preamble, not the head of the
  projection.** `apply` may not write `profile.toml` or `AGENTS.md` at all, so prefix
  stability is a property of the **assembled projection**, and that is where it is
  asserted — a test comparing only the files on disk would miss a reordering done inside
  the assembly.
* **`hits` merges by `max`, never by sum.** The same sessions are re-mined every day, so
  a sum would inflate without bound and a fact's confidence would be a function of how
  long the job had been installed rather than how often the user does the thing.
* **Entry dates are day-granular**, so an entry's `last_seen_ms` re-reads as midnight UTC
  — up to a day of fuzz against a 90-day window. Acceptable at that ratio; it would not
  be at a 7-day one.
* **`Habit` and `Convention` need at least two observations**, and a candidate that
  matches an existing entry is merged rather than suppressed by the head-dedupe rule.

### Stage C's rulings, and the one seam it confirmed

* **"The same window" now means one cadence interval**, and the budget is **per window
  rather than per run** (maintainer's ruling: stage C reported the ambiguity rather than
  choosing silently). The ledger records what the window spent, so a second run inside it
  — **including an explicit `pie schedule run`** — spends the remainder. A per-run budget
  would make the ceiling a function of how many times the user typed a command, which is
  not a budget.
* **`Cadence::Monthly` is a fixed 30 days**, not a calendar month, and its doc now says
  so. A 31-day month drifts by a day, which is fine for a monthly digest and is what
  `interval_ms` actually promises; "calendar month" claimed precision the function does
  not have. The rolling rule is deliberately narrow: the window only rolls on a
  **scheduled** run whose stamp is in the past, so neither a manual run nor a backwards
  clock can hand the budget back early.
* **`run_job` takes the spawn as a parameter** (`S: FnOnce(&Job) -> Result<Spend>`),
  rather than calling `provider::stream` itself. That is the only shape in which "zero
  provider spawns" can be **counted** instead of described — the requirement's whole
  point. `run_job` also wraps `check_budget`'s error to name the job, because the frozen
  `check_budget(&Budget, u64)` signature has no room for a job name.
* **`Job`/`Ledger` read `config::Jobs`.** That dependency is outside this section's frozen
  list and is intentional: `config.rs` owns the serialisation surface (`JobCadence`,
  which maps to `schedule::Cadence` by an exhaustive match so the two cannot drift) and a
  separate test asserts the shipped defaults are `never` / `0` / vendor-history off, with
  a zero budget really refusing. The property is only meaningful as a claim about the
  *defaults*, so it is asserted there and `run_job` is asserted to reach no provider.

---

## 17. Monthly watchtower (M8)

Roadmap: `watch/news.rs` + `watch/drift.rs`. Done: `~/.pie/reports/watchtower-YYYY-MM.md`
produced and notified; the drift diff shows any flag, schema or policy change.

### Layout follows `provider.rs`, not `mod.rs`

`src/watch.rs` beside `src/watch/{news,drift}.rs` — the same shape as `provider.rs`
beside `provider/*.rs`. ACCEPTANCE §3 forbids `mod.rs`, and until M6 that rule had no
enforcement at all (see the bug log): the M6 plan named `src/tui/mod.rs`, broke the
rule, and the suite stayed green. M8 gets the shape right and the rule gets a gate.

### Two jobs, one report, and neither spends model tokens by default

PLAN §11a: *"monthly, configurable, network-only (no model tokens unless you ask for a
digest)"*. That single clause decides M8's shape, and it is the same decision M7 makes:

* **`drift`** spawns each CLI for its own `version` and `--help` — the probes I8 already
  established. They cost **no** model tokens (a `--help` is not a turn), which is why
  drift is safe to run on a schedule at all.
* **`news`** fetches configured sources over the network and ranks them by **rule**
  against the stack configured in `profile.toml`. No model is consulted.
* A model-written digest is an **opt-in extra** behind the same budget gate as M7's
  model consolidation. One budget mechanism, two consumers — not two mechanisms.

### Compliance framing is a requirement, not a tone choice

PLAN §11a is explicit: the watchtower *"flags when a change means pie must change
behavior or stop, never how to evade a safety measure"*. So the drift report classifies
every finding into exactly one of three outcomes, and the classification is part of the
contract because it is what the report is *for*:

| Class | Meaning |
|---|---|
| `compatible` | a flag or field appeared; pie needs no change |
| `pie-must-change` | a flag moved, was renamed, or a stream field changed shape; a named pie file must follow |
| `pie-must-stop` | a new policy, consent, or anti-automation measure; driving the CLI is no longer the sanctioned path, and the report says so plainly |

A digest that only reported findings would be a changelog. The class is what makes it
usable, and every finding that cannot be classified must be reported as *unclassified*
rather than defaulting to `compatible` — a silent downgrade to "fine" is the failure
mode that would make the whole job worthless.

### Snapshots are compared, so the first run has nothing to compare to

A drift check with no previous snapshot must report "first snapshot taken, nothing to
diff" and **never** "no drift found". Those two are indistinguishable in a report that
only prints findings, and only one of them is true.

### Frozen interfaces

```rust
// src/watch.rs
pub enum DriftClass { Compatible, PieMustChange, PieMustStop, Unclassified }
pub struct CliSnapshot { pub backend: String, pub version: String, pub help: String, pub strings: Vec<String>, pub taken_ms: u64 }
pub struct Drift { pub backend: String, pub class: DriftClass, pub what: String, pub detail: String }

pub fn snapshot_dir(home: &Path) -> PathBuf;                       // $PIE_HOME/watch
pub fn reports_dir() -> PathBuf;                                   // $PIE_HOME/reports
pub fn render_report(month: &str, drifts: &[Drift], news: &[NewsItem], first_run: bool) -> String;

// src/watch/drift.rs
pub fn diff(previous: &CliSnapshot, current: &CliSnapshot) -> Vec<Drift>;   // pure
pub fn classify(entry: &Drift) -> DriftClass;                      // pure, table-driven

// src/watch/news.rs
pub struct NewsSource { pub name: String, pub url: String }
pub struct NewsItem { pub title: String, pub url: String, pub source: String, pub impact: u32, pub matched: Vec<String> }
pub trait Http: Send + Sync { fn get(&self, url: &str) -> Result<String>; }
pub fn rank(items: Vec<NewsItem>, stack: &[String]) -> Vec<NewsItem>;   // pure, rule-based
pub fn fetch(source: &NewsSource, http: &dyn Http) -> Result<Vec<NewsItem>>;  // network
```

Three corrections the scaffold made to the first draft, each for a reason worth keeping:

* **`NewsSource`, not `Source`.** `learn::Source` already means "which store a turn came
  from" (M7). Two `Source`s in one crate is a name that means nothing at a call site.
* **The transport is a `trait Http`, not a concrete client.** A test that reaches the
  network fails when a blog reorders its index, which teaches nothing about pie. The
  trait is the seam that makes `fetch` testable *and* keeps the network in one place.
* **`DriftClass::label()` is part of the type**, because the classification is the
  report's whole content and a label written twice (once in `render_report`, once in a
  test) is a label that drifts.

Snapshots live in `$PIE_HOME/watch` and reports in `$PIE_HOME/reports` — different
lifetimes, one diffed and one read.

### The `--help` producer, which the first draft of this section assumed

`DESIGN` said drift diffs each CLI's `--help`; nothing in `src/` could produce it.
`Provider` had `version_args`/`version_command` and no help equivalent, so `CliSnapshot::help`
had **no producer at all** — a contract that names a capability nothing implements, which is
this project's most-recorded failure shape (B25/B27/B28/B30/S12). Stage A reported it
instead of hardcoding argv per backend at the call site.

The fix is two trait methods mirroring the version pair, because the *reason* is identical:
codex is `node <codex.js>` on Windows and its `.cmd` shim cannot be spawned without a
shell, so `help_command` overrides to carry the launcher prefix. A hardcoded `--help`
against the launcher would compare **node's** help text between runs and report the
difference as a change to codex.

**A probe that returns empty help is a diagnostic, not a quiet snapshot.** An empty capture
is unrepresentable in `render_report` (which sees only findings), so it would read as "no
drift". The probe must treat empty output as a stated error naming the backend.

### What stage A decided, where this section was silent

* **A changed version is `PieMustChange`**, not `Unclassified`. `RISKS.md` §3 and I8 make the
  recorded version the baseline the parsers were validated against, so a bump is a signal
  to re-verify argv, not an unknown.
* **`classify` is keyed on the finding's label**, because the frozen `classify(&Drift)`
  signature hands it nothing else. That is why the finding vocabulary is a closed table
  (`CLASSES`) with every class reachable from a fixture — a class no fixture produces is a
  class nobody has checked, and the test suite asserts that rather than assuming it.
* **A rename is paired by similarity and can only escalate a class, never downgrade it.**
  An unpaired "removed" plus "added" would read as two benign findings when the truth is
  that a flag pie passes has been renamed out from under it.
* **`diff` refuses to compare two different backends.** An agy-vs-codex pair is a caller
  bug, and the alternative — reporting every difference as drift — would produce an
  all-clear-shaped report from nonsense input.
* **A `--help` line that vanished is a finding too**, not only flags: prose is where a
  CLI announces a new consent or automation policy, which is the class that matters most.

### What stage B decided, where this section was silent

* **Matching is whole-word, so `c++` does not match `C++20`** and `ai` does not match
  `sailboat` or `said`. Stated because it is a real limitation a reader will meet: a
  prefix rule would fix `c++` and immediately make `ai` match `aix`. The documented rule
  is the one that does not produce false confidence, and stage C's report text must not
  imply exhaustive coverage.
* **A valid-but-empty feed is a stated error, not an empty digest** — the same call M5's
  `ocr` makes on an empty 200 (`ACCEPTANCE.md` §8). Its message distinguishes "contains no
  `<item>` or `<entry>`" from "answered with N items, none carrying a title", so a quiet
  source is diagnosable rather than invisible. An RSS feed lists recent posts, so a truly
  empty one is anomalous; treating it as "nothing happened" is how a broken fetch reads.
* **A config-shaped fetch failure keeps its kind** (`PieError::Config`, exit 2) instead of
  being flattened into a generic transport error (exit 1). A malformed URL is a
  configuration mistake, and the exit code is what a script sees.
* **The source's own name is not a haystack.** Scoring matches against the source name
  would make every item from a feed called "Rust Weekly" match the term `rust`, which is a
  score that measures the feed rather than the story.
* **The feed is parsed by hand, with no new dependency** (`Cargo.toml` is the
  maintainer's), and only ever slices at `str::find` boundaries — a byte slice at a
  multi-byte boundary is a panic on external input, which is invariant I6.

Three rules carry over, and each has already been paid for once in this project:

1. **The clock is a parameter.** `render_report` takes `month: &str`; nothing reads the
   system clock to decide a file name, so `watchtower-YYYY-MM.md` is testable at any
   date instead of only in the current month.
2. **A network failure is a stated error, never an empty success.** The same rule M5's
   `ocr` stage shipped (`ACCEPTANCE.md` §8): an unreachable source produces a report
   line naming the source and the failure, and the report says the digest is
   incomplete. A digest silently missing a source reads as "nothing happened".
3. **The vendor CLIs are probed, never configured.** Drift reads `--help` output; it
   does not read or write `~/.codex/config.toml` or `~/.claude/settings.json` (§10's
   static check applies here too).

### The REA half, stated as the degradation it is

`RISKS.md` §14: there is no disassembler on this machine, so the structural REA check
degrades to version + `--help` + binary-strings diff — "REA-lite". Full structural checks
become available only after `rea setup`. The report labels which mode produced it, so a
"REA-lite" report is never mistaken for a structural one.

**The third leg does not run, and the report says so.** `CliSnapshot::strings` has no
producer, and the obvious one is *wrong*: probing the resolved program for codex would run
`strings` over **node's** binary, because codex is `node <codex.js>` here — the same
trap `help_command` exists to avoid. A wrong comparison is worse than a missing one, so
stage D reported the leg as inert in the report body rather than filling the field with
something plausible. Version and `--help` **are** compared, which is the half that catches
renamed flags.

The missing producer has a shape, for whoever picks it up: extract strings from the
**vendor's own artefact** — the native binary for agy and claude, the `codex.js` script
for codex, both already resolvable through the launcher-prefix logic — and give
`Provider` a `strings_command` beside `version_command`/`help_command`, since it is the
same kind of probe with the same failure mode.

### CLI surface

```
pie watch [news|drift|report] [--dry-run] [--month YYYY-MM]
```

Opt-in: with the default config nothing is due and nothing is fetched.

---

## 18. claude backend + the tool loop (M9)

Roadmap: implement the claude adapter to spec; verify live when a subscription exists;
build the `tool_use` → `tool_result` loop, reused by the HTTP backends.

**This milestone cannot be verified live here.** `~/.claude/` exists but holds no
credential and no history (checked 2026-10-12), so there is no Claude subscription on
this machine. M9's verdict is therefore `offline-verified` and says so; it is never
`done`.

### claude is the only backend with a real system prompt

`knowledge/claude.md`: `--append-system-prompt <text>`. Every other backend folds the
projection in front of the prompt because it has no system role (agy) or no verified
system-prompt flag (codex) — see `Capabilities::system_prompt`, already `true` only for
claude. The bytes must stay identical; only the mechanism differs (`DESIGN.md` §10).

### The tool loop is the substance, and it inverts the delegate assumption

`RISKS.md` §1 is the reason M0–M8 exist in delegate mode: `agy --print` and
`codex exec` are one-shot, they run their own tools, and they cannot accept an injected
`tool_result`. claude in SDK mode can
(`-p --input-format stream-json --output-format stream-json`). So M9 is where pie
finally *owns* a tool loop, and it is the code path the M10 HTTP backends reuse
(`RISKS.md` §8) — one implementation, two consumers, not two implementations.

The loop is only correct if all four of these hold, and each is a stage check:

1. a `tool_use` block is matched to its `tool_result` by `tool_use_id`, and a result for
   an unknown id is a stated protocol error rather than a silently dropped frame;
2. the child's stdin is written without deadlocking on the stdout loop (**the classic
   two-pipe deadlock** — a synchronous write while the child blocks writing stdout);
3. the loop is **bounded**: a model that requests tools forever is stopped by an
   iteration cap, and the cap's effect is reported rather than swallowed;
4. the loop cannot run against a delegate-mode backend. `capabilities().tool_loop`
   gates it, so asking for the loop on agy or codex is a config error, not a hang.

### Nothing here spends quota

claude is `enabled = false` and unauthenticated, so no M9 stage may spawn a real
claude. The SDK protocol is exercised against a **scripted stand-in child** that emits
recorded `stream-json` frames and reads `tool_result` frames back — the same
stand-in pattern `tests/cache.rs` and `tests/process.rs` already use. A live test exists
and is `#[ignore]`d with a stated reason, ready for the day a subscription appears.

---

## 19. HTTP backends (M10)

Roadmap: re-enable OpenRouter and DeepSeek from `DEFERRED-BACKENDS.md`. The scope, the
wire shapes and the model lists live in that document and are not duplicated here.

Three decisions this milestone owes, because they are the ones that could silently
break an invariant that is already green:

1. **These are the first backends that legitimately use an API key.** I9
   ("subscription-only auth, never an API key") is an invariant about the *CLI*
   backends. The HTTP backends need the key in the request and must never be handed a
   vendor CLI's credential — and a missing key is a **stated** error, never a quiet
   fallback to a different backend, because "answered with a different subscription than
the one asked for" is the failure `resolve_target` was written to prevent.
2. **`cost_usd_micros` renders again only where it is real.** M4 removed the cost column
   because every subscription backend reports zero, and a permanent `$0.00` implies cost
   is measured when it is not (`TOKEN-ECONOMICS.md` §10c). That reasoning cuts the other
   way here: these backends must report a real cost or the column stays absent for them.
3. **The tool loop comes from M9, not from a second copy.** A backend that needs
   `tool_calls` and finds `tool_loop` unimplemented is an error, not a degraded path.

Not verifiable live without the user's own API keys: the transport is tested against a
recorded SSE corpus and a stand-in HTTP layer, and the verdict says which half ran.

