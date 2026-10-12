# claude CLI — reverse-engineered

- Version: **2.1.296 (Claude Code)** (`@anthropic-ai/claude-code`, npm)
- Binary shim: `C:\Users\amiru\AppData\Roaming\npm\claude`
- Kind: Anthropic Claude Code CLI, subscription-locked (Pro/Max).
- Status: **future-proofing only** — no Claude subscription on this machine.
  Adapter is implement-to-spec; verify live when a subscription exists. Must not
  block the critical path (v0 = agy + codex).

## Auth

`~/.claude/.credentials.json` (email-locked subscription OAuth). **API key is
never used** — pie drives the subscription login only. **Not logged in on this
machine** — `~/.claude` holds only an `ide/` dir and `~/.claude.json` is a
343-byte stub. Run `claude` (or `claude /login`) to authenticate before live
print-mode tests.

## Print mode (one-shot turn)

```
claude -p <prompt> \
    --output-format stream-json \
    --dangerously-skip-permissions \
    [--model <id>] \
    [--append-system-prompt <profile>] \
    [--add-dir <dir>]
```

- `-p/--print` runs non-interactively and exits.
- `--output-format text|json|stream-json` (print only; `stream-json` = realtime
  streaming). `--input-format text|stream-json` for SDK-style multi-message.
- `--append-system-prompt <text>` / `--append-system-prompt-file <path>` —
  pie's profile projection point.
- `--dangerously-skip-permissions` bypasses permission checks;
  `--allow-dangerously-skip-permissions` only *enables* it as an option.
- `--permission-mode acceptEdits|bypassPermissions|manual|…`;
  `--permission-prompts host|none`.
- `-c/--continue`, `--resume <session-id>`, `--session-id` for conversation
  resume (token-saving path).
- `--model`, `--fallback-model`, `--add-dir`, `--agents`, `--mcp-config`,
  `--settings`, `--json-schema`, `--bare`, `--autocompact`, `--bg/--background`.

## Output — stream-json (expected, not yet live-verified)

Claude Code stream-json is NDJSON; the well-known event envelope is:

```jsonl
{"type":"system","subtype":"init",…}
{"type":"assistant","message":{"content":[{"type":"text","text":"…"}]}}
{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{…}}]}}
{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"…","content":"…"}]}}
{"type":"result","subtype":"success","result":"…","usage":{…},"total_cost_usd":…}
```

Confirm exact field names with one live `claude -p "hi" --output-format
stream-json` run after auth is configured.

## Models

`claude` model ids like `claude-sonnet-4-6`, `claude-opus-4-6` (or the `sonnet`/
`opus` aliases); pass `--model` to pin one. No static list command needed —
pie can ship a small static catalog and let `--model` accept user ids.

## Usage / quota

Interactive `/usage` (or `/cost`) shows plan usage. Print-mode support is
unverified — if `/usage` cannot expand in `--print`, omit claude from `pie
usage` and rely on `result.usage` per turn.

## Caveats

- Not authenticated yet → live print-mode contract still to verify.
- `--append-system-prompt` is the single config surface pie uses; Claude Code's
  own `CLAUDE.md`/settings are deliberately not relied on.
- Windows shim: spawn hidden and prefer the npm `claude.cmd`/`claude.js` path
  (or native binary) without a visible console.
