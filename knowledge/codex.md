# codex CLI — reverse-engineered

- Version: **codex-cli 0.162.0** (`@openai/codex`, npm)
- Binary shim: `C:\Users\amiru\AppData\Roaming\npm\codex` →
  `node_modules/@openai/codex/bin/codex.js` → native `codex.exe`.
- Kind: OpenAI Codex CLI, sub-locked (ChatGPT/Codex plan).

## Auth

`~/.codex/auth.json` (present on this machine). This is the **email-locked
ChatGPT/Codex subscription** credential, not a pay-as-you-go API key. Probe with
`codex login status`. The access token is used for the ChatGPT usage endpoint
(below); never echo it.

## Non-interactive exec

```
codex exec --cd <dir> \
    --approve-for-me \
    --skip-git-repo-check \
    --json \
    [--model <id>] \
    <prompt>
```

- `-C/--cd <dir>` sets the working root.
- `--approve-for-me` auto-approves within the workspace-write sandbox.
  **Mutually exclusive with `--sandbox`.** For full bypass there is
  `--dangerously-bypass-approvals-and-sandbox` (avoid).
- `--skip-git-repo-check` allows running outside a git repo.
- `--json` emits events as JSONL to stdout.
- `--model/-m`, `-c/--config key=value`, `--add-dir`, `--ephemeral`,
  `-o/--output-last-message <file>`, `--output-schema <file>`.
- No prompt arg → reads stdin; `-` also reads stdin.

## Output — JSONL (verified live)

One JSON object per line:

```jsonl
{"type":"thread.started","thread_id":"…"}
{"type":"turn.started"}
{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"OK"}}
{"type":"turn.completed","usage":{"input_tokens":13768,"cached_input_tokens":11008,"cache_write_input_tokens":0,"output_tokens":5,"reasoning_output_tokens":0}}
```

- Answer is `item.completed` → `item.type:"agent_message"` (or
  `"assistant_message"`) → `item.text`.
- Usage is the final `turn.completed` → `usage` (input/cached/output/reasoning).

## Subcommands

`exec/e`, `review`, `login`, `logout`, `agents`, `resume`, `fork`, `apply/a`,
`archive`, `delete`, `unarchive`, `queue`, `mcp`, `plugin`, `doctor`, `sandbox`,
`update`, `completion`, `app-server`, `remote-control`, `exec-server`, `cloud`,
`features`.

### Resume: exec options come **before** the `resume` subcommand

`resume` is a subcommand of `exec`, not a flag, so the `exec`-level options must
precede it. This was verified by probe (2026-10-12, codex-cli 0.162.0):

```
codex exec resume <id> --cd . --approve-for-me --json --help
  => error: unexpected argument '--cd' found

codex exec --cd . --approve-for-me --json resume <id> <prompt>   # correct order
```

pie got this wrong in M0 and it was fixed in M2 (bug B11, `ACCEPTANCE.md` §9).
Any instruction to "resume with `codex exec resume <id>`" omits the flag ordering
and produces an argv codex rejects. `--last` is the "most recent session"
variant, and lives in the same position.

### Reasoning effort is **not** a codex flag

`codex exec` has no `--effort` (confirmed against `codex exec --help`). Effort is
a config value, not an argv flag, so pie cannot select it the way it does for agy.
`src/provider/codex.rs` warns on stderr if an effort is requested rather than
inventing a `-c` key. A verified `-c` mapping is still an open question.

## Models

Discovered by codex from its backend (gpt-5.x / gpt-6.x). Seen locally:
`gpt-5.3-codex-spark`, `gpt-5.5`, `gpt-5.6-luna/sol/terra`, `gpt-6-astra/luna/
sol`, `gpt-6.1-sol`. `~/.codex/models_cache.json` holds a cache.

## Auth

`~/.codex/auth.json`. The real `auth_mode` value observed on this machine is
**`"chatgpt"`** (with `OPENAI_API_KEY: null`), not `"subscription"`. Any
classifier must therefore treat `chatgpt` as the subscription mode — `pie` does,
and it refuses an API-key-only credential rather than silently running in
pay-as-you-go mode.

## Usage / quota

ChatGPT subscription quota endpoint (not API-key):
`GET https://chatgpt.com/backend-api/wham/usage` with
`Authorization: Bearer <codex token>` + `ChatGPT-Account-Id: <account id>`
headers (token/account from `~/.codex/auth.json`).

**The "5h + weekly buckets" description is not what this account returns.**
Observed live 2026-10-12: **one 30-day window**, with `secondary_window: null` —

```
limit_window_seconds: 2592000   (30 days)
used_percent: 12
reset_at: 2026-11-08T01:52:52Z
```

and rendering as `30d  88.0%  2026-11-08T01:52:52Z`. So the window set is
account- and plan-dependent, and the `5h`/`weekly` pairing in `PLAN.md` §6 must
not be assumed. pie labels a bucket from its actual duration
(`limit_window_seconds`), falling back to `period`/`weekly`, and treats a null
`secondary_window` as normal. Token and account id are never logged.

## Caveats

- Windows `codex.cmd` shim cannot be spawned directly without a shell; launch
  `node <…>/@openai/codex/bin/codex.js` (or `codex.exe`) with hidden window.
- `--sandbox` and `--approve-for-me` are mutually exclusive.
- Transient 503s on the responses websocket occur; codex retries internally.
