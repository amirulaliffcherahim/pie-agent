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

## Models

Discovered by codex from its backend (gpt-5.x / gpt-6.x). Seen locally:
`gpt-5.3-codex-spark`, `gpt-5.5`, `gpt-5.6-luna/sol/terra`, `gpt-6-astra/luna/
sol`, `gpt-6.1-sol`. `~/.codex/models_cache.json` holds a cache.

## Usage / quota

ChatGPT subscription quota endpoint (not API-key):
`GET https://chatgpt.com/backend-api/wham/usage` with
`Authorization: Bearer <codex token>` + `ChatGPT-Account-Id: <account id>`
headers (token/account from `~/.codex/auth.json`). Returns 5h + weekly buckets
and reset credits.

## Caveats

- Windows `codex.cmd` shim cannot be spawned directly without a shell; launch
  `node <…>/@openai/codex/bin/codex.js` (or `codex.exe`) with hidden window.
- `--sandbox` and `--approve-for-me` are mutually exclusive.
- Transient 503s on the responses websocket occur; codex retries internally.
