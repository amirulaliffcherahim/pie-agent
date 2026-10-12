# pie — pi rewritten in Rust

`pie` is a Rust reimplementation of the pi coding-agent loop, rebuilt around one
goal: **combine the agy, codex, and claude CLIs into one agent** so
subscription-locked proprietary plans are usable from a single place with a
single profile.

> **Safety note.** The technique is *driving the official CLIs headlessly*
> (`agy --print`, `codex exec --json`, `claude -p`). That uses each CLI's own
> auth and quota. We deliberately do **not** extract OAuth tokens or reimplement
> the vendors' private APIs — that is the pattern that triggered Google's Feb
> 2026 account-suspension sweeps. Treat pie as a personal/experimental tool and
> stay within your own subscriptions.

## 1. Scope

**Active backends (the proprietary-sub combination):**

| Backend | Access | Auth | AI call | Usage |
|---|---|---|---|---|
| agy CLI | sub-locked | agy stored auth | `agy --print` (stream-json) | `agy --print /usage --json` |
| codex CLI | sub-locked | `~/.codex/auth.json` | `codex exec --json` | ChatGPT `/wham/usage` |
| claude CLI | sub-locked | `claude` stored auth | `claude -p` (stream-json) | `/usage` if supported |

**claude is future-proofing only** — no Claude subscription on this machine yet.
Implement the adapter to spec and verify live when a subscription exists; it must
never block the critical path. **The live v0 path is agy + codex.**

**Commented out (re-enable by removing a comment, not deleted):**

- OpenRouter (`OPENROUTER_API_KEY`, OpenAI-compat SSE) — plain API-key fallback.
- DeepSeek (`DEEPSEEK_API_KEY`, OpenAI-compat SSE) — cheap API fallback.

**Removed:** every other pi provider (OpenAI, Anthropic, Google, Mistral,
Bedrock, Azure, …) — commented or deleted so only the five above remain.

## 2. Architecture

Single async (tokio) binary in `src/`:

```
pie/
├── Cargo.toml
├── src/
│   ├── main.rs            # clap args, dispatch, print/tui modes
│   ├── config.rs          # ~/.pie/config.toml + env + CLI overrides
│   ├── profile.rs         # ONE persona/rules/memory → projected system prompt
│   ├── memory.rs          # user model + durable memory (know-me target)
│   ├── session.rs         # session JSONL (mirrors pi's shape)
│   ├── schedule.rs        # daily/monthly background job runner
│   ├── learn.rs           # daily "know me": mine sessions → profile/memory
│   ├── watch.rs           # monthly watchtower (parent module - no mod.rs; DESIGN §2)
│   ├── watch/
│   │   ├── news.rs        # AI news digest (sources + impact ranking)
│   │   └── drift.rs       # CLI drift check (version/help/strings; REA opt-in)
│   ├── agent.rs           # the loop: prompt -> tools -> stream -> persist
│   ├── agent/
│   │   ├── tools.rs       # read, bash, edit, write, grep, find, ls
│   │   ├── ocr.rs         # file -> OCR text via itsmini (Baidu), cached
│   │   └── system.rs      # minimal system prompt (tools + short rules)
│   ├── mcp_ocr.rs         # MCP server exposing `ocr` -> itsmini (all CLIs)
│   ├── provider.rs        # Provider trait + registry + selection
│   ├── provider/
│   │   ├── driver.rs      # shared subprocess + NDJSON/JSONL streaming
│   │   ├── agy.rs
│   │   ├── codex.rs
│   │   ├── claude.rs
│   │   ├── openrouter.rs  # COMMENTED OUT
│   │   └── deepseek.rs    # COMMENTED OUT
│   ├── auth.rs            # auth state per backend + login hints
│   ├── usage.rs           # quota snapshot per backend + unified report
│   └── tui/               # ratatui (M7)
└── PLAN.md · ROADMAP.md
```

## 3. Provider trait

```rust
#[async_trait]
pub trait Provider: Send + Sync {
    fn id(&self) -> &'static str;   // "agy" | "codex" | "claude"
    async fn auth_status(&self, cfg: &Config) -> AuthStatus;
    async fn list_models(&self, cfg: &Config) -> Result<Vec<Model>>;
    async fn stream(&self, cfg: &Config, req: &ChatRequest,
        on: &mut dyn FnMut(StreamEvent) -> Result<()>) -> Result<Usage>;
    async fn usage(&self, cfg: &Config) -> Result<UsageReport>;
}
```

Helpers: `cli::stream_jsonl()` (hidden spawn, line-parse, timeout + kill-tree)
for all three CLIs; `http::stream_sse()` reserved for the commented API backends.

## 4. CLI contracts

- **agy** (verified): `agy --print <prompt> --dangerously-skip-permissions
  --disable-slash-commands --output-format stream-json --print-timeout <n>s
  --add-dir <cwd> [--model <id> --effort <e>]`. NDJSON `init` → `step_update`
  (`agent_response` → `text_delta`) → `result` (`status`,`response`,`usage`).
  `--model` requires `--effort`; prompt must be bounded (no ENAMETOOLONG).
- **codex** (verified): `codex exec --cd <cwd> --approve-for-me
  --skip-git-repo-check --json [--model <id>] <prompt>`. JSONL ends in
  `item.completed` → `item.type:"agent_message"` → `item.text`.
- **claude** (verify on install): `claude -p <prompt> --output-format
  stream-json --verbose --dangerously-skip-permissions`. NDJSON
  `system/init` → `assistant` (text + tool_use) → `result`.

## 5. Auth & subscription

- All three CLIs are **email-locked subscriptions** (Claude Pro/Max, ChatGPT/Codex,
  Antigravity plan) — not pay-as-you-go API keys. pie authenticates through each
  CLI's own login/OAuth and **never injects an API key** for them.
- Don't store tokens. Probe each CLI (`codex login status`, `claude auth
  status`, agy auth probe) and show ready / needs-login + a login hint.
- `pie auth` → a table of all three backends.

## 6. Usage / quota

- agy: `agy --print /usage --output-format json --print-timeout 30s`.
- codex: ChatGPT `GET /wham/usage` (token from codex auth) — 5h/weekly buckets.
- claude: `/usage` if print mode supports it, else omit.
- `pie usage` → unified table + pie's own session-log token/cost totals
  (weekly/monthly tabs).

## 7. Model catalog

- agy: `agy models` → cache with TTL. codex/claude: catalog + cache.
- `pie models --refresh` forces live refresh; selection is `provider/model`.

## 8. Unified profile, memory & routing

One config, one memory, one persona across all three CLIs — no per-CLI training.

- `~/.pie/profile.toml` + `~/.pie/AGENTS.md` + `~/.pie/memory/*.md` are the
  single source of truth. The vendors' own configs (`.claude/settings.json`,
  `~/.codex/config.toml`, agy agents) are ignored.
- `profile.rs` builds the system prompt once, then projects it:
  claude → `--append-system-prompt` (verify), codex → `-c`/injected `AGENTS.md`,
  agy → folded into the user prompt (no system role).
- Backend switch mid-session re-bootstraps from pie's transcript.
- Optional `pie route <task>` picks the cheapest/highest-quota sub; persona stays
  identical.

## 9. Performance & token efficiency (non-negotiable)

- tokio async; streaming renders as it arrives; hidden subprocesses (no popups).
- Stall watchdog + kill-tree + one resume retry; cold-spawn per turn, then
  resume (`--conversation`/`--resume`) to reuse the vendor's cached prefix.
- Lazy loading: no auth/model probe at startup; catalogs disk-cached.
- Minimal system prompt; resume-over-resend; prefix cache; compaction; tool
  output truncation; token totals from stream events (never re-read sessions).

## 10. Agent loop, tools & OCR

- **v0 is delegate mode** (agy + codex): a task goes to the CLI whole, the CLI
  runs its own tools, and pie returns the final answer. No pie tool cards.
- **tool-loop** (pie's `read/bash/edit/write` driving `tool_use`→`tool_result`)
  exists only for claude SDK mode (future-proof) and the commented HTTP
  backends. Build/test it in that milestone, not earlier.
- Session JSONL persist/resume, `/model`, `/usage`, `/reset`.
- **OCR tool — itsmini home server (Baidu OCR)** — `ocr <file>` turns an
  image/scanned-PDF into text, cached at `~/.pie/ocr-cache/`. Token win: ~1–2k
  vision tokens → ~200–500 text tokens.
  - **Reachability:** in delegate mode agy/codex run their *own* tools, so a
    pie-native tool never reaches them. Expose `ocr` as an **MCP server**
    instead — all three CLIs speak MCP (agy `mcp` + `~/.gemini/config/
    mcp_config.json`, codex `mcp` + `config.toml`, claude `--mcp-config`) — so
    every model gets `ocr` uniformly. pie hosts a tiny MCP server (stdio/HTTP)
    that forwards to itsmini; the tool-loop path (claude/HTTP) reuses it.
  - itsmini contract: `POST <itsmini>/ocr` `{ file, lang?, output }` →
    `{ text, pages, confidence }`. (itsmini = separate home-server project.)
  - Also a plain `pie ocr <file>` command for manual use.

## 11. Automation (background tasks)

Two scheduled jobs keep pie current and personalized. Both are opt-in, have a
budget, and write reviewable artifacts — never silent.

### 11a. Monthly watchtower (tech watch + CLI drift)
Keep pie working against agy/codex/claude and keep you informed without searching.

- **AI news scan:** configured sources (vendor changelogs/blogs, Hacker News,
  arXiv, RSS) → a short digest ranked by impact on *your* stack.
- **CLI drift check:** for each CLI snapshot version + `--help` + key binary
  strings; diff vs the last snapshot to spot changed flags, output schema, or new
  automation/anti-scripting policy. With a real disassembler (`rea setup` →
  Ghidra/Hopper) add REA structural checks; otherwise degrade to version+help+
  strings ("REA-lite").
- **Compliance framing:** exists so pie *adapts and stays within policy* — it
  flags when a change means pie must change behavior or stop, never how to evade a
  safety measure.
- **Output:** `~/.pie/reports/watchtower-YYYY-MM.md` + in-app notification.
  Monthly, configurable, network-only (no model tokens unless you ask for a digest).

### 11b. Daily "know me" (personalization)
pie adapts to how you work and never forgets past interactions.

- **Mine sessions:** pie's sessions, plus opt-in read-only import of the CLI
  stores — agy `~/.gemini/antigravity-cli/{history.jsonl, conversation_summaries.db}`,
  codex `~/.codex/{history.jsonl, session_index.jsonl, sessions/**}`, claude
  `~/.claude/**`.
- **Extract:** recurring requests, preferred stacks/conventions, tool habits,
  terminology, explicit preferences → update `~/.pie/profile.toml`,
  `~/.pie/AGENTS.md`, `~/.pie/memory/*.md` (append + consolidate).
- **Memory model:** combines pi's session JSONL, codex's memories/history SQLite,
  claude's `CLAUDE.md` style, and agy's `userSettings`/summaries.
- **Consolidate:** dedupe/merge, decay stale facts, keep provenance (source
  session), stay reviewable and revertible.
- **Budget:** cheap/local model by default — never silently spend a premium sub's
  quota. Daily, configurable, skippable.
- **Privacy:** fully local; raw transcripts never leave the machine.

## 12. Risks

- ToS/automation: CLI-driving is the safer path but still gray; personal use only.
- Protocol fragility: pin known-good CLI versions, parse tolerantly.
- Windows argv limit: bound prompts (≤ ~20k) or stdin.
- claude not installed here yet: M3 installs + verifies flags.

See `ROADMAP.md` for the milestone sequence.
