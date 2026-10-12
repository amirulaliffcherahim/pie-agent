# agy CLI — reverse-engineered

- Version: **1.3.3** — observed 2026-10-12 by pie's `--list-backends` probe.
  Originally reverse-engineered at **1.3.2**. The flag surface and stream schema
  in this file are unchanged as far as the probe can tell; a full contract
  re-check is the monthly watchtower's job (M8, `ROADMAP.md`).
- Binary: `C:\Users\amiru\AppData\Local\agy\bin\agy`
- Kind: Google Antigravity CLI (Rust), sub-locked (no public API key).

## Auth

Stored and refreshed by agy itself (OAuth). No token file to read; treat auth as
"present" when `agy --print` returns a live `result` event instead of an auth
error. `AppData/Local/agy` contains only `bin/` — auth/config live in agy's own
location and are managed through its login flow.

## Print mode (one-shot turn)

```
agy --print <prompt> \
    --dangerously-skip-permissions \
    --disable-slash-commands \
    --output-format stream-json \
    --print-timeout <n>s \
    --add-dir <cwd> \
    [--model <id> --effort <e>]
```

- `--print` / `-p` / `--prompt` run one prompt and exit. `--print` consumes the
  next token as the prompt.
- `--output-format text|json|stream-json` (default `text`). Use `stream-json`
  for live text deltas + tool steps.
- **`--model` requires `--effort`** — omitting effort errors:
  `invalid model selection … requires --effort (available: low, medium, high)`.
  Effort set: `low|medium|high|xhigh|max`.
- `--print-timeout 0` waits until the turn completes (default).
- `--disable-slash-commands` prevents `/…` expansion in the prompt.
- `--dangerously-skip-permissions` auto-approves tools (headless otherwise
  denies prompts).

## Output — stream-json NDJSON (verified live)

Three event shapes, one JSON object per line:

```jsonl
{"event":"init","conversation_id":"…","init":{"cwd":"…","tools":["view_file","run_command",…],"permission_mode":"always-proceed"}}
{"event":"step_update","step_update":{"conversation_id":"…","step_index":1,"state":"DONE","step_type":"agent_response","text_delta":"OK\n","duration_seconds":2.15,"usage":{"input_tokens":20704,"output_tokens":20,"thinking_tokens":19,"cache_read_tokens":0,"total_tokens":20724}}}
{"event":"result","result":{"conversation_id":"…","status":"SUCCESS","response":"OK\n","duration_seconds":35.9,"num_turns":1,"usage":{…}}}
```

- `step_update` with `step_type:"agent_response"` carries `text_delta` (the
  streamed answer). Tool steps are `step_type:"tool"` with `tool_name`,
  `tool_info`, `state: ACTIVE|DONE|ERROR`.
- Terminal `result` carries `status` (`SUCCESS|OK|ERROR|FAILURE|CANCELLED|
  TIMEOUT`), the full `response`, `error`, and cumulative `usage`.
- `usage` fields: `input_tokens`, `output_tokens`, `thinking_tokens`,
  `cache_read_tokens`, `total_tokens`.

## Models

`agy models` prints the live catalog (id + name). Cache to disk with a TTL for
offline startup. Known ids seen locally: `gemini-3.8-flash`, `gemini-3.7-flash`,
`gemini-3.6-flash`, `gemini-3.1-pro`, `claude-opus-5-5`, `claude-sonnet-5-5`,
`gpt-oss-120b`.

## Usage / quota

`agy --print /usage --output-format json --print-timeout 30s`
(slash-command expansion — do **not** pass `--disable-slash-commands`). Returns
quota groups/buckets (5h + weekly, remaining fraction + reset time), zero model
tokens.

## Caveats

- `--model` must be paired with `--effort`.
- Windows command-line length limit → keep prompt ≤ ~20k chars; agy has no
  stdin prompt path, so truncate when necessary.
- `--conversation <id>` resumes an existing agy conversation (token-saving path).
