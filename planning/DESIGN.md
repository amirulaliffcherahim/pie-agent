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
│   ├── agy-models.json       # { fetched_at, models:[…] }  TTL 24h
│   └── codex-models.json     # shaped from ~/.codex/models_cache.json
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
(`NoSubscription`). Do **not** scrub `GOOGLE_APPLICATION_CREDENTIALS`/`CLOUD_CODE`
— agy's own auth path may need them (`knowledge/internals.md`).

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
  --output-format json --print-timeout 30s` **without** `--disable-slash-commands`
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

### claude (M0 stub)

`binary()` resolves, `auth_status()` reports `NoSubscription`/`NeedsLogin` using
the `~/.claude/.credentials.json` + `~/.claude.json` facts from
`knowledge/claude.md`. `list_models()` returns a static catalog. `stream()`
returns `PieError::Unsupported`-style config error. `enabled = false` by default.

## 10. CLI surface for M0–M2

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

## 11. Test strategy

| Layer | What | Runs |
|---|---|---|
| Unit (inline `#[cfg(test)]`) | pure parsers fed recorded lines: agy event→`StreamEvent`, codex event→`StreamEvent`, usage mapping, argv builder, config precedence, env scrub, prompt cap, error→exit code | `cargo test` |
| Integration `tests/` | registry lists agy/codex/claude; config file+env+flag precedence; scrub removes exactly the deny-list; prompt cap fires at >20 000; tolerant parse survives garbage/unknown events | `cargo test` |
| Live smoke `tests/live_smoke.rs` | real `agy`/`codex` turns and usage probes | `#[ignore]`, only with `PIE_LIVE=1` — **spends subscription quota** |

Rules: no test spawns a real CLI unless `PIE_LIVE=1`. Fixtures live in
`tests/fixtures/*.jsonl` and are copied from the verified samples in
`knowledge/agy.md` / `knowledge/codex.md` — not invented. Every milestone gate
runs `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test`.

## 12. Explicitly deferred

M3 profile/memory projection · M4 unified auth/usage tables · M5 agent loop +
sessions + OCR MCP · M6 ratatui TUI · M7 learn · M8 watchtower · M9 claude +
tool-loop · M10 HTTP backends. `tool_loop` stays `false` for every M0–M2 backend;
the flag exists now so the loop can branch on it later (RISKS §1, §2).
