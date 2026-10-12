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
quota_probe   = "http"             # http | off — see the note below

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
  therefore gated: `codex.quota_probe = "http" | "off"` (env
  `PIE_CODEX_QUOTA_PROBE`). `codex` exposes no CLI subcommand for quota, so
  `PLAN.md` §6 specifies the endpoint — but a private-endpoint call must be
  switchable off without a code change. With `off`, `Probe` returns an empty
  report with a `notes` entry and makes no request; `pie usage codex` still
  prints session-log totals. See `knowledge/9router-findings.md` §0 and §4.

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

```rust
pub enum Input { Char(char), Enter, Esc, Backspace, Up, Down, Left, Right, Tab, Ctrl(char) }
pub enum Action { None, Quit, Submit, Cancel, ScrollUp, ScrollDown, OpenPicker,
                  PickerNext, PickerPrev, PickerAccept }
pub enum Status { Idle, Running, Done, Failed(String) }
pub enum Entry { User(String), Assistant(String), Tool { name: String, state: ToolState }, Notice(String) }

pub struct App { /* prompt, backend, model, session, status, transcript, tool_events,
                    scroll, picker, should_quit */ }

pub fn reduce(app: &mut App, input: Input) -> Action;
pub fn apply_event(app: &mut App, event: &StreamEvent);
pub fn render(frame: &mut Frame, app: &App);
pub async fn run_session(app: &mut App, cfg: &Config) -> Result<()>;   // the thin loop
```

`reduce` and `apply_event` take and return only pie's own types. `Frame` is ratatui's
and appears in exactly one signature.

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
