# Tests — what runs, what proves what

Four things can run a test. This file tells you which is which, so a green tick
means something specific.

| Runner | Scope | Cost | When |
|---|---|---|---|
| `cargo test` | everything except live smoke | free | after **every** code change |
| `bash scripts/qc.sh` | gates G1→G4 in order, fail-fast | free | before a milestone commit (Linux / WSL / macOS) |
| `powershell -File scripts/qc.ps1` | the same gates on Windows | free | before a milestone commit (Windows) |
| `bash scripts/qc.sh --live` | the above **plus G5** | **real subscription quota** | only at a milestone gate, deliberately |

Rules that keep this honest:

- A test may not spawn a vendor CLI unless it is `#[ignore]`d *and* gated on
  `PIE_LIVE=1`. `cargo test` therefore never spends quota and never needs the
  network, which is what makes it safe to run constantly.
- Every `#[ignore]` carries a reason string, so `cargo test -- --ignored` is
  self-explanatory rather than mysterious.
- No test touches the real `~/.pie`. Anything that writes state points `PIE_HOME`
  at a `tempfile::tempdir()`, which also lets the test *assert on what was
  written*.

## Test files

### `tests/process.rs` — the process layer, against real children

Spawning `cmd` / `sh`, never a vendor CLI. These exist because `src/process.rs`
carries three guarantees everything else leans on, and only a real spawn can
prove them.

| Test | What it proves |
|---|---|
| `stdout_lines_arrive_in_order_and_exit_zero_is_reported` | lines are delivered in order; exit 0 is reported as success |
| `a_non_zero_exit_is_reported_not_turned_into_an_error` | exit status is *data*, not an error (DESIGN §6 rule 7) |
| `stderr_is_captured_separately_from_stdout` | stderr never contaminates the protocol stream |
| `the_deadline_fires_and_kills_the_child` | a 400 ms deadline really does kill a 30 s child, and returns promptly |
| `scrubbed_env_keys_do_not_reach_the_child` | the env scrub removes the key — verified by dumping the child's whole environment, with a sanity check that the probe is visible *without* scrubbing |
| `a_missing_binary_is_a_spawn_error_not_a_panic` | a bad path is a typed error, not a panic |
| `non_utf8_bytes_on_stdout_do_not_panic` | invalid UTF-8 on stdout is survived |

### `tests/no_secrets.rs` — "no hardcoded keys, nothing persisted"

Two halves, because they fail in different ways.

**Static** — scans every `.rs` file under `src/` and `tests/`:

| Test | What it proves |
|---|---|
| `no_credential_shaped_literal_exists_in_the_tree` | no credential-shaped string literal exists. Two detections: known credential prefixes (`AIza`, `sk-ant-`, `ghp_`, `-----BEGIN`, …) and a "long opaque run containing a digit, an uppercase and a lowercase letter" heuristic. The few synthetic samples that exist to test *redaction* are listed in `ALLOWED_SAMPLES` with the reason each one is there, so an exemption is a reviewed decision rather than an accident |
| `api_key_variables_are_never_assigned_a_value_in_source` | none of `ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN`, `OPENAI_API_KEY`, `GOOGLE_API_KEY` is ever assigned a literal in source |

**Behavioural** — runs the real binary with API keys present in the environment:

| Test | What it proves |
|---|---|
| `auth_never_surfaces_or_persists_an_api_key` | `pie auth` prints no key and writes none |
| `doctor_never_surfaces_or_persists_an_api_key` | `pie doctor` prints no key and writes none |
| `usage_never_surfaces_or_persists_an_api_key` | `pie usage` prints no key and writes none |
| `the_legacy_auth_file_is_left_untouched` | `~/.pie/auth.json` — the earlier JS implementation's extracted OAuth tokens — is byte-identical after pie runs. pie must never read or rewrite it (`PLAN.md` §5) |

The behavioural tests use a distinctive probe value
(`pie-compliance-probe-…`), plant it in all four API-key variables, then assert
it appears in **no** output stream and in **no** file anywhere under `PIE_HOME`.
That is the strongest available statement of "nothing is persisted".

### `tests/live_smoke.rs` — gate G5, real CLIs, real quota

`#[ignore]`d, and additionally requires `PIE_LIVE=1`. Refuses to run a model that
is not in `APPROVED_MODELS` (`agy/gemini-3.8-flash`, `codex/gpt-6-luna`), so the
cost of a live gate cannot drift.

| Test | What it proves |
|---|---|
| `agy_turn_succeeds_and_is_recorded` | the real agy argv is accepted, the real stream matches the parsed contract, and exactly one session-log row is written with `ok: true` and non-zero input tokens |
| `agy_usage_probe_reports_buckets` | the `/usage` slash expansion works… and writes **no** session row, proving a probe is not a model turn |
| `codex_turn_succeeds_and_is_recorded` | as above for codex |
| `codex_usage_probe_reports_buckets` | the quota endpoint is reached and parsed, with no turn spent |
| `a_live_turn_does_not_leak_key_material` | a real turn's stdout and stderr contain no key-shaped material |

`assert_turn_recorded` also asserts the full frozen `usage.jsonl` schema, so a
backend that silently drops a field fails G5 rather than corrupting M4's totals.

### `tests/parse_agy.rs` · `tests/parse_codex.rs` — the stream parsers (M1 / M2)

Parser tests driven by recorded fixtures in `tests/fixtures/`, copied from the
verified samples in `knowledge/agy.md` and `knowledge/codex.md` — never invented.
They test the `EventRouter` implementations **directly**, with no process
involved, which is the reason `EventRouter` is a trait.

## Unit tests

Everything else lives beside the code it tests (`#[cfg(test)] mod tests`). The
notable ones:

- `src/config.rs` — precedence (flag → `PIE_*` env → file → default), malformed
  TOML, and the env-scrub deny-list.
- `src/jsonl.rs` — tolerant parsing: BOM, stray CR, half-written lines, unknown
  events.
- `src/process.rs` — secret redaction, including the regression for the
  `token=<blob>` leak.
- `src/errors.rs` — exit codes are stable.
- `src/provider/agy.rs` · `codex.rs` — the argv contracts, the prompt cap, and
  the "never spawn `codex.cmd`" rule.
- `src/session_log.rs` — the frozen row schema and append-only behaviour.

## Adding a test

1. Does it need a vendor CLI? It goes in `tests/live_smoke.rs`, `#[ignore]`d,
   `PIE_LIVE`-gated, approved models only.
2. Does it need to write state? Set `PIE_HOME` to a `tempfile::tempdir()`.
3. Does it need the environment? Take a lock: `crate::testing::EnvGuard` in unit
   tests, or a `tokio::sync::Mutex` in async integration tests. `std::env` is
   process-global and Rust runs tests in parallel — an unguarded mutation will
   fail someone else's test, which is exactly what happened once already
   (`ACCEPTANCE.md` §9, B5).
