# Test fixtures — provenance

Every file in this directory is either a **verbatim capture** from this machine
or a sample whose **shape comes from a knowledge file**. Nothing is invented
free-hand: `planning/DESIGN.md` §11 requires fixtures to be copied from the
verified samples rather than made up. Each section below records the provenance
of its own files.

No fixture contains a credential. No test that reads these files makes a network
call or spawns a vendor CLI.

## agy (M1)

Every file here is either a **verbatim capture** from this machine or a sample
whose **shape comes from `knowledge/agy.md`** with the elided fields filled in.
Nothing is invented free-hand. `planning/DESIGN.md` §11 requires fixtures to be
copied from the verified samples rather than made up.

| File | Provenance |
|---|---|
| `agy_turn.jsonl` | Shapes are the three documented NDJSON events in `knowledge/agy.md` ("Output — stream-json NDJSON (verified live)"); the `…` elisions (`conversation_id`, the `tools` array, the result `usage`) are filled with the values the same document shows in its `step_update` sample (`input_tokens: 20704`, `output_tokens: 20`, `thinking_tokens: 19`, `cache_read_tokens: 0`, `total_tokens: 20724`). The two `step_type:"tool"` rows use the documented tool fields (`tool_name`, `tool_info`, `state: ACTIVE\|DONE`). |
| `agy_garbage.jsonl` | Constructed for invariant I5 (`planning/ACCEPTANCE.md` §2): one banner line, one unknown event kind, one non-JSON line and one blank line, surrounding a documented-valid `agent_response` + `result`. |
| `agy_error_result.jsonl` | Documented-valid `result` with `status: "ERROR"` and the `error` field the knowledge file says the terminal event carries. |
| `agy_cancelled_result.jsonl` | Documented-valid `result` with `status: "CANCELLED"` (one of the statuses `knowledge/agy.md` lists) and **no** `error` field, so the stderr fallback path is exercised. |
| `agy_truncated.jsonl` | Documented-valid `init` + `agent_response`, deliberately with no terminal `result` — the truncated-stream case. |
| `agy_models.txt` | **Verbatim live capture** of `agy models` on 2026-10-12 (zero-token probe, agy 1.3.3): 18 rows = 7 base models × their effort variants, behind a `Fetching available models...` banner. |
| `agy_usage.json` | **Verbatim live capture** of `agy --print /usage --output-format json --print-timeout 30s` on 2026-10-12 (zero-token probe, agy 1.3.3). Note: it returned the JSON envelope with an **empty `response`** and no quota payload at all — see the M1 report. |

### Observations recorded while capturing

* `agy models` prints a `Fetching available models...` banner on stdout before the
  rows, and its ids carry the effort suffix (`gemini-3.8-flash-high`) while the
  display name carries it as a parenthetical (`Gemini 3.8 Flash (High)`).
  `parse_models` groups the rows by base id and prefers the display's
  parenthetical, falling back to the id suffix; `the_recorded_catalog_agrees_with_itself_on_every_effort`
  asserts that the two encodings agree on every row of this capture, so
  preferring one over the other cannot drop or invent an effort.
* The effort sets are **per model and non-uniform**:
  * `gemini-3.8-flash`, `gemini-3.7-flash`, `gemini-3.6-flash` -> low, medium, high
  * `gemini-3.1-pro` -> low, high (**no medium**)
  * `claude-opus-5-5`, `claude-sonnet-5-5` -> low, medium, high
  * `gpt-oss-120b` -> **only medium**

  This capture is therefore the source of truth for `--effort` validation, not
  the documented vocabulary: `build_args` refuses an effort the served model does
  not offer, and substitutes the closest one when only the *configured default*
  is unsupported (`planning/RISKS.md` §7 — never spend more quota than asked).
  The documented list (`low|medium|high|xhigh|max`) is the fallback when no
  catalog is on disk.
* The `/usage` probe printed a partial-output warning on **stderr** and the
  result envelope on **stdout** after hitting its own `--print-timeout 30s`,
  i.e. the slash expansion did not return quota data in print mode on this
  machine. `parse_usage` therefore reports the buckets as **missing, not zero**,
  so an empty table cannot be misread as either "quota available" or "out of
  quota".

## codex (M2)

| File | Source | Notes |
|---|---|---|
| `codex_turn.jsonl` | `knowledge/codex.md` § "Output — JSONL (verified live)", verbatim | The four recorded lines of a live `codex exec --json` run. The recorded sample elides the real `thread_id` as `…`; a real-shaped id is substituted so the line stays parseable. |
| `codex_noisy.jsonl` | Composed from the same recorded lines plus event/item names read out of the shipped `codex.exe` string table (zero-token probe, `grep -a`) | Covers invariant I5: a non-JSON banner line, `item.started` / `item.updated`, a tool item, a `reasoning` item, and a future event type, all around a valid `turn.completed`. Item type names (`command_execution`, `reasoning`) and fields (`command`, `exit_code`, `status`, `aggregated_output`) are codex's own strings. The agent message uses the `content[].text` form seen in codex's on-disk rollout records, so both answer shapes are exercised across the two fixtures. |
| `models_cache.json` | `~/.codex/models_cache.json` on the development machine, trimmed | Same shape (`fetched_at`, `etag`, `client_version`, `identity`, `models[]`), three real models (`gpt-6-luna`, `gpt-5.5`, `memory4-dream`) with their real `default_reasoning_level` and effort lists, plus one deliberately slug-less entry to prove it is dropped. Large per-model fields (tool config, context windows) are omitted — the parser reads only `slug`, `display_name`, `default_reasoning_level`, `supported_reasoning_levels[].effort`. |

The quota-window shapes are inline in `tests/parse_codex.rs`, next to the test
that asserts on them: one is the shape documented in `knowledge/codex.md`, the
other is the single-window response actually observed from
`GET https://chatgpt.com/backend-api/wham/usage` on this machine.
