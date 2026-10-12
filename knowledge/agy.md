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

`agy --print /usage --output-format json --print-timeout 0`
(slash-command expansion — do **not** pass `--disable-slash-commands`). Returns the
quota groups/buckets at **zero model tokens**.

**Verified 2026-10-12 on agy 1.3.3.** `num_turns: 0`, `duration_seconds: 0`, every
field of `usage` zero, and the quota payload under a `command` key — verbatim
capture in `tests/fixtures/agy_usage.json`:

```json
{"status":"SUCCESS","num_turns":0,"usage":{all zeros},
 "command":{"name":"usage","data":{
   "description":"Within each group, models share a weekly limit and a 5-hour limit...",
   "groups":[
     {"name":"Gemini Models","description":"Models within this group: Gemini Flash, Gemini Pro",
      "buckets":[
        {"id":"gemini-weekly","name":"Weekly Limit Remaining","window":"weekly",
         "remaining_fraction":0.884968638420105,"reset_time":"2026-10-15T01:32:32Z"},
        {"id":"gemini-5h","name":"Five Hour Limit Remaining","window":"5h",
         "remaining_fraction":0.9882727265357971,"reset_time":"2026-10-12T16:00:37Z"}]},
     {"name":"Claude and GPT models",
      "buckets":[
        {"id":"3p-weekly","window":"weekly","remaining_fraction":0,
         "reset_time":"2026-10-16T00:37:40Z"},
        {"id":"3p-5h","window":"5h","remaining_fraction":1}]}]}}}
```

Path: `command.data.groups[]` → each group's `buckets[]` →
`{id, name, description, window, remaining_fraction, reset_time}`. `reset_time`
is **absent** on a disabled bucket, so a missing value is normal, not an error.

Two details that matter for pie:

- **A bucket's group is required to identify it.** Both groups carry a bucket
  whose `window` is `weekly`, and both are named "Weekly Limit Remaining". A label
  built from the bucket alone renders two indistinguishable rows, so the group
  name must be part of it.
- **`--print-timeout 0` is required**, not a shorter timeout. With `30s` the
  expansion did not complete inside the window and the capture came back as an
  empty `response` with no payload; with `0` ("wait for the turn to complete") it
  returns immediately. Do not "protect" this probe with a short timeout — it is
  free, so waiting is the correct behaviour.

## Probing slash commands from git-bash: set `MSYS_NO_PATHCONV=1`

> **This cost a wrong design decision once. Read it.**

MSYS/git-bash rewrites an argument that begins with `/` into a Windows path
before the child process sees it. So this probe:

```sh
agy --print /usage --output-format json --print-timeout 0     # WRONG from git-bash
```

reaches agy with the literal prompt text `C:/Program Files/Git/usage`. agy then
treats it as a *prompt*, spends a real model turn (~20 700 input tokens), and
answers a question about how much disk space Git uses. Three separate probes
produced three different misleading results this way, and nearly concluded that
`/usage` was broken and expensive when it is in fact free and reliable.

Set `MSYS_NO_PATHCONV=1` for any manual probe from git-bash that passes a
leading-slash argument. This affects probes only: pie spawns `agy` through
`std::process::Command`, which performs no such rewriting, so pie's own argv was
always correct. Do not "fix" pie for a shell artifact.

## Usage / quota

## Resume, and cache-token accounting (measured 2026-10-12)

Measured through pie against agy 1.3.3, three turns on `gemini-3.8-flash`:

| Turn | `input_tokens` | `cache_read_tokens` | duration | answer |
|---|---|---|---|---|
| A fresh, seeds "remember 41" | 20 670 | 0 | 40.5 s | `ok` |
| B **resumed** via `--conversation <id>` | 22 937 | 18 619 | **41.2 s** | `41` |
| C fresh, same follow-up as B, no resume | 13 556 | **104 041** | 69.7 s | verbose |

**Two conclusions, both contradicting an assumption we were about to build on.**

### 1. Resume works, and does not make agy faster

B answered `41`, so `--conversation <id>` genuinely continues the conversation. But it
cost 41.2 s against A's 40.5 s: **no latency win at all.** agy's ~10 s per-invocation
initialisation plus inference dominates, and resume does not avoid the init because it
is still a fresh process. Resume buys *context continuity*, not speed — which M5 needs
for multi-turn work, but is not the answer to "agy is slow".

For latency the lever is backend choice: codex answers a full turn in ~3.6 s.

### 2. `cache_read_tokens` is not a subset of `input_tokens`

Row C reported more cached tokens than input tokens (104 041 against 13 556), because
agy accumulates cache reads across its own internal tool-loop steps rather than
reporting a single prompt's cached portion. codex, by contrast, uses OpenAI semantics
where `cached_input_tokens` *is* a subset of `input_tokens` (1 792 of 13 424).

Consequence: any "cache hit rate" of the form `cached / input` is wrong for agy and
can exceed 100% — it did, 767%. `report::cache_hit_rate` now divides by the larger of
the two, which is bounded and defensible under either reading, but **the semantics are
still not pinned per backend** and any token-efficiency metric built on this field
must say which backend it is talking about. See `TOKEN-ECONOMICS.md` section 9.

Row C is also not a clean control: it answered verbosely (857 output tokens), so its
duration and token counts are not comparable to B's terse `41`.

## Caveats

- `--model` must be paired with `--effort`.
- Windows command-line length limit → keep prompt ≤ ~20k chars; agy has no
  stdin prompt path, so truncate when necessary.
- `--conversation <id>` resumes an existing agy conversation (token-saving path).
