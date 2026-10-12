# Internals — how the three CLIs actually work

Deep structure of the three subscription CLIs: packaging, auth, config, session
storage, model catalogs, wire targets, and tool protocols. Sourced from on-disk
state, bundled type definitions, and binary string extraction (REA's native
disassembler was unavailable — no Hopper/Ghidra installed).

This is for **understanding** so pie can drive the CLIs correctly and detect
breakage. It is not a plan to reimplement the vendors' private APIs.

## Packaging & binaries

| CLI | Form | Size | Install |
|---|---|---|---|
| agy | native (Rust) | 190 MB | `~/.gemini/bin/agy.exe`, copy at `AppData/Local/agy/bin/agy.exe` |
| codex | native + node shim | — | npm `@openai/codex` → `bin/codex.js` → native `codex.exe` |
| claude | native (compiled) | 260 MB | npm `@anthropic-ai/claude-code` → `bin/claude.exe` (platform pkgs like `-win32-x64`) |

All three are native binaries now — no JS bundle to read (claude's `cli-wrapper.cjs`
is only a `--ignore-scripts` fallback launcher).

## Auth internals

- **agy** — home `~/.gemini/`. Google OAuth (`accounts.google.com/o/oauth2/auth`),
  tokens managed by agy. Env surface includes `GOOGLE_APPLICATION_CREDENTIALS`,
  `GOOGLE_API_KEY`, `CLOUD_CODE`. Do not use the API-key paths.
- **codex** — `~/.codex/auth.json`, keys: `auth_mode`, `last_refresh`,
  `tokens{access_token, id_token, refresh_token, account_id}`, `OPENAI_API_KEY`.
  `auth_mode` distinguishes **subscription** vs API-key; pie must require the
  subscription mode and never set `OPENAI_API_KEY`.
- **claude** — `.credentials.json` under the config dir
  (`ANTHROPIC_CONFIG_DIR`, default `~/.claude/`). Binary also exposes
  `ANTHROPIC_API_KEY` and `ANTHROPIC_AUTH_TOKEN`; **setting either switches to
  pay-as-you-go billing** and must be avoided. `~/.claude.json` is machine state
  (firstStartVersion, machineID, migrations) — not credentials. Not logged in on
  this machine (`~/.claude/` has only `ide/`).

## Config

- **agy** — `~/.gemini/config/config.json` (keys: `allow`, `autoExecutionPolicy`,
  `browserJsExecutionPolicy`, `enableTerminalSandbox`, `globalPermissionGrants`,
  `nonWorkspaceFileAccessPolicy`, `plugins`, `themeMode`, `userSettings`,
  `version`, …) and `~/.gemini/antigravity-cli/settings.json`
  (`model` e.g. `"Gemini 3.8 Flash (Medium)"`, `permissions.allow`,
  `trustedWorkspaces`, `artifactReviewPolicy`, `allowNonWorkspaceAccess`).
- **codex** — `~/.codex/config.toml` (minimal here: `[tui]`, `[windows] sandbox`).
- **claude** — `~/.claude.json` (machine state) + project `CLAUDE.md`. pie
  deliberately injects its own profile via `--append-system-prompt` instead.

## Sessions / resume

- **agy** — `~/.gemini/antigravity-cli/conversation_summaries.db` (SQLite) plus
  `history.jsonl` (`{display, timestamp, workspace, type}`). Resume with
  `--conversation <id>` / `--continue`.
- **codex** — `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl` (full transcripts),
  `session_index.jsonl` (`{id, thread_name, updated_at}`),
  `history.jsonl` (`{session_id, ts, text}`), and `state_*.sqlite`. Resume with
  `codex exec resume <id>` / `--last`.
- **claude** — per-project session transcripts under `~/.claude/`; resume with
  `--resume <session-id>` / `--continue`.

## Model catalogs

- **agy** — `agy models` (display names, e.g. "Gemini 3.8 Flash"). Settings store
  the display form with effort in parens: `"Gemini 3.8 Flash (Medium)"`.
  `--model` accepts the id (`gemini-3.8-flash`) with `--effort`.
- **codex** — `~/.codex/models_cache.json`:
  `{fetched_at, etag, client_version, identity, models:[{slug, display_name,
  description, default_reasoning_level, supported_reasoning_levels:[{effort,
  description}]}]}`. 7 entries incl. internal ones (`memory4-dream`,
  `gpt-reserve`, `codex-auto-review`) and user ones (`gpt-6-luna`,
  `gpt-5.6-terra`, `gpt-5.6-luna`, `gpt-5.5`). Efforts seen: `low|medium|high|
  xhigh|max|ultra`.
- **claude** — model aliases via `ANTHROPIC_DEFAULT_{HAIKU,SONNET,OPUS}_MODEL`;
  `--model` accepts an alias (`sonnet`, `opus`) or a full id.

## Usage / quota

- **agy** — `agy --print /usage --output-format json` (zero-token slash expansion).
- **codex** — ChatGPT `GET /backend-api/wham/usage` with the subscription token
  (`~/.codex/auth.json`), 5h + weekly buckets.
- **claude** — interactive `/usage`; print-mode support unverified → prefer
  per-turn `result.usage`.

## Wire targets (understanding only — do NOT reimplement)

- **agy** — Google OAuth + `aiplatform.googleapis.com` / `aicode.googleapis.com`
  (Cloud Code Assist), `antigravity-unleash.goog` (flags), a Cloud Run
  auto-updater. (This is the private-API surface that carries suspension risk;
  driving the CLI keeps us out of it.)
- **codex** — `chatgpt.com/backend-api/codex` (responses websocket/SSE).
- **claude** — `api.anthropic.com` (plus Vertex/Bedrock/Azure variants).

## Tool protocol (drives the pie tool-loop decision)

- **claude** — `sdk-tools.d.ts` defines ~44 tools (`Bash`, `FileRead/FileEdit/
  FileWrite`, `Glob`, `Grep`, `Agent`, `Task*`, `TodoWrite`, `WebFetch/WebSearch`,
  `Mcp`, `NotebookEdit`, `ExitPlanMode`, `EnterWorktree`, …). SDK mode
  (`--input-format stream-json` + `--output-format stream-json`) supports
  `tool_use` → `tool_result` round-trips → **pie can drive tools**. (Future-proof:
  no claude subscription on this machine, so this path stays unverified until one
  exists.)
- **agy** — internal tools (`view_file`, `grep_search`, `find_by_name`,
  `list_dir`, `run_command`, `subagent`, `browser_*`, …) run inside agy; print
  mode is one-shot (`--input-format stream-json` = one turn per stdin line) →
  **delegate mode**.
- **codex** — internal tools; `exec` is one-shot → **delegate mode**.

## Critical do-nots

1. Never set `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` / `GOOGLE_API_KEY` for these
   backends — it silently flips them to pay-as-you-go billing.
2. Require codex `auth_mode` = subscription; refuse API-key-only credentials.
3. Never call the vendors' private endpoints directly; only drive the CLI.
