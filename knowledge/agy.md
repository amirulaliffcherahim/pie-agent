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

`agy models` prints the live catalog: a `Fetching available models...` banner on
stdout, then `<id>` TAB `<display name>` rows. Cache to disk with a TTL for
offline startup.

**The id is `{base}-{effort}` — the effort is part of the id, not only a separate
flag.** Captured 2026-10-12 on agy 1.3.3; verbatim sample in
`tests/fixtures/agy_models.txt`:

```text
gemini-3.8-flash-high	Gemini 3.8 Flash (High)
gemini-3.8-flash-medium	Gemini 3.8 Flash (Medium)
gemini-3.8-flash-low	Gemini 3.8 Flash (Low)
gemini-3.1-pro-high	Gemini 3.1 Pro (High)
gemini-3.1-pro-low	Gemini 3.1 Pro (Low)
gpt-oss-120b-medium	GPT-OSS 120B (Medium)
```

**Effort support is per model and is not uniform.** From the captured catalog:

| base id | efforts offered |
|---|---|
| `gemini-3.8-flash` · `gemini-3.7-flash` · `gemini-3.6-flash` | low, medium, high |
| `gemini-3.1-pro` | low, high — **no `medium`** |
| `claude-opus-5-5` · `claude-sonnet-5-5` | low, medium, high |
| `gpt-oss-120b` | **only `medium`** |

Consequence for pie: `--effort` must be validated against the chosen model's
actual set, and a default effort must not be injected blindly — `gemini-3.1-pro`
offers no `medium`, so a blanket default of `medium` is an invalid combination for
it. Treat the catalog as the source of truth and a hardcoded list as a fallback
only. Note also that agy's own settings file stores the display form with the
effort in parentheses (see `internals.md`), which is what the parenthetical here
is.

The earlier note in this file listing base ids without an effort suffix
(`gemini-3.8-flash`, …) was written from a truncated view of the catalog; the
suffix is always present.

## Usage / quota

`agy --print /usage --output-format json --print-timeout 30s`
(slash-command expansion — do **not** pass `--disable-slash-commands`). Intended
to return quota groups/buckets (5h + weekly, remaining fraction + reset time) at
zero model tokens.

**Not reproducible on agy 1.3.3 as of 2026-10-12.** That exact command returned a
`result` envelope with `status:"SUCCESS"`, `num_turns:0`, `response:""` and an
all-zero `usage`, plus a partial-output warning on stderr — no quota payload at
all. Verbatim capture: `tests/fixtures/agy_usage.json`.

So on this version `pie usage agy` cannot obtain buckets this way. Requirements
that follow:

- "no buckets returned" must be surfaced as a **note**, never as an error and
  never as an empty table the user could read as "no quota left" or "all fine";
- the note must say that agy 1.3.3 returned no quota payload for `/usage` in print
  mode, so the buckets are *missing*, not zero;
- retry candidates for a later probe: `--output-format text`, dropping
  `--print-timeout`, or a non-`--print` invocation.

`PLAN.md` §6 assumes this worked; that assumption is now tracked as an open
question rather than a fact.

## Caveats

- `--model` must be paired with `--effort`.
- Windows command-line length limit → keep prompt ≤ ~20k chars; agy has no
  stdin prompt path, so truncate when necessary.
- `--conversation <id>` resumes an existing agy conversation (token-saving path).
