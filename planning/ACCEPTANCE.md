# pie — Acceptance & QC gates

How each milestone is judged "done". Criteria are **frozen before implementation**
and may only change by editing this file in the same commit that changes behavior.
`ROADMAP.md` says what to build; this file says how we prove it was built.

Scope: **M0–M2**. Milestones M3+ get their own sections when they start.

## 1. The five gates

Every milestone must pass all five before it is committed as complete.

| Gate | Command | Pass condition |
|---|---|---|
| G1 Format | `cargo fmt --check` | no diff |
| G2 Lint | `cargo clippy --all-targets -- -D warnings` | zero warnings (warnings are errors) |
| G3 Tests | `cargo test` | all pass, 0 ignored-without-reason |
| G4 Offline acceptance | the milestone's `Acceptance` commands below | exact expected output |
| G5 Live acceptance | the milestone's `Live` commands below | exact expected shape, real CLI |
| G6 Cross-target check | `cargo check --target x86_64-unknown-linux-gnu --all-targets --no-default-features` | compiles |

G5 spends real subscription quota. Approved models only: **`agy/gemini-3.8-flash`**
and **`codex/gpt-6-luna`** — do not substitute a pricier model. G5 is skippable
only by explicit user decision, and a skipped G5 marks the milestone
`offline-verified` in the run log rather than `done`.

G6 exists because the requirement is **Windows + WSL + Linux**. WSL2 is
installed on the development machine (`FedoraLinux-44`) but cannot boot
(`HCS_E_SERVICE_NOT_AVAILABLE` — hypervisor feature unavailable), so Linux
cannot be verified locally by running tests. G6 catches `cfg`-gated compile
errors without linking; the GitHub Actions matrix is the real Linux evidence.
See `PLAN.md` companion `PLATFORMS.md` for the per-platform support matrix.

**G6's command was wrong until M6, and it had no runner.** Two separate defects,
found together while wiring M6's dependencies:

1. **`--no-default-features` is required, not optional.** As originally written the
   command fails locally with `failed to find tool "x86_64-linux-gnu-gcc"`: the
   default `http` feature pulls `reqwest` → `rustls` → `ring`, which needs a C
   compiler for the target, and this machine has no Linux cross toolchain. The
   documented gate therefore **could not pass as written.** Same class as B12 — a
   criterion derived from a document instead of from running the command it
   names.
2. **Nothing ran it.** Every run-log row reports G6 green, but neither
   `scripts/qc.ps1` nor `scripts/qc.sh` invoked it; the G6 column rested on a
   hand-run command and a claim, with nothing able to contradict it. That is
   B20's shape ("a gate ran but could not fail") pointing the other way: a gate
   that could not run.

Both runners now invoke G6, and both **SKIP it loudly and count it separately**
when the target is not installed, rather than passing silently — a silent skip is
how B20 happened. The consequence of (1) is stated rather than hidden:
`--no-default-features` means G6 does **not** cover `cfg(feature = "http")` code
for Linux. CI's ubuntu job compiles and runs it natively and is the stronger
evidence; G6 is the local pre-flight that catches those errors before a CI round
trip.

G1–G3 are the standing QC loop: run them after **every** code change, not only at
the end of a milestone. `scripts/qc.sh` (unix/WSL) and `scripts/qc.ps1` (Windows)
run G1–G3 in order and stop at the first failure.

### Invoke the gates so a failure actually blocks the commit

Two of the bugs in §9 (B7, B9) are the same defect: the gate ran, but nothing was
wired to *stop* on it. Both are easy to repeat, so the rule is explicit.

- **Use the gate runners.** `scripts/qc.ps1` and `scripts/qc.sh` are fail-fast and
  exit non-zero. An ad-hoc chain is not equivalent.
- **Never pipe a gate into something that discards its status.** `cargo clippy … |
  grep … ; git commit` always commits: the pipe makes the exit status that of
  `grep`, and `;` runs the next command regardless. That is exactly how B9 landed.
- **Stage before gating.** `cargo fmt --all` rewrites files it was not asked
  about; if you then stage a subset and commit, the reformat is left behind and the
  *committed* tree is red even though your working tree was green. That is B7.
- **Prove a red tree at the commit, not in the working tree.** `git stash` and
  re-run the failing gate, as B7 was diagnosed, before concluding which revision
  is broken.

## 2. Global invariants (checked at every gate, every milestone)

These are cross-cutting gates from `ROADMAP.md`. Each has an assertion somewhere:

| # | Invariant | Where proven |
|---|---|---|
| I1 | No console window ever appears from a child process | `process.rs` sets `CREATE_NO_WINDOW` on Windows (`creation_flags`); on unix the child leads its own process group instead. Unit test asserts the flag constant |
| I2 | API-key env vars never reach a child | `env_hygiene` test: the deny-list (including `GOOGLE_APPLICATION_CREDENTIALS`) is removed from every child; `CLOUD_CODE`, which is a mode selector rather than a credential, is preserved |
| I3 | No secret is ever logged or echoed | `process::redact` + `redact` regression tests (long opaque runs, `Bearer <token>`); codex token redaction test; a `token=<blob>` leak was a real M0 bug (see §9) |
| I4 | Prompt argv is bounded | `prompt_cap` test: >20 000 chars for agy ⇒ `PromptTooLong`, exit 2 |
| I5 | A bad stdout line never aborts a run | tolerant-parse test: padding garbage + unknown events around a valid `result` still succeeds |
| I6 | No panic on external input | `#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic))]` in `lib.rs` |
| I7 | `models`/`auth`/`doctor`/`--list-backends` spend zero model quota | `auth` runs no subprocess; `models`/`usage` never call `stream()`; verified by inspection of those paths |
| I8 | Backend version is recorded for drift detection | `doctor` prints each CLI's own version and writes `~/.pie/cache/versions.json`; codex's row must name codex, not node (real M0 bug, see §9) |
| I9 | Subscription-only auth, never an API key | codex `auth_mode` gate + no key injection; `NoSubscription` error path exists |
| I10 | No credential is hardcoded, printed, or persisted by pie | `tests/no_secrets.rs`: a static scan for credential-shaped literals plus a behavioural run of the real binary with API keys in the environment, asserting they appear in no output and in no file under `PIE_HOME` |
| I11 | The legacy `~/.pie/auth.json` (extracted OAuth tokens) is never read, written, or recreated | `tests/no_secrets.rs::the_legacy_auth_file_is_left_untouched` and a `session_log` unit test |
| I12 | **No test passes only because this machine has vendor state** | **`G3d no vendor state`**: the entire suite is run again with `HOME`/`USERPROFILE` redirected to an empty directory, because that is what a CI runner is. Each test must establish the state it asserts about — `tests/parse_agy.rs`'s `PinHome` installs a scratch `~/.gemini` for exactly this reason. Added after **B32**: four agy tests were green locally and red in CI, because the developer's real home satisfied a subscription gate the tests never mentioned |

## 3. M0 — Scaffold + provider trait

**Acceptance**

```
cargo run -- --list-backends
```

A header row, then exactly three backend rows in this order. Captured
2026-10-12:

```
BACKEND   BINARY                             VERSION       AUTH            DETAIL
agy       agy\bin\agy.exe                    1.3.3         ready
codex     C:\Program Files\nodejs\node.exe   codex-cli 0.1…[truncated]  ready
claude    Roaming\npm\claude.cmd             2.1.296 (Clau…[truncated]  no-subscription
```

Exact column widths vary; the assertions are:

- `claude` must appear even though it is disabled — it is future-proofing.
- No API/HTTP backend was in the registry **until M10 built two**. This bullet required
  their absence, because a table row for a backend that cannot answer is a promise the
  binary does not keep; M10 implemented both, so the requirement inverts and
  `tests/acceptance.rs` now asserts that they **appear**, report `needs-login`, name their
  key variable, and are not shown as a *missing binary*. The old assertion was replaced with
  its inverse rather than deleted, so the pair records the reversal.
- Every backend resolves to a path and reports an auth state, or reports
  `binary-missing` — never a panic, never a hang, never an empty table. This is
  what makes the CI run meaningful: a CI runner has **no** vendor CLI installed,
  so CI exercises the degraded path for all three rows.
- The `VERSION` column must name the backend's own version. codex resolving via
  `node` must report `codex-cli …`, not node's version — that was bug B3.

`agy` reporting `1.3.3` while `knowledge/agy.md` documented `1.3.2` is a CLI-drift
observation (F1), not a failure: recording it is the point of invariant I8.

**Checks**

- [ ] `Provider` is object-safe and the registry returns `Vec<Box<dyn Provider>>`
- [ ] `Capabilities` reports `tool_loop = false` for agy and codex
- [ ] `agy` reports `stdin_prompt = false`, `max_prompt_argv = 20000`
- [ ] `~/.pie/config.toml` missing ⇒ defaults, no error; malformed ⇒ `PieError::Config`, exit 2
- [ ] precedence test: flag beats `PIE_*` env beats `config.toml` beats default
- [ ] no `unwrap`/`expect` outside tests (I6)
- [ ] no commented-out source files anywhere under `src/` — deferred designs live
      in `planning/DEFERRED-BACKENDS.md`, and the M10 backends are absent from the
      module tree and from `provider::registry()`
- [ ] no `mod.rs` files; `provider.rs` sits beside `provider/`
- [ ] `main.rs` is under ~60 lines and holds no business logic
- [ ] no credential-shaped literal anywhere: `cargo test --test no_secrets`
- [ ] running the binary with API keys in the environment writes none of them to
      `~/.pie` and prints none of them (`tests/no_secrets.rs`, behavioural half)

**Live:** none. M0 makes no model calls.

## 4. M1 — agy backend (delegate) — LIVE

**Acceptance (offline, fixtures from `knowledge/agy.md`)**

```
cargo test --test parse_agy
```
- `init` line ⇒ `StreamEvent::Init` with `conversation_id` + tools
- `step_update` / `agent_response` ⇒ `TextDelta("OK\n")`
- `step_update` / `tool` ⇒ `ToolStep { state: Active|Done|Error }`
- `result` line ⇒ `Done { status: "SUCCESS" }` + `TurnOutcome`
- usage mapping: `thinking_tokens` → `thinking_tokens`, `cache_read_tokens` → `cache_read_tokens`
- `status: "ERROR"` ⇒ `PieError::Backend`, exit 1
- garbage line + unknown event around a valid `result` ⇒ still succeeds (I5)

**Live**

```
cargo run -- -p --model agy/gemini-3.8-flash "hi"
cargo run -- usage agy
```

- the `hi` turn prints a short reply and exits 0
- one row is appended to `~/.pie/usage.jsonl` with `provider:"agy"`,
  `model:"gemini-3.8-flash"`, `ok:true`, non-zero `input_tokens` (§7 schema of DESIGN.md)
- `usage agy` prints quota buckets with remaining fraction + reset time
- `usage agy` prints **no** session-log row: it is a zero-token slash expansion
  (`num_turns: 0`, all usage fields zero), so it must never be recorded as a turn

**Effort and the model catalog** (revised 2026-10-12, after live evidence — see §9 B10)

- The catalog id is `{base}-{effort}` and effort support is **per model and not
  uniform**: `gemini-3.8/3.7/3.6-flash` offer low|medium|high, `gemini-3.1-pro`
  offers low|high with no medium, `gpt-oss-120b` offers only medium
  (`knowledge/agy.md` Models).
- `--model` without `--effort` takes the configured default when the model offers
  it; when it does not, the closest supported effort is substituted and announced
  on stderr — it must never build an argv agy will reject.
- an **explicit** `--effort` the model does not offer ⇒ `PieError::Config` (exit 2)
  naming that model's valid set
- the catalog is the source of truth; any hardcoded list is a fallback for when
  the catalog is unavailable, and must be documented as such

**Checks**

- [ ] `--disable-slash-commands` present for turns, **absent** for `/usage` (deliberate asymmetry)
- [ ] `--print-timeout` passed for both; turns may use `0`, and the `/usage` probe
      **must** use `0` — with `30s` the expansion returned an empty payload (B10)
- [ ] model catalog read from `agy models` and cached with 24h TTL; a cache hit makes no CLI call
- [ ] catalog rows are grouped by base id, one `Model` per base, carrying that model's own effort set
- [ ] `usage` buckets carry their **group** name in the label: both groups contain a
      `weekly` bucket named "Weekly Limit Remaining", so a bucket-only label is ambiguous
- [ ] a missing `reset_time` (a disabled bucket) becomes `None`, not an error
- [ ] hidden spawn — no console flash on the live run (I1)
- [ ] timeout ⇒ process tree killed, `PieError::Timeout`, exit 5 (test with a tiny timeout)
- [ ] a manual `/usage` probe from git-bash must set `MSYS_NO_PATHCONV=1`, or MSYS
      rewrites `/usage` into a Windows path and the probe answers a question about
      Git instead (B10). pie's own spawn path is unaffected

## 5. M2 — codex backend (delegate) — LIVE

**Acceptance (offline, fixtures from `knowledge/codex.md`)**

```
cargo test --test parse_codex
```
- `thread.started` ⇒ `Init { conversation_id: Some(thread_id) }`
- `turn.started` ⇒ no user-visible event (or `Other`)
- `item.completed` + `agent_message` ⇒ `TextDelta`
- `turn.completed` ⇒ `Usage` + `Done`
- `cached_input_tokens` → `cache_read_tokens`; `reasoning_output_tokens` → `thinking_tokens`
- non-agent item types (reasoning, tool calls) ⇒ `ToolStep`/`Other`, never `TextDelta`

**Live**

```
cargo run -- -p --model codex/gpt-6-luna "hi"
cargo run -- usage codex
```

- `hi` turn prints a reply, exits 0, appends a `provider:"codex"` row to the session log
- `usage codex` prints the quota window(s) from `wham/usage` when the probe is
  opted in (`codex.quota_probe = "http"`); with the default `"off"` it prints the
  disabled report and session-log totals instead
- the token appears in **no** output, error, or log line (I3 — grep the captured output)
- refusal path: with an API-key-only credential the backend reports
  `NoSubscription`, exit 3, instead of silently billing (I9)

**Revised against live evidence, 2026-10-12** (see §9 B11/B12)

- **The window set is not "5h + weekly".** This account returns a single
  **30-day** window with `secondary_window: null`
  (`limit_window_seconds: 2592000`, 12% used, reset `2026-11-08T01:52:52Z`), which
  renders as `30d  88.0%  2026-11-08T01:52:52Z`. The criterion is therefore: **a
  bucket per window the endpoint actually returns**, labelled from its real
  duration, with a null `secondary_window` treated as normal — not a fixed pair.
  `PLAN.md` §6's "5h/weekly buckets" must not be assumed.
- **The real `auth_mode` is `"chatgpt"`**, not `"subscription"`. The classifier
  must accept it as the subscription mode.
- **codex has no `--effort` flag.** Effort is a config value; requesting an effort
  must produce a clear warning rather than an invented `-c` key.
- **Resume ordering:** `exec`-level options precede the `resume` subcommand —
  `codex exec <flags> resume <id> <prompt>`. The reverse order is rejected with
  `unexpected argument '--cd' found`.

**Checks**

- [ ] never spawns `codex.cmd`; resolves `node <…>/codex/bin/codex.js` (or native `codex.exe`) (DESIGN §9)
- [ ] `--approve-for-me` and `--sandbox` are never both passed (they are mutually exclusive)
- [ ] `--skip-git-repo-check` always passed (pie runs in non-repo workspaces)
- [ ] every `exec`-level option precedes the `resume` subcommand
- [ ] a bucket is labelled from `limit_window_seconds` / the window's real duration,
      and a null `secondary_window` is not an error
- [ ] `codex.quota_probe = "off"` — **the default** — constructs **no** HTTP
      request, touches neither the credential store nor disk, and still lets
      `pie usage codex` print session-log totals; the assertion is on the
      *default* config, not on an explicitly-set `off`, because a switch that
      defaults to on is not an opt-in
- [ ] opting in with `codex.quota_probe = "http"` does reach the endpoint path, so
      the switch is real in both directions rather than a flag with no effect
- [ ] auth gate reads `auth_mode` from `~/.codex/auth.json` and requires subscription
- [ ] `~/.pie/auth.json` (legacy, holds extracted tokens) is never read — assert with a test that the path is untouched/no reader exists

## 6. M4 — unified auth + usage

Closed 2026-10-12. **Its work was built as part of M0–M2 and never given a verdict** —
a bookkeeping fault, recorded here so the roadmap can tell it is finished. From
`ROADMAP.md` M4:

> `pie auth`: agy + codex live; claude "no subscription"; API rows commented.
> `pie usage`: unified weekly/monthly + session-log token/cost totals (no token spend).
> **Done:** both render without a selected model.

**Acceptance**

```
cargo run -- auth
cargo run -- usage
```

Both must exit 0 **with no `--model` given** — that is the roadmap's Done condition,
and it is the thing worth asserting, because a command that silently requires a model
would still "work" if you happened to supply one.

**Deviations, recorded rather than left silent**

| Roadmap wording | What shipped | Why |
|---|---|---|
| "weekly/monthly" | "last 7 days" / "last 30 days" | The same windows; the label states the actual range rather than implying a calendar week or month. Satisfied by equivalence, not renamed |
| "token/cost totals" | token totals only, no cost column | Every backend is subscription-locked, so `cost_usd_micros` is `0` on every real row. A permanent `$0.00` would imply cost is measured when it is not. `TOKEN-ECONOMICS.md` §10c states this for the project: pie optimises tokens, wall clock and quota, never money. Cost renders again if M10's API-key backends land, where it is real |

Also note the quota half of `usage` is cached rather than always probed
(`agy.usage_cache_ttl_secs`), because the probe costs agy ~5–10 s of its own
initialisation. A cached reading is labelled with its age.

**Checks**

- [ ] `auth` and `usage` both succeed with **no model selected**
- [ ] `auth` lists the three CLI backends and nothing else
- [ ] `usage` prints all three windows and the totals block
- [ ] neither command writes a session-log row, and neither spends model quota (I7)
- [ ] `tests/acceptance.rs` asserts the rendered output, not only the exit status

## 7. M3 — profile projection and routing

Contract: `DESIGN.md` §10. Listed after M4 because M4 was closed out of sequence;
**M3 is the active milestone**. From `ROADMAP.md` M3: *"`~/.pie/profile.toml` +
`AGENTS.md` + `memory/*.md`; projection per backend"*, with the Done condition
*"same persona on agy and codex; no per-CLI config"*.

**Acceptance**

```
cargo test --test profile
cargo run -- profile
```

**Checks**

- [ ] **The Done condition at payload level**: the persona agy receives and the
      persona codex receives are the *identical text*. Asserted without spending
      quota, because a test needing two live turns would be run rarely
- [ ] the projection is byte-identical when built from a different working directory
      and at a different time — no timestamp, session id, cwd, model id or quota
      figure anywhere in it
- [ ] `memory/*.md` concatenate in filename order, independent of filesystem
      enumeration order
- [ ] a missing `profile.toml`/`AGENTS.md`/`memory/` is an empty projection, not an
      error: a fresh install works with no configuration
- [ ] claude is given the persona via `--append-system-prompt`, not folded into the
      prompt, and that path is labelled unverified (no subscription on this machine)
- [ ] a projection that pushes agy's argv past the 20 000-char cap is
      `PromptTooLong` naming the projection, **never** a truncated persona
- [ ] no backend receives a persona the user did not configure
- [ ] nothing reads or writes vendor config: a static check over `src/` for
      `~/.codex/config.toml` and `~/.claude/settings.json`

      **One exception, allow-listed with its reason** (found while implementing M3):
      `auth::agy_credential` probes `~/.gemini/config/config.json` for *presence* to
      decide whether agy is logged in (I9). That is an auth probe, not configuration
      — pie never *uses* vendor config to shape the persona. This check originally
      banned `.gemini/config` outright, which contradicted M1's own auth path; the
      contradiction was the contract's, and the contract was wrong.
- [ ] `pie profile` spends no quota and writes no session row
- [ ] routing: `general.default_backend` selects codex by default; an unrecognised
      value is a config error; an explicit `--model` always wins

**Live (G5)**

```
# profile.toml sets a distinctive marker the model must echo
cargo run -- -p --model agy/gemini-3.8-flash "<marker probe>"
cargo run -- -p --model codex/gpt-6-luna "<marker probe>"
```

The marker must appear in **both** replies. This is the real proof of `ROADMAP`'s
Done condition: payload equality proves the same bytes were sent, a live turn proves
the persona was actually honoured. Two turns, both on approved models.

## 8. M5 — sessions, compaction and OCR

Contract: `DESIGN.md` §11. Done condition from `ROADMAP.md`: *"`pie -p "audit this
repo"` delegates + persists; resume works"*.

**Acceptance, per stage.** Each becomes a G4 gate selected by name, so a red gate
names the stage that broke rather than the whole milestone.

| Stage | Command | Shape |
|---|---|---|
| A persist | `cargo test --test sessions` | a turn writes `<id>.jsonl` plus an index row; a write failure never fails the turn |
| B continue | `cargo test --test sessions` | `--continue` picks the newest session for provider+cwd; `--session <id>` targets one; a vendor id that no longer exists re-bootstraps instead of erroring; **the session id is surfaced after a turn**, as `[resume] <vendor-id>` already is, because otherwise there is no way to target `--session <id>` without first running `pie sessions` |
| C compact | `cargo test --test compact` | extractive: head + state block + tail; spawns nothing (quota-free); stored rows untouched; the block sits after the persona |
| D ocr | `cargo test --test ocr` and `cargo run -- ocr <fixture>` | a reachable stub is cached by content hash; an unreachable itsmini is a stated error, never an empty success |
| E mcp | `cargo test --test mcp` | a stdio handshake: `initialize`, then `tools/list` returns exactly one tool named `ocr` |
| F switch | `cargo test --test sessions` | switching backends rebuilds the prompt from pie's transcript and records the new vendor id — payload-level, no quota |
| G5 | live, by hand | two turns with `--continue`: the second answers from the first's context |

**Checks**

- [ ] the frozen `usage.jsonl` schema is unchanged — sessions use their own files
- [ ] no session row is written by `auth`, `usage`, `models`, `doctor`, `profile` or `ocr`
- [ ] compaction spawns nothing and spends no quota
- [ ] compaction never deletes stored rows, so it is reversible by inspection
- [ ] the state block sits after the static persona, for prefix-cache safety
- [ ] MCP advertises exactly one tool, and enabling the server is opt-in
- [ ] an OCR failure is a stated error, never an empty success
- [ ] a resume whose vendor id is gone re-bootstraps from pie's transcript (`RISKS.md` §6)
- [ ] a session write failure does not fail a successful turn
- [ ] `--continue` is opt-in: a plain `pie -p` starts a fresh conversation

**Not verifiable here**: `ocr` against a real itsmini, which does not exist yet. Stage
D therefore tests the stub contract and the failure path, not a real conversion, and
says so rather than implying coverage it does not have.

### Notes stage C raised, recorded rather than left implicit

- **Its gate was decorative at first.** Section 8 originally named
  `cargo test --test sessions` as stage C's command, so compaction could have broken
  without any gate failing — the same class as B20, where a gate ran but could not
  fail. Both runners now have a separate `G4 acceptance (M5 compact)`. A per-stage
  gate is only worth having if it names the stage's own tests.
- **Order is relative, not absolute.** The composed order is persona → head turns →
  state block → tail turns → task. The *head* sits between the persona and the block
  because chronology matters and the head is the most stable part of the history. The
  property that protects the cache is therefore the relative one — the volatile task
  text last, nothing volatile before it — not "the block is immediately after the
  persona".
- **"What changed" is an outcome ledger, not a file-change list.** A `TurnRow`
  records `ok` and `stop_reason`, and in delegate mode pie never sees the vendor's
  file edits, so the block can say *that* a turn succeeded and *how* it stopped — not
  which files moved. The dropped turns are handed back rather than discarded, so the
  detail is recoverable even though it is not in the block.

## 9. M6 — interactive TUI

Contract: `DESIGN.md` §12. Done condition from `ROADMAP.md`: *"interactive parity across
agy + codex"*.

**Acceptance, per stage.** Every stage gets its **own** gate named after its **own**
test binary. That is not boilerplate: in M5 two of three stages shipped with a gate
that could not fail them, because the gate ran a different stage's tests. A stage
gate that does not name the stage's own tests is decorative.

| Stage | Command | Shape |
|---|---|---|
| A state + reducer | `cargo test --test tui_app` | every `Input` maps to the documented `Action`; typing, submit, cancel, scroll, picker transitions; `apply_event` folds a `StreamEvent` into the transcript; no crossterm type appears in the reducer's signature |
| B render | `cargo test --test tui_render` | with `TestBackend`: the prompt line, the status, and both `User` and `Assistant` entries appear; a long transcript scrolls rather than overflowing; a picker renders its options; the buffer is asserted, not merely non-empty |
| C loop + wiring | `cargo test --test tui_loop` | the turn runs as a task and its events reach the app through the channel without blocking: assert a scripted sequence of events lands in order while the loop stays responsive to a quit input |
| D picker + keys | `cargo test --test tui_keys` | the documented keybindings each produce their action; choosing a model updates `App::model`; every keybinding is documented in help; a second turn can be submitted after the first completes |
| G5 | live, by hand | drive a **real** turn's `StreamEvent`s through `apply_event` and assert the rendered buffer contains the answer, then `pie tui` starts and quits cleanly |

**Checks**

- [ ] the reducer takes no crossterm type — `src/tui/app.rs` has zero crossterm imports, asserted by **reading the file's own source** in a test, not asserted in a doc comment
- [ ] `render` is asserted through `TestBackend`, so it is tested without a terminal
- [ ] the UI does not block on a turn: events arrive while the loop still accepts input
- [ ] **a second turn can be submitted after the first completes.** The first freeze made the TUI single-turn (`Done` set a terminal status and only `Idle` could submit); the lifecycle is now stated in `DESIGN.md` §12 and asserted, because "the TUI can hold one conversation" is the kind of defect that looks like working software
- [ ] the keybinding help and the reducer read the **same** table, so the documented keys cannot drift from the implemented ones — the assertion is that every documented row produces its documented action through `reduce`, not that a help string exists
- [ ] **the whole keymap is visible at 80x24**, an ordinary terminal — asserted, because the first version fitted whole only from ~200 columns and omitted `tab` at 80, which is a criterion satisfied on a terminal nobody has
- [ ] a bare `pie` still prints help and never enters the TUI
- [ ] `pie tui` is opt-in, and no existing test or CI job can enter it accidentally
- [ ] the UI states that tool steps are the *vendor's*, since delegate mode means pie runs none of its own
- [ ] the G6 cross-target check still passes with the new dependencies
- [ ] `pie tui` on a non-TTY stdin fails with a clear message rather than emitting escape codes into a pipe

**Not verifiable here**: human interaction. Keystroke sequences are covered through the
reducer, and rendering through the buffer — but nobody can automate "does this feel
usable", so that remains a judgement for the user, and M6's verdict says so.

## 10. Bug protocol

A "bug" is any behavior that contradicts `ROADMAP.md`, `DESIGN.md`, or a
knowledge file. Process, in order:

1. **Record** — a one-line description + the exact command that reproduces it.
2. **Severity** — `blocker` (milestone gate fails) · `major` (wrong answer,
   quota/secret risk) · `minor` (cosmetic, protocol noise).
3. **Failing test first** — add a test that fails for the stated reason. No fix
   without a test. This is the regression guard.
4. **Fix** — smallest change that makes the test pass.
5. **Re-run G1–G3**, and G5 if the fix touched a backend path.
6. **Commit** as `fix:` (PATCH per `VERSIONING.md`), message naming the symptom.

Severity handling:

| Severity | Action |
|---|---|
| blocker | stop the milestone; do not commit anything else until green |
| major | fix inside the milestone; it cannot be marked done with an open major |
| minor | may be deferred, but must be logged in the run log with a reason |

Bug categories to actively hunt before marking a milestone done:

- **Parser fragility** — CLI auto-update changes a field/event (RISKS §3)
- **Protocol drift** — unknown event treated as fatal instead of forwarded
- **Secret leak** — token in stdout/stderr/log/error message (RISKS §10)
- **Quota burn** — a probe that spends model tokens (RISKS §7, §13)
- **Hang** — no deadline, or a child that outlives its parent (RISKS §6)
- **Env leak** — API key reaching a child, flipping it to pay-as-you-go (RISKS §10)
- **Panic** — `unwrap`/`expect`/index-out-of-bounds on external input

## 11. Independent verification

Each milestone, after G1–G3 are green and before it is called done, dispatch an
independent reviewer subagent with **no shared context** and give it: the design
contract, the CLI knowledge file, and the diff. It must be told to try to refute
the milestone, not to confirm it. Findings come back as rows with severity; each
is either fixed (with a test) or logged as accepted with a written reason.

The reviewer must not be the same agent that wrote the code.

## 12. Run log

Appended as gates pass. Evidence = command + observed result, not a claim.

**Convention:** every count and quoted output in this table is a *snapshot at the
commit that recorded it*, not a claim about the current tree. The QC sweep caught
the M0 row being read as current when its numbers were the M0 commit's. To learn
what HEAD does, run `scripts/qc.ps1` / `scripts/qc.sh` — never trust a number
here. A row is a record of what was verified, not a substitute for verifying.

| Milestone | G1 | G2 | G3 | G4 | G5 | G6 | Verdict | Evidence |
|---|---|---|---|---|---|---|---|---|
| M0 | ✅ | ✅ | ✅ | ✅ | n/a | ✅ | **done** | Snapshot at the M0 commit (`1ee7c18`). 113 tests passed then (100 lib + 6 `no_secrets` + 7 `process`), 5 live-smoke tests correctly ignored; `--list-backends` printed agy/codex/claude with agy `1.3.3`, codex `codex-cli 0.162.0`, claude `2.1.296` + `no-subscription`; `cargo check --target x86_64-unknown-linux-gnu --all-targets --no-default-features` clean. **Not the current total** — see the M1/M2 rows for HEAD, and re-run `scripts/qc.ps1` rather than trusting any number here |
| M1 | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | **done** | G4 `cargo test --test parse_agy` → 31 passed. G5 live: a real `agy/gemini-3.8-flash` turn printed `ok` (exit 0) and logged `{input_tokens:20661, cached_input_tokens:0, output_tokens:23, reasoning_tokens:22, ok:true, stop_reason:"SUCCESS"}`; no key material in stdout or stderr; a second live turn passed `agy_turn_succeeds_and_is_recorded` and the free probe passed `agy_usage_probe_reports_buckets`. `pie usage agy` rendered all 4 real buckets across 2 groups with group **and** window visible, and logged no session row. G6 clean |
| M2 | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | **done** | G4 `cargo test --test parse_codex` → 34 passed. G5 live: a real `codex/gpt-6-luna` turn printed `ok` (exit 0) and logged `{input_tokens:13424, cached_input_tokens:1792, output_tokens:5, reasoning_tokens:0, ok:true, stop_reason:"completed"}` — the full frozen schema, and the first live evidence that the vendor prefix cache actually operates (13.3% on a trivial turn). `pie usage codex` printed `30d 88.0% 2026-11-08T01:52:52Z` and logged **no** session row, proving a probe is not a turn. G6 clean |
| M3 | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | **done** | G4 `cargo test --test profile` → 12 passed; `cargo run -- profile` → exit 0 with a provenance table (KIND / BYTES / SOURCE). G5 live: a rule in `profile.toml` requiring the token `ZQ7-MARKER` was honoured by **both** backends — agy replied `hello` + `ZQ7-MARKER`, codex replied `hello` + `ZQ7-MARKER` — so the persona is honoured, not merely sent. Payload equality (agy's persona text == codex's) is asserted without quota in `tests/profile.rs`, and determinism was verified by assembling from two different working directories and byte-comparing. G6 clean. Two findings: the contract's vendor-config check contradicted M1's own agy auth probe, so the contract was wrong and section 7 now allows exactly that one presence check; and the B13 redaction fix had over-corrected into destroying paths in diagnostics (B23) |
| M4 | ✅ | ✅ | ✅ | ✅ | n/a | ✅ | **done** | Closed out of sequence: its work was built during M0–M2 and had no verdict until now. G4 `cargo run -- auth` and `cargo run -- usage` both exit 0 **with no `--model`**, which is the roadmap's Done condition. `tests/acceptance.rs` asserts the rendered rows, all three usage windows and the totals block, plus that neither command writes a session row. Two deviations recorded in section 6. G5 is `n/a`: neither command spends model tokens |
| M5 | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | **done** | Six stages, each with its own gate: **A** persist, **B** `--continue`/`--session`, **C** compact, **D** ocr, **E** mcp, **F** switch. 392 tests. **G5 live**: turn 1 seeded a session (replied `ok`), turn 2 with `--continue` replied **`41`** from turn 1's context, both reporting the same session id; `pie sessions` showed 2 turns; `usage.jsonl` grew to 2 rows and still has exactly its 14 frozen keys. Independent MCP handshake against the real binary: 3 stdout lines, **all** valid JSON, `tools == ["ocr"]` (count 1). G6 clean. **Known gap, stated rather than glossed**: a multi-line switched payload is not delivered end-to-end on Windows, because batch stand-ins reject multi-line argv — stages B and F both hit it, and it is payload-tested plus a cfg-gated half with Linux CI getting the stronger half |
| M6 | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | **done** | Six stages, each with its own gate: **A** state+reducer, **B** render (`TestBackend`), **C** loop + channel + wiring, **D** picker + keymap. 498 tests, 7 live correctly ignored, across 19 binaries. **G4** gained a named gate per M6 stage — `tui_app` 36, `tui_render` 20, `tui_loop` 19, `tui_keys` 20 — plus a check on the **built binary** that `pie tui` refuses a pipe: non-zero exit, no escape codes, a message naming the reason, and all four of its branches proved able to fail with a stand-in. **G5 live**: a real `agy/gemini-3.8-flash` turn and a real `codex/gpt-6-luna` turn were each driven through `apply_event` and asserted **in the rendered buffer** (`a_real_*_turn_renders_through_the_tui_reducer`) — that is the roadmap's Done condition, not a proxy for it; plus both quota probes and the leak check, 7/7. **The first G5 run FAILED** (`codex_usage_probe_reports_buckets`, stale against the new default — B33), so M6 was not declared done until a later run was green. G6 clean. **24 offline gates green** (G1/G2/G3/G3b/G3c/G3d + 17 G4 + G6), **25/25 with `-Live`**. **Not verifiable here**: human interaction — keystrokes are covered through the reducer and rendering through the buffer, but "does this feel usable" stays a judgement for the user, and no verdict above should be read as claiming it |
| M7 | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | **done** | Four stages, each with its own gate binary: **A** extract (`learn` 15), **B** consolidate (`consolidate` 14), **C** schedule + budget (`schedule` 7), **D** commands (`memory_cmd` 20). 574 tests, 7 live correctly ignored. **G5, run on real data and quota-free:** three real `agy/gemini-3.8-flash` turns produced four real transcripts; `pie learn --report` mined 3 turns into 4 candidates with correct kinds and counts — `uses cargo` hits 3, the repeated preference hits 2, and `never write unwrap...` classified as `Preference` and **not** `Habit` — each carrying session-id + turn provenance. `pie memory` printed them; `pie profile` showed the learned section **appended** to the projection at 637 bytes with `profile.toml`/`AGENTS.md` never created, so the head was not reordered; `pie memory revert <id>` restored the pre-apply state and `pie memory` then reported "nothing learned yet". The deliberate breaks were caught by **named** tests, and two by a **count** rather than an error path: making the default config spend fired `left: 1, right: 0` on the spawn counter, and making the job spawn-then-report-`NotDue` fired `left: 4, right: 0`. **28/29 offline gates green.** **G5 for M1-M6 was not re-run, and did not need to be**: M7 changed no payload those tests prove (`learn`/`memory`/`schedule`/`commands`/`tui`/`cli`/`config` only), and the rule is to re-run *after* a payload changes. **Not verifiable here**: whether the learned preferences are *good* — extraction is rule-based and inspectable, but "does this reflect how I actually work" is the user's judgement. The vendor-store importers are fixture-tested only: `~/.claude` has no history on this machine (checked), and reading any vendor store is opt-in and off by default |
| M8 | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | **done** | Four stages, each with its own gate binary: **A** drift (`drift` 25), **B** news (`news` 22), **C** report (`watchtower` 10), **D** command (`watch_cmd` 17). **658 tests**, 27 binaries, 7 live correctly ignored. **G5 on the real CLIs, and quota-free** (a `--help` is not a turn, which is the whole reason drift is safe to schedule): `pie watch --month 2026-11` probed **agy 1.3.3, codex-cli 0.162.0 and claude 2.1.296**, wrote **exactly one** report and three snapshots, and reported "first snapshot taken, nothing to diff"; a second run reported **"no drift found — every probe matched the previous snapshot"** and still wrote one file. **32/33 offline gates green.** Three findings came from running the thing rather than from its tests: **B35** (`agy --help` prints usage to **stderr** — 2905 bytes there, 0 on stdout — so every real run failed, and the empty-capture rule added earlier in this same milestone is what made it loud instead of silent; had an empty help been written as a snapshot it would have reported "no drift" forever); **B34** (`CliSnapshot::strings` has no producer, and the obvious one would read *node's* binary for codex, so the report now says the third REA leg is inert instead of pretending); and the `REA_MODE_DETAIL` header, which claimed "selected binary strings" while nothing produced them. Stage D's process died mid-run on a transport error **after** its work was durable, so the deliverable was verified and the missing gate-failure evidence was produced by the maintainer instead (two deliberate breaks, each caught by a named test, each restored byte-identical). **Not verifiable here**: the structural REA check — no disassembler is installed, so `strings` comparison is unimplemented and the report labels the mode accordingly; and whether the impact ranking is *useful*, which depends on terms the user configures |
| M9 | ✅ | ✅ | ✅ | ✅ | n/a | ✅ | **offline-verified** | Three stages: **A** claude `stream-json` (`parse_claude` 29), **B** the tool loop (`tool_loop` 17), **C** capability + wiring. **711 tests**, 29 binaries, **35/35 gates green including `-Live`**. **The verdict is `offline-verified`, not `done`, and the reason is stated rather than glossed**: `~/.claude/` holds no credential and no history, so no claude turn is possible and every claude fixture is **constructed from `knowledge/claude.md`** — a document that says its own envelope is *expected* rather than captured. A fixture written from a specification is not evidence the specification is right, so these tests are evidence about pie's protocol handling only. G5 is `n/a` for the milestone and the live test runs anyway: without a credential it asserts the refusal (naming claude, logging the attempt as `ok: false` with zero tokens), and with one it would assert the recorded envelope — so the first live run becomes the evidence that document never had. **Two defects, both found at seams no stage's own gate could see:** **B36** — `drive` dropped every event a router owed beyond the first, and the dropped one was a `tool_use`, i.e. a conversation that never finishes; found by a *driver-level* test written after stage A flagged the seam, failing on its first run with `left: ["text"]` while the router's own 28 tests passed. **B37** — the frozen rule "flip `tool_loop` once the loop exists" was unsatisfiable against a backend whose `stream` returns `Unsupported`, so the flag stays `false` and §17 was corrected. The deadlock is covered three ways, including a **permanent control** asserting the stand-in *can* deadlock a single-threaded transport — without it the deadlock test would assert nothing. I2 is asserted behaviourally on the loop's second spawn path, because a mirrored invariant stops holding silently |
| M10 | ✅ | ✅ | ✅ | ✅ | n/a | ✅ | **offline-verified** | Two layers, one gate binary each: **A** the SSE wire (`sse` 30) and **B** the HTTP transport (`http_transport` 13); **C** wires the two backends over **one** parameterised core — a deliberate departure from `DEFERRED-BACKENDS.md`'s `openrouter.rs` + `deepseek.rs` sketch, because that document also says DeepSeek is "identical apart from the two constants", and two near-duplicate files describing one wire protocol are two places for the wire to drift. **755 tests, 37/37 gates green including `-Live`.** **`cost_usd_micros` is real end-to-end**: `Usage` gained the field (additive, `serde(default)`, so every frozen row and fixture still reads), `sse` maps the endpoint's own figure, and `session_log`'s row writer reads it instead of the `0` it was hardcoded to for ten milestones — asserted by a test that a reported cost **reaches the row** while a subscription backend's stays zero. **The registry grew to five and M0's rule was inverted rather than deleted**: `--list-backends` now asserts the two API-key backends *appear*, report `needs-login`, name their key variable, and are **not** shown as a missing binary (they have no binary to miss). **The follow-up closed the two stubs**: `HttpClient` gained a required `get` plus a **bounded** body reader, so `list_models` now reads the vendor's own `GET /models` (verified live — it returned OpenRouter's real catalog) and `usage` reads OpenRouter's `GET /key` prepaid balance; the mapping is pure and unit-tested, and the fake-key probe confirmed the key reaches **no** output and nothing under `$PIE_HOME`. **G5 remains `n/a — needs the user's key`**, stated rather than skipped: a live *turn* spends real credit in a currency nobody opted into, so the transport is tested against a scripted client and the wire against constructed frames — and a constructed frame is not evidence that a provider's real stream matches it. **Not verifiable here**: any live turn, for that reason |

Verdict values: `done` (all applicable gates) · `offline-verified` (G5 skipped by
user decision) · `blocked` (a gate fails) · `not started`.

### Re-validation, 2026-10-12 (after M3)

M0–M4 were re-validated bottom-up before starting M5, and the re-validation is
**stronger evidence than the rows above**, not a repeat of them: M3 rewrote
`build_args` for agy and codex to fold in the persona, which invalidated the M1 and
M2 live evidence recorded at their own commits. A milestone's G5 does not survive a
change to the argv it verifies.

| Gate | Result |
|---|---|
| G1, G2, G3, G3b | pass — **260 tests**, 5 live correctly ignored |
| G4 M0 / M1 / M2 / M3 ×2 / M4 ×2 | pass, all seven selected by name |
| G6 cross-target `x86_64-unknown-linux-gnu` | clean |
| G5 live: agy turn, agy probe, codex turn, codex probe, leak check | **5/5 pass** on the current tree |
| G5 M3: persona marker honoured | pass on **both** backends (`hello` + `ZQ7-MARKER`) |

No milestone regressed. The lesson worth keeping: **re-run a live gate after any
change to the payload it proves** — otherwise the earlier green is a statement about
a revision that no longer exists.

## 13. Bug log

Every bug found, the gate that caught it, and the regression test that now
prevents it. This is the record the milestone verdicts above rest on.

### M0

| # | Sev | Symptom | Caught by | Fix | Regression test |
|---|---|---|---|---|---|
| B1 | major | `redact` failed to redact `token=<80-char blob>`. The `=` broke the token split, so a credential could reach an error message or log line (`RISKS.md` §10) | `process::tests::redacts_long_opaque_blobs` | Rewrote redaction as two passes: collapse any unbroken 60+ char `[A-Za-z0-9._-]` run, then apply known prefixes. Also redacts the token *after* `Bearer` | `redacts_long_opaque_blobs`, `redacts_the_token_after_bearer`, `short_ordinary_text_is_untouched` |
| B2 | major | `StreamEvent` used serde internal tagging, which **cannot** represent a newtype variant holding a string — `pie -p --json` would have failed at runtime with `cannot serialize tagged newtype variant` | `provider::tests::events_serialise_with_a_kind_tag` | Switched to adjacent tagging (`{"event":"text","data":…}`), pinned each tag with an explicit `rename`, and asserted the tag equals `StreamEvent::kind()` | `events_serialise_with_a_kind_tag` (round-trips all 7 variants) |
| B3 | major | codex's version probe reported **node's** version (`v25.9.0`) instead of codex's, because the probe ignored the `node <codex.js>` launcher prefix. Drift detection (I8) would have compared the wrong binary | `--list-backends` acceptance run | Added `Provider::version_command()`, defaulting to `(binary, version_args)`; codex overrides it to carry the script prefix | `version_probe_carries_the_node_script_prefix` |
| B4 | major | `#[async_trait]` missing on the three `impl Provider` blocks — the crate did not compile at all (E0195) | `cargo check` | Added the attribute to each impl | compile gate |
| B5 | minor | Env-mutating tests raced on `HOME`/`PIE_HOME` (process-global, parallel threads), producing a nonsense failure in `paths::tests::empty_override_is_ignored` | `cargo test` | Added `src/testing.rs` with a shared `EnvGuard` that serialises env mutation and restores prior values on drop | whole `paths`/`auth`/`config` env suite |
| B6 | minor | Table truncated paths with `…[truncated]`, hiding which binary was meant. `agy\bin\agy.exe` was unreadable | `--list-backends` acceptance run | Added `short_path`: keep the last three components, clip to the column width | `long_paths_keep_the_identifying_tail`, `short_path_never_exceeds_the_column_width`, `unix_style_paths_are_shortened_too` |
| B7 | minor (process) | **A committed tree failed G1.** `tests/live_smoke.rs` was committed unformatted, because `cargo fmt --all` was run *before* writing that file and the next commit staged only selected paths. The tree at HEAD was red | `cargo fmt --all --check` run with the change stashed, to prove HEAD rather than the working tree | Reformat committed; the habit is now **`cargo fmt --all` → `git add -A` → run G1–G3 → commit**, so a formatting change can never be left behind in the working tree | covered by G1 itself |
| B8 | **blocker** | **The Windows gate runner did not run at all.** `scripts/qc.ps1` contained UTF-8 em-dashes and no BOM, so Windows PowerShell 5.1 read the file as Windows-1252, turned `—` into `\uFFFD?"` inside string literals, and failed to *parse* — `Unexpected token ')'`. The gate runner on the primary platform was dead, and nothing had ever executed it | Running it (`powershell -NoProfile -ExecutionPolicy Bypass -File scripts/qc.ps1`), which failed before G1 with parse errors | Both gate scripts converted to pure ASCII; the encoding rule documented in `qc.ps1`'s header; CI now parse-checks `qc.ps1` under `shell: powershell` **and** asserts it contains no byte > 127 | CI step "Gate runner self-check (parse + ASCII-only)" |
| B9 | minor (process) | A commit landed with a **red G2**. `cargo clippy` failed in a test (`unnecessary_to_owned`) but the command was `cargo clippy … \| grep … ; git commit`: the pipe made the status that of `grep`, and `;` committed regardless. Same family as B7 - the gate ran, but could not gate | Reading the gate output in the same command that committed | Fixed the lint; the rule is now written into §1 | the gate runners themselves |
| B10 | **major** | **Three `/usage` probes gave three different wrong answers, because the shell rewrote the argument.** git-bash/MSYS turns a leading-slash argument into a Windows path, so `agy --print /usage` reached agy as the literal prompt text `C:/Program Files/Git/usage`. Consequences: one probe burned a real ~20 700-token model turn answering *"how much disk space does Git use"*; another returned an empty payload with `num_turns: 0`; that empty one was recorded as the fixture. The near-miss: I concluded `/usage` was broken and quota-costing and drafted a design change to drop `pie usage agy` entirely. It is in fact free (`num_turns: 0`, all usage zero) and returns a full quota payload | Re-running with `MSYS_NO_PATHCONV=1`, which returned the real payload. The discriminator is cheap and should have been used first: a **mangled** probe shows `num_turns: 1` with ~20 700 input tokens, while a **real** expansion shows `num_turns: 0` and all-zero usage - a free probe reporting 20 700 tokens spent is a probe-harness bug, not CLI behaviour | Corrected `tests/fixtures/agy_usage.json` to the real capture; fixed `usage_args()` to `--print-timeout 0` (with `30s` the expansion returned an empty payload); **reverted a false claim I had written into `knowledge/agy.md`** from the corrupted capture; documented the trap in `knowledge/agy.md` and `PLATFORMS.md`; ACCEPTANCE section 4 now requires a manual probe to set `MSYS_NO_PATHCONV=1` | Real-capture parser tests (4 buckets across 2 groups, `remaining_fraction` ~= 0.8849); ACCEPTANCE section 4 checks. Note pie's own spawn path was never affected - `std::process::Command` does no rewriting |
| B11 | **major** | **The resume argv was wrong, and it was mine from M0.** `build_args` emitted `codex exec resume <id> --cd … --approve-for-me …`. Codex rejects that: `resume` is a subcommand, so `exec`-level options after it are not recognised, and `codex exec resume <id> --cd . --approve-for-me --json --help` returns `error: unexpected argument '--cd' found`. Every resumed codex turn would have failed. M0 had flagged the ordering as unverified ("NOTE(M2): confirm against `codex exec --help`") and the confirmation found it wrong | The M2 agent probed `codex exec resume <id> --cd . --approve-for-me --json --help` instead of trusting the note, and wrote the failing test first | Reordered to `exec <flags> resume <id> <prompt>`; regression test `every_exec_option_precedes_the_resume_subcommand`; documented in `knowledge/codex.md` and `internals.md` | `tests/parse_codex.rs`; ACCEPTANCE section 5 check |
| B12 | minor | **A frozen acceptance criterion was wrong.** Section 5 required `usage codex` to print "5h + weekly buckets", following `PLAN.md` §6. A live probe shows this account returns a single **30-day** window with `secondary_window: null` (`limit_window_seconds: 2592000`, reset `2026-11-08T01:52:52Z`). An implementation written to the criterion would have rendered nothing, or invented a missing bucket | The M2 agent's zero-cost live probe of the endpoint, reported rather than coded around | Section 5 revised to "a bucket per window the endpoint actually returns", labelled from its real duration; `knowledge/codex.md` records the observed shape. Same class of error as the agy section 4 revision (B10): a criterion derived from a plan document rather than from the CLI | ACCEPTANCE section 5; parser tests over the observed payload |
| B13 | **major** | **The redactor was weaker than the rule it exists to uphold.** `process::redact` recognised `sk-`, `eyJ`, `ghp_`, `gho_`, `ya29.` and nothing else, while `tests/no_secrets.rs` declared `AIza`, `sk-ant-`, `sk-proj-`, `xoxb-` and `-----BEGIN` forbidden — five of seven declared prefixes were not redactable. Three separate gaps: (1) the missing prefixes; (2) prefix matching only fired at the *start* of a whitespace-delimited token, so `key=AIza…` and `"token":"sk-…"` slipped through; (3) `+`, `/` and `=` were not run characters, so one base64 credential split at the first `+` or `/` into fragments that each fell under the 60-char threshold — which also missed a 39-char Google key entirely | The independent QC sweep, safety lens (ACCEPTANCE section 7) | Prefix list expanded to `SECRET_PREFIXES` and prefix matching now scans anywhere in a token with a `>= 8 chars after the prefix` guard so a bare marker is not treated as a secret; `+/=` added as run characters; threshold lowered to 40 with the deliberate bias toward over-redaction documented | `redacts_every_declared_secret_prefix`, `redacts_a_google_api_key_embedded_after_an_equals_sign`, `redacts_base64_containing_plus_slash_and_equals`, `a_bare_prefix_without_a_value_is_not_redacted`, plus `every_forbidden_prefix_is_also_redactable` in `tests/no_secrets.rs`, which makes the two lists unable to drift again |
| B14 | **major** | **agy had no subscription gate, and a service-account credential reached every child.** `Codex::stream` called `require_subscription()` first; `Agy::stream` had no check at all, and `auth::agy_credential` can never return `ApiKeyOnly`, so nothing stopped a turn. Separately, `GOOGLE_APPLICATION_CREDENTIALS` was deliberately *preserved* by the env scrub, on an assumption; `knowledge/internals.md` lists it among agy API-key paths and says not to use them, so a service account in the environment was a billing switch with no guard | QC sweep, safety lens | Added `agy::require_subscription()`, which refuses when the state directory is absent but **proceeds with a warning** when the classification is unreadable, because that heuristic is not load-bearing and must not gate a hard refusal; added `GOOGLE_APPLICATION_CREDENTIALS` to `BUILTIN_SCRUB` and corrected DESIGN section 8, PLATFORMS section 2 and I2, which all asserted the opposite | `redacts_every_declared_secret_prefix` for the scrub list; agy re-verified with a live turn after the scrub change (`agy_turn_succeeds_and_is_recorded`) |
| B15 | minor | Two paths could persist or log unredacted data. `driver.rs` logged an unparsed vendor stdout line with **no** redaction at all, so with `PIE_LOG=debug` a line like `OPENAI_API_KEY=sk-…` went to stderr verbatim; and `write_version_snapshot` stored `--version` output unredacted, so a CLI echoing a credential would write it into `~/.pie/cache/versions.json` | QC sweep, safety lens | Both routed through `process::redact` before printing or storing | covered by the B13 redaction tests plus the existing `snapshot_payload_has_no_room_for_a_secret` |
| B16 | minor | `PIE_STRIP_ENV=0` disabled the credential scrub **silently** for `pie -p` and `--list-backends`; the warning lived only inside `report_local_state`, i.e. `pie doctor`, so an inherited value (shell profile, parent process) meant running unscrubbed with no indication | QC sweep, safety lens | Warning emitted on stderr at config resolution, before any spawn, so every command surfaces it | manual: `PIE_STRIP_ENV=0 pie --list-backends` |
| B17 | minor | Two guards were weaker than their claims. The I11 test was **command-shaped** — it ran only `auth`, `usage` and `doctor`, so a reader on the turn path would have passed. And `kill_tree` spawned `taskkill`/`kill` without the env scrub, so an OS helper inherited credentials | QC sweep, safety lens | I11 guard replaced with a **path-shaped** static check: no production source outside `paths.rs` may name `legacy_auth_file`, which covers `-p` and `models` without spending quota; test modules are skipped because a test asserting non-modification is not a reader. `kill_tree` now applies `BUILTIN_SCRUB` to both helpers | `nothing_outside_paths_rs_names_the_legacy_auth_file`; the behavioural loop also gained `--list-backends` |
| B18 | **blocker** | **A non-UTF-8 byte on stdout aborted a healthy turn, so I5 was false.** `BufReader::lines()` fails with `InvalidData` on a byte that is not valid UTF-8, and that error path killed the child and returned `PieError::Io` — discarding a good answer because a vendor printed one stray byte. `drain_tail` already took the lossy view for stderr, so the two paths disagreed. The test meant to cover this emitted the invalid bytes only under `cfg(not(windows))`, so it passed **vacuously on the primary platform** and would have gone red on the CI ubuntu job that actually reached the byte | QC sweep, contract lens | Replaced `lines()` with a capped, lossy reader (`read_line_capped`, on `fill_buf`/`consume`) shared by the stdout loop and `drain_tail`; the invalid bytes are now written to a file and echoed by the shell, so the test exercises the path on Windows too | `non_utf8_bytes_on_stdout_do_not_abort_the_run` |
| B19 | **blocker** | **Unchecked `u64` accumulation over a user-writable file.** `report::totals` used plain `+` on values read from `~/.pie/usage.jsonl`, and `Totals::total_tokens` added three more. One row with `input_tokens: u64::MAX` panicked a debug build and wrapped silently in release, giving wrong totals with no error | QC sweep, robustness lens | Every accumulation is `saturating_add`, with the reason documented on both items | `totals_saturate_instead_of_overflowing` |
| B20 | **blocker** | **Three gates did not gate.** (1) G4 for M0 ran `--list-backends` and discarded stdout, so an empty or wrong table passed every gate. (2) Both live quota probes asserted only the header line, which `render_report` prints unconditionally, so a parser returning zero buckets passed G5. (3) The `PIE_LIVE` guard returned early instead of failing, so `cargo test -- --ignored` reported "5 passed" while executing nothing | QC sweep, gate-integrity lens | (1) `tests/acceptance.rs` runs the real binary and asserts three rows in order, all five header columns, a valid auth state per row, and that no deferred backend is registered. (2) The probes also require no "no buckets reported" line and at least one rendered percentage. (3) The guard asserts rather than returns | `tests/acceptance.rs`; strengthened `*_usage_probe_reports_buckets`; the guard itself |
| B21 | medium | **Router semantics that could lose or invent an answer.** codex read the answer from the live accumulator, so an agent item arriving after `turn.completed` was appended to it, and a `turn.completed` with no agent message returned success with an empty answer and logged `ok: true`; a trailing `turn.failed` errored a turn that had already delivered. agy assigned terminal usage wholesale, so a partial `usage` zeroed fields already accumulated, accepted a `text_delta` from any step type that was not `tool`, and let a second `result` overwrite the first terminal. Also, codex checked the status string before `exit_code`, so a command that exited non-zero rendered as a completed tool step — contradicting the name of the test asserting it | QC sweep, contract and robustness lenses | codex freezes the answer at `turn.completed` and refuses an empty one as a protocol error; the first terminal wins and a later failure is ignored **with a warning**; agy merges usage field-wise (`Usage::merge`), gates text on `step_type == "agent_response"`, and latches the first terminal; `exit_code` is checked before `status` | `usage_merge_does_not_zero_fields_a_later_event_omits`, `a_failure_after_the_terminal_event_does_not_erase_the_answer`, plus updated `a_malformed_payload_of_any_shape_does_not_abort_the_run` and `a_completed_item_with_a_non_zero_exit_code_is_a_failed_tool_step` |
| B22 | medium | **Unbounded input from sources pie does not control.** `Instant::now() + spec.timeout` panics on overflow and the timeout is an unvalidated `u64`; `argv_char_len` counted chars where Windows counts UTF-16 code units, under-measuring non-BMP prompts into an opaque `CreateProcessW` failure; `kill_tree` awaited `taskkill` with no deadline, on the timeout path it exists to enforce; `cache::read` read any file whole; and the argv cap measured the configured binary name before `stream` swapped in the resolved path | QC sweep, robustness lens | `checked_add` with a config error; UTF-16 counting on Windows; `kill_tree` bounded by a 5 s timeout; a 16 MiB size guard on cache reads, with the reason in the code | `an_implausibly_large_cache_file_is_a_miss_not_an_allocation`, existing cap tests |
| B23 | **major** | **The fix for B13 introduced over-redaction that destroyed file paths in diagnostics.** Adding `/` to the opaque-run charset (to catch base64 secrets containing it) meant any unix-style path over 40 characters was redacted: a real config error printed `C:«redacted»\config.toml` instead of naming the file, and an error that cannot name the offending file is close to useless. A path is not a secret, and paths appear in almost every diagnostic | Running the real binary with a long `PIE_HOME` and deliberately invalid TOML — not by a test, which is the point: the security lens that found B13 could not have found this, because over-redaction looks like success | `/` removed from the run charset, with the trade-off written into the code: paths are far more common in diagnostics than unprefixed base64, so the path wins. The precise mechanism is `SECRET_PREFIXES`, which catches known credential shapes regardless of length; the run heuristic is only the backstop for unknown ones. Residual gap recorded rather than hidden: a long base64 secret *containing* `/` can survive in fragments | `a_long_path_in_a_diagnostic_is_not_redacted`, which asserts both a Windows and a unix path survive |

### M6

| # | Sev | Symptom | Caught by | Fix | Regression test |
|---|---|---|---|---|---|
| B25 | **blocker (gate)** | **A required gate had no runner, and its command could not pass as written.** G6 is listed as required in §1 and every run-log row reports it green, but neither `qc.ps1`, `qc.sh` nor CI invoked it — the column rested on a hand-run command and a claim with nothing able to contradict it. And `cargo check --target x86_64-unknown-linux-gnu --all-targets` as documented **fails** locally (`failed to find tool "x86_64-linux-gnu-gcc"`), because the default `http` feature pulls reqwest → rustls → ring and needs a C compiler for the target; `--no-default-features` is required, not optional | Wiring M6's new dependencies and *looking for* the gate rather than trusting the column | Both runners now invoke G6 with the working flag, and **SKIP it loudly and count it separately** when the target is missing rather than passing silently. §1's row corrected. The cost is stated: `--no-default-features` means G6 does not cover `cfg(feature = "http")` for Linux, which only CI's ubuntu job can | the runner itself; the verdict line now distinguishes green from skipped |
| B26 | **blocker (contract)** | **The frozen M6 contract made the TUI single-turn.** The keybinding table sent `Submit` only from `Status::Idle`, and `apply_event(Done)` set `Status::Done`; **nothing anywhere returned to `Idle`**, so after the first answer `Enter` was inert forever. A TUI that can hold one conversation is not the milestone | Stage A read the table against its own implementation and **reported the contradiction instead of patching it**, which is exactly what the "report, never silently deviate" rule exists to produce. The contract was wrong, not the code | The table now reads *status is not `Running`*, and §12 states the whole lifecycle in one place. `Esc` also stopped erasing a `Failed` reason, and a second `Tab` no longer discards the catalog the caller filled in | `a_second_turn_can_be_submitted_after_the_first_completes`, `a_failed_reason_survives_closing_the_picker`, `tab_twice_does_not_discard_the_catalog` |
| B27 | minor (process) | **The no-`mod.rs` convention had no enforcement** and M6's own plan broke it: §3 requires "no `mod.rs` files; `provider.rs` sits beside `provider/`", and the M6 stage table named `src/tui/mod.rs`. The scaffold created it, the suite stayed green, and nothing anywhere would ever have noticed | Reading §3's checklist *as a checklist* and running its commands by hand instead of assuming a green suite meant the list was satisfied | The TUI module became `src/tui.rs` beside `src/tui/`, matching `provider.rs`; both runners now assert the convention, so a future `mod.rs` fails a gate instead of silently joining the tree | `G3c module layout` in both runners |
| B28 | **major (criterion unmet)** | **No backend ever emits `StreamEvent::Usage`, so `pie -p --json` never carries token accounting.** ACCEPTANCE §5's frozen criterion says `turn.completed` ⇒ `Usage` **+** `Done`; `codex.rs` and `agy.rs` construct only `Done`. The event kind exists in the frozen `StreamEvent` set, `--json` is documented as emitting "the same StreamEvent serde shape", and the TUI's `apply_event` `Usage` row was dead code as a result — the status line would have claimed every turn cost nothing | Stage C, by grepping for constructions rather than assuming a type in the enum is a type on the wire. **A frozen criterion from M2 was an over-claim for ten milestones and no gate could have caught it**, because every M2 test asserts the router's *outcome*, not the event stream | the driver emits `Usage` from the finalised outcome, **after `Done`**. The complete figure only exists once the process has exited and `outcome()` has merged every partial report, so the driver is the only place holding it whole; `done` stays terminal for the *answer*, and `usage` is accounting. Buffering `Done` so `usage` could precede it was considered and rejected — it buys a cosmetic ordering for a new failure mode where an error path swallows an already-produced `Done`. All-zero usage is **suppressed**, because `Usage::merge`'s convention is that a zero field means "not reported" and emitting zeros would assert a turn cost nothing | `a_finished_agy_turn_streams_usage_after_done` and `a_finished_codex_turn_streams_usage_after_done`, both confirmed RED first (`saw ["init", "tool", "tool", "text", "done"]`) |
| B32 | **blocker (CI red)** | **Four `tests/parse_agy.rs` tests passed only on a machine with agy logged in, so `G4 acceptance (M1)` — M1's own named gate — was red on all three CI platforms while the local run was green.** `Agy::stream` calls `require_subscription()?` first, and `auth::agy_credential()` returns `Credential::Absent` when `$HOME/.gemini` is not a directory. The tests' `PinHome` helper pointed `PIE_HOME` at a scratch dir but left `HOME`/`USERPROFILE` alone, so on the developer's machine the real `~/.gemini/config/config.json` satisfied the gate and on a CI runner it did not. Each test then failed on `Err(AuthRequired)` where it expected `BinaryMissing`, `Timeout`, success, or a `Config` error naming the model. **Every document in this repository that treats CI's ubuntu job as the real Linux evidence was resting on a job that fails** | **Not a gate, and not the local suite: a sweep of every test binary with `HOME` redirected to an empty directory.** The trigger was a stage report noting almost in passing that an existing `stream` test "satisfies the gate by leaving `HOME` alone (the real `~/.gemini` exists here)" — a claim worth testing rather than accepting | `PinHome` now also points `HOME`/`USERPROFILE` at the scratch dir and installs a scratch `~/.gemini/config/config.json`, so the tests establish the state they assert about instead of inheriting it. `require_subscription` itself is unchanged: the gate is deliberate (§13 B14) and blocking a turn without a login is correct | the five tests themselves, plus the new **`G3d no vendor state`** gate, which runs the entire suite with `HOME` redirected so a future test cannot reintroduce this invisibly |
| B29 | minor (latent panic) | **A large `App::scroll` panics inside ratatui.** `render` hands `App::scroll` straight to `Paragraph::scroll`, and ratatui 0.29 then computes `y + area.height` and overflows: `attempt to add with overflow` at `paragraph.rs:483`. Measured boundary at 60x20 — last OK `65519`, first panic `65520` — i.e. it panics whenever `scroll + transcript_height > u16::MAX` | **Not a stage.** Found by the maintainer sweeping adversarial payloads across a width×height grid while verifying stage B's *different* panic claim. Stage B's own scroll tests used ordinary values | Clamp the offset inside `render` before handing it to `Paragraph` | a sweep over `App::scroll` including `u16::MAX`, at several sizes |
| B30 | minor (process) | **A dependency feature was enabled on an assumption nothing checked, and was never used.** `crossterm`'s non-default `event-stream` feature was added to `Cargo.toml` with a comment stating it was *required* for the loop to select over the terminal stream and the turn channel. `EventStream` does implement `futures_core::Stream`, but `futures-core` is only a *transitive* dep, so calling `.next()` needs `futures-util` — `use futures_core::stream::Stream;` fails `E0433`. The loop reads keys with `event::read()` on a detached thread instead, and nothing referenced the feature | Stage C, which tried to compile the dependency the maintainer had documented as necessary and reported `E0433` rather than working around it silently | Feature removed and the comment replaced with why it is absent and what was actually built. An unused feature is worse than no feature: the next reader treats it as load-bearing | the `Cargo.toml` comment plus the loop's own tests |
| B31 | minor (cosmetic) | The non-TTY refusal message reads `...needs a terminal on both stdin and stdout, and stdin and stdout is not one` — the placeholder is substituted with a compound subject, so the singular verb is wrong for the both-streams case | Reading `pie tui`'s actual output on a pipe during verification, which is the only place it is visible | Reworded so the sentence holds for each of the three cases; the three assertions pinning the old phrasing move with it | `a_pipe_on_either_end_refuses_to_start_the_tui` |

**B29 is the one worth dwelling on.** It was found by attacking the *same area* stage B had already tested and passed, with a grid sweep and payloads B had no reason to try. Stage B's own finding (the 2-column `WordWrapper` panic) was real and its guard is load-bearing — verified by dropping `MIN_WRAPPED_COLUMNS` to 1 and reproducing `index outside of buffer` at width 2, then restoring it. But two panics lived in one function, and the stage found one. **A stage's report is a statement about what it looked for, and independent verification is what turns "tested" into "tested by someone trying to break it".**

The two M6 entries that belong to the harness and the quota decision are recorded above (B24) and in the `wham/usage` commit respectively. **The pattern across B25–B30 and S12 is one thing, and it is the most useful result of this milestone.** In every case **the artefact existed and the thing connecting it to a failure did not**:

| Artefact that looked like coverage | What was actually missing |
|---|---|
| G6's row in §1, green in every run-log row | any runner that invoked it (B25) |
| §3's "no `mod.rs` files" checklist item | any check (B27) |
| a dependency feature documented as *required* | any code referencing it (B30) |
| M2's "`turn.completed` ⇒ `Usage` + `Done`" criterion | any test asserting the event stream rather than the outcome (B28) |
| `VERSIONING.md`'s changelog-and-tag rule | any signal, since none is mechanically decidable (S12) |
| the frozen keybinding table | anyone walking every row against the lifecycle (B26) |

A gate with no runner, a rule with no check, a contract table nobody traversed, and a criterion only ever asserted on a different surface all read as coverage. **The generalisable rule: for every claim, name the thing that would fail if the claim were false. If the answer is "nothing, but it is written down", the claim is not evidence.** Six of them were found in one milestone, from four directions — wiring a new dependency, reading a checklist, implementing a frozen table, and sweeping an already-tested function with adversarial input (B29). **None was found by a stage's own gate passing.**

| B33 | minor (artefact drift) | **The first G5 run failed, and the re-run is what caught it.** `codex_usage_probe_reports_buckets` asserted real quota buckets with no environment set, so it started failing the moment `codex.quota_probe` defaulted to `off` — the very decision it was supposed to help verify. It is the same shape as B26: a decision changed, and an artefact derived from the old one did not | Running G5, which the `wham/usage` change required — this is the existing rule "re-run a live gate after any change to the payload it proves" working as intended, not an accident | Split into a pair, because the two halves have different costs and only one needs money: the **default is off** is asserted offline in `tests/acceptance.rs` (no network, no credential — the disabled path returns before the credential is read), and **opting in still returns real buckets** stays live in `tests/live_smoke.rs`, where `run_pie_with_env` passes `PIE_CODEX_QUOTA_PROBE=http` explicitly | `the_default_codex_usage_probe_is_disabled_and_says_so` (offline) and `codex_usage_probe_reports_buckets_when_it_is_opted_in` (live) |
| B34 | major (contract vs code) | **`CliSnapshot::strings` has no producer, so the third leg of "REA-lite" never runs — and the obvious producer would be wrong.** `DESIGN` §17 and §16 both describe the degraded check as version + `--help` + binary strings. Probing the *resolved program* for codex would run `strings` over **node's** binary, because codex is `node <codex.js>` on this platform: a comparison that silently reports node's strings as codex's, which is the exact trap `help_command` exists to avoid | **Not a gate and not a stage's tests.** Found by running the built binary and reading its own output: `pie watch` prints the leg as inert, with that reasoning, in the report body. Stage D's process died before it could report the finding, and the artefact carried it instead | Recorded rather than faked. The report now states the limitation where a reader will see it; `DESIGN` §17 narrows the claim and documents the shape of the missing producer (`strings_command` beside `version_command`/`help_command`, extracting from the vendor's own artefact). A wrong comparison would have been worse than a missing one | the report-body note, asserted by `tests/watch_cmd.rs` |
| B35 | **blocker (gate)** | **`agy --help` writes its usage to stderr, so every real `pie watch` run failed — and the naive "fix" would have been worse than the failure.** Measured: 2905 bytes on **stderr**, 0 on stdout, because Go's flag package prints usage there. The probe captured stdout alone, so it saw an empty capture. Had it written that as a snapshot instead of erroring, it would have recorded an empty help text and reported "no drift" on every later run, forever, with nothing to notice | **The empty-capture rule added earlier in this same milestone.** Running the built binary against the real CLIs is what surfaced it — no stage's tests could, because every one of them used a stand-in that printed help to stdout | `capture` now falls back to the spawn summary's bounded stderr tail **when stdout produced nothing**, and only then: a CLI that writes to both has already given us the content, and appending the copy would put every line in the snapshot twice. The report header's REA legend was corrected in the same pass — it named "selected binary strings" while nothing produced them, and the header is the part a reader skims | `a_help_capture_that_arrives_on_stderr_is_still_a_capture`, whose stand-in mimics agy's own stdout/stderr split rather than a convenient one. Verified to fail on the reverted fix |

### M9

| # | Sev | Symptom | Caught by | Fix | Regression test |
|---|---|---|---|---|---|
| B36 | **major (silent data loss)** | **`drive` dropped every event a router owed beyond the first, and the event it dropped was a `tool_use`.** `EventRouter::on_line` returns **one** `Option<StreamEvent>`, but one claude `assistant` message can carry several content blocks (`text`, `tool_use`, `text`). Stage A wrote a faithful multi-block parser plus an inherent `drain_pending`, and the driver never asked for the rest — so a mixed message surfaced as `["text"]` and the call was gone. A lost `tool_use` is a conversation that never finishes | **A driver-level test the maintainer wrote after reading stage A's report**, which flagged the seam itself: `drive` spawns a real stand-in and asserts the sink sees all three blocks in order. It failed on the first run with `left: ["text"]`. The router's own 28 tests could not have caught it — they call the inherent method directly, which proves the router is faithful and says nothing about whether anything reaches the drain | `drain_pending` became a **trait** method (default `Vec::new()`, so agy and codex needed no change) and `drive` drains unconditionally after each line. `ClaudeRouter`'s was an inherent method that never overrode it, which is why the default silently won | `drive_drains_the_extra_blocks_a_single_assistant_line_carries` |
| B37 | minor (contract) | **The frozen M9 acceptance said to flip `tool_loop` to `true` once the loop existed — and implementing that showed the rule was wrong.** A capability answers "can this backend accept an injected `tool_result`?", and `Claude::stream` still returns `Unsupported`, so you cannot run a turn on claude at all. Flipping it would have had no effect today (`check_supported` gates a path nothing can reach) while telling every future reader that claude turns work | Wiring stage C, which is exactly when the contradiction became visible | Kept `false`, with the reason changed from "the loop does not exist" to "the turn path does not exist". §17's stage row and Checks bullet were corrected to match, and the test asserts the **pair** — flag false *and* the missing piece is the turn path — because either half alone can be satisfied by a lie | `the_tool_loop_capability_follows_the_turn_path_not_the_loop`, which also asserts agy and codex stay false |

**Two lessons this milestone produced that the earlier ones had not.** First, **a router's unit tests cannot see a driver that never calls it**: B36 lived precisely in the seam between two well-tested pieces, and the only thing that found it was a test at the *boundary*. Second, **a frozen rule can be wrong in a way only implementation reveals** — B37's rule read perfectly and was unsatisfiable against a backend whose turn path is unimplemented.

### Test harness

| # | Sev | Symptom | Caught by | Fix | Regression test |
|---|---|---|---|---|---|
| B24 | **blocker (process)** | **The gate hung silently instead of failing.** `EnvGuard::set` takes a plain `Mutex`, so a test that took a second guard while the first was still in scope blocked on its own thread forever. `cargo test` has no per-test timeout, so G3 stopped producing output and never returned; the visible symptom was a dozen *unrelated* tests (all the `paths`, `agy` and `config` suites that need the env lock) reporting "has been running for over 60 seconds". The milestone was unverifiable and the only signal was wall clock | Not a gate — the gate was the casualty. Noticed because the run was bounded and produced no verdict after 30 minutes. A second, sharper reading was possible and wrong: the same symptom is what a test making a real network call looks like, and `~/.codex/auth.json` *does* exist on this machine | `src/testing.rs` now tracks the lock per thread and **panics immediately**, naming the fix, when the same thread takes it twice. Another thread holding it is ordinary contention and still waits. A deadlock cannot be asserted, but the panic that replaces it can | `taking_a_second_guard_on_the_same_thread_panics_instead_of_deadlocking` (`#[should_panic]`), plus `scoped_guards_are_reusable_in_sequence` for the half a naive check would break |

**What this one cost, and why it is logged as a process bug.** The defect was introduced by the maintainer while making the `wham/usage` decision, and it was invisible in the working tree: `cargo test --lib config::tests::` would have run the offending test in a *different* filter order and may still have hung. The lesson is the one §1 already states about gates — a check that cannot report is not a check — extended to the harness: **a test binary that hangs is a gate that failed without saying so.** Bounding the run is what turned it into a diagnosis.

**The near-miss worth recording:** the first hypothesis was a live `wham/usage` call from the new opt-in test, because `HomeGuard` sets `HOME`/`USERPROFILE`/`PIE_HOME` to a temp dir while the real `~/.codex/auth.json` exists. That hypothesis was wrong — `require_subscription()` reads the guarded `HOME`, so the credential is genuinely absent and the probe stops at the auth gate — but it was worth checking before the fix, because "a test reached a private vendor endpoint" and "a test deadlocked" produce the same missing output.

### Findings that are not code bugs

| # | Observation | Action |
|---|---|---|
| F1 | **CLI drift**: agy reports `1.3.3`, but `knowledge/agy.md` documents `1.3.2`. This is precisely what I8 exists to catch | `knowledge/agy.md` and `knowledge/README.md` version updated; the flag/schema notes were left unchanged because nothing in the stream contract is known to have moved |
| F2 | codex resolves to `node.exe` + `codex.js` on this machine (no native `codex.exe`, and the `codex.cmd` shim cannot be spawned directly). The BINARY column therefore shows node | Documented in `codex.rs` and `PLATFORMS.md`; the launch path is the verified one |
| F3 | WSL2 is installed but cannot boot (`HCS_E_SERVICE_NOT_AVAILABLE`) | Linux is verified by G6 locally and by the CI matrix; see `PLATFORMS.md` |

## 14. Open suggestions from the QC sweep

Accepted as real, deliberately not done yet. Recorded here so they are decisions
rather than oversights. None affects a milestone verdict.

| # | Suggestion | Why it is deferred |
|---|---|---|
| S1 | `agy::build_plan` measures the argv against the configured binary *name* and `stream` then swaps in the resolved absolute path, so the measured length is not the spawned length | Harmless at the current numbers (20 000 cap versus a ~32 767 limit) and the measured value over-estimates rather than under-estimates. Fix when the cap is next touched |
| S2 | `println!`/`print!` in `commands/*` panic on a write failure, so `pie -p … \| head -1` exits 101 with a broken-pipe message | Rust ignores `SIGPIPE`; the fix is `writeln!` on a locked stdout ignoring `Err`. Cosmetic, but it is the cheapest remaining panic |
| S3 | `session_log::read_all` reads the whole log, and the log is append-only and never rotated | Correct today — totals must see every row, and truncating would make them silently wrong. Revisit only if a real log gets large, and use a tail read for the rolling windows rather than capping "all time" |
| S4 | The version snapshot in `commands/backends.rs` is a plain `fs::write`, so a crash mid-write leaves a truncated file | The reader tolerates a corrupt cache (it is a miss), so the consequence is one wasted probe |
| S5 | itsmini is configured by `PIE_ITSMINI_URL` alone, with a hardcoded `http://127.0.0.1:8000` default and a 120 s timeout constant, rather than by `[itsmini] url` in `config.toml` as every other setting is | Stage D shipped env-only resolution; wiring `Config` through `src/ocr.rs` and `commands/ocr.rs` is a small follow-up, and the env var works today. Worth doing before the MCP server (stage E) hardens the surface, since MCP config will want the same knob |
| S6 | `DESIGN` §11 names `agent.compact_after_turns`, but **no such config key exists** — the compaction threshold is a constant of 40 | A contract that promises a setting the code does not offer is the kind of drift that makes a doc untrustworthy. Either wire the key or stop naming it; stage F used the constant directly, which is the honest behaviour while the key is absent |
| S7 | On a **switched** turn, `compact::assemble` replaces stage B's per-turn framing, so a failed turn's `stop_reason` reaches the payload only via the state block instead of a `## turn N` marker | `src/compact.rs` is frozen by stage C's brief, so the composition was reported rather than patched. The consequence is a slightly coarser history on switched turns, not a correctness problem |
| S8 | `--switch <unknown>` yields `PieError::Config` while `--model <unknown>/x` yields `PieError::UnknownBackend`; both print `known: agy, codex, claude` and both exit 2, but the variants differ | Stage F followed its brief literally and reported the divergence instead of quietly choosing. User-visible behaviour is consistent; only the internal variant differs, so this is tidiness rather than a defect |
| S9 | `apply_event(Done)` sets `Status::Done` even when the vendor's own status string says otherwise. The authoritative terminal status is the loop's `Finished`, so a genuine failure arrives as `Failed` — but a backend that returned `Ok` with a non-success `Done` would render as done | The agy parser already maps `status: "ERROR"` to `PieError::Backend`, so the combination cannot arise from agy; teaching the TUI another backend's success vocabulary would put backend knowledge in the layer that deliberately has none (`DESIGN.md` §12). A test asserts the ordering that matters — `Finished(Err)` after `Done` ends `Failed` |
| S10 | `App::scroll` has no upper bound in the reducer; `render` clamps what it draws | How far down is *meaningful* depends on the transcript's rendered height, which only `render` knows. `reduce` clamps at zero from below, so a scroll past the end draws an empty window rather than panicking or drawing out of range. Recorded so the asymmetry is a decision rather than an omission |
| S11 | `tests/tui_render.rs` briefly failed to compile while stage B was mid-edit (`usize == u16`), which broke the *other* stages' `cargo clippy --all-targets` and would have failed the full gate | Not a defect — it is the documented cost of three stages sharing one checkout, which is why every M6 brief carries "a transient compile error may not be yours". Stage A reported it without touching the file, which is the behaviour the line is there to produce. Recorded because a real defect could wear exactly the same clothes, so the distinction should be visible in the log rather than resolved by memory |
| S12 | **`VERSIONING.md`'s changelog and tagging rules are unenforced, and both were unmet.** The `[Unreleased]` block was empty while M1–M6 had landed, and `git tag` was empty although the same file says to tag each release. The `[Unreleased]` block is populated as part of closing M6, so the rule is now satisfied for this state of the tree | **No gate is added, deliberately.** "Was the changelog updated as work landed" is not mechanically decidable from the repository, so the only cheap checks available — that an `[Unreleased]` heading exists, or that the top released version matches `Cargo.toml` — would pass for the exact tree that was wrong. A gate that cannot fail the thing you care about is the defect `ACCEPTANCE.md` §1 and M5's two decorative gates are about, so adding one here would make the situation worse by lending it credibility. The honest handling is a checklist item at release time: **promote `[Unreleased]` and tag in the same commit**, and treat an empty `[Unreleased]` above a non-empty commit range as the signal |
| S13 | The TUI's picker overlay is 50% of the frame, and a merged agy+codex catalog can exceed it, so the list scrolls with **no indicator that it is scrollable** | Verified not to be a selection bug: ratatui 0.29's list rendering auto-scrolls to keep the highlighted row visible, so a `PickerAccept` can never take a row the user could not see. What is missing is only the affordance, which no M6 criterion requires. Left for a follow-up pass rather than added under a milestone that is closing |
| S14 | `Agy::stream` calls `require_subscription()` **before** `build_plan`, so on a machine with no agy login a purely static argv error (`--effort` the model does not offer) is reported as `AuthRequired` rather than as a config error naming the model | Deliberate ordering — the gate is the security-relevant check and runs "before anything expensive" (§13 B14) — but it does mean a config error is unreachable without a credential. No correctness impact: both exit non-zero and name a fix, and a user with no login cannot use the backend at all. Recorded because it is the kind of precedence that looks like a bug when a test asserts the config error and gets the auth one, which is exactly what B32 hit |
| S15 | **`render_report`'s frozen signature has no channel for news-source failures**, yet §17 and §16 both require the report to say the digest is incomplete. Stage C met the requirement by encoding the failure as a `NewsItem` carrying a sentinel (`FAILED_SOURCE_MARK` + `failed_source()`), and `render_report` partitions on it | The requirement **is** met and tested, so this is a wart rather than a defect — but it conflates two types, and a future caller that forgot to partition would render a failure as a headline. Cleanup: add `failed: &[NewsSource]` to the signature. The same shape applies twice more in that function: `Drift` carries no `file` field, so the pie file a `pie-must-change` needs is derived from `backend` through a table, and there is no `mode` parameter, so the REA mode is an exported constant rather than an argument. All three are worth one follow-up pass together |
| S16 | **`cargo fmt --all` rewrote an in-flight stage's test file mid-run.** Stage C's prescribed gate reformatted `tests/news.rs` while stage B was still editing it, which is B7's mechanism one level down: a repo-wide formatter is a writer on every file, so it cannot be part of a *stage* gate while other stages hold files | Both stages noticed and worked around it (formatting their own files with `rustfmt -- <paths>`), and the final tree is formatted. The general rule, now written into the M8 briefs: with parallel stages, a stage formats only the files it owns |

---

## 15. M7 — daily learn + schedule

Contract: `DESIGN.md` §16. Done condition from `ROADMAP.md`: *"after a few sessions,
`profile.toml`/`memory/*.md` reflect real habits; `/memory` shows what pie learned and
lets you revert it"*.

> Appended as §15 rather than inserted after §9, to avoid renumbering the
> cross-references these documents already make to existing sections.

**Acceptance, per stage.** Every stage gets its **own** gate named after its **own**
test binary. M7 has four stages and four distinct binaries for exactly that reason —
M5's two decorative gates and M6's stage D both came from a gate that ran a *different*
stage's tests, and M7 is the largest surface yet.

| Stage | Command | Shape |
|---|---|---|
| A extract | `cargo test --test learn` | recurrence counting, explicit-preference recognition, stack/terminology **from prompt text**; **pure** — no spawn, no clock read, deterministic across two calls on the same input |
| B consolidate | `cargo test --test consolidate` | merge/dedupe against the existing doc, confidence decay outside the window, provenance retained; `apply` returns a journal and `revert` restores the pre-apply bytes exactly |
| C schedule | `cargo test --test schedule` | `due()` at and around its boundary; the budget refuses *before* spawning; the ledger's `spent` prevents a second spend in the same window |
| D commands | `cargo test --test memory_cmd` and `cargo run -- memory` | `pie learn`/`memory`/`schedule` render; `--dry-run` writes nothing; `pie memory revert <ID>` undoes a real apply; TUI `/memory` routes through `Submit`, not a new `Input` |
| G5 | live, by hand | run A–D on **real** sessions and read the artifacts |

**Checks**

- [ ] **the cross-cutting gate, asserted rather than described:** with the **default**
      config a scheduled run performs **zero** provider spawns — the *count* is
      asserted, using the counting-stand-in pattern of `tests/cache.rs`, not merely
      the absence of an error
- [ ] a job whose estimate exceeds the budget is refused **before any spawn**, so a
      budget of `0` cannot be "exceeded by one call"
- [ ] model consolidation is **off** by default: `pie learn` with the default config
      spawns nothing and `grep`ping the run for a backend invocation finds none
- [ ] `pie learn --model b/m` without a configured budget is a **config error**, not a
      silent spend
- [ ] the default path is deterministic: two `extract` runs over the same turns produce
      byte-identical candidates, so a report can be diffed
- [ ] **extraction reads no vendor store and no answer text.** The roadmap says "stack
      from prompt text and tool names", but no session row records tool names (`TurnRow`
      has its nine frozen keys and no `tools` field), so `Stack` comes from prompt text
      only — recorded here rather than left as a silent shortfall. The model's own
      vocabulary is deliberately **excluded**: what the model says is not evidence about
      what the user does, and that exclusion is asserted by name
- [ ] every learned line carries provenance (session id + turn index) and `pie memory
      show` prints it. A merged candidate names the **newest** observation, ties broken by
      `(session_id, turn_index)`, because one provenance field cannot name them all and an
      accident of iteration order is not a rule
- [ ] `hits` counts **turns**, not occurrences: one mention repeated three times in one
      turn is one observation, or a single verbose prompt outranks a real pattern
- [ ] decay **demotes**, it never deletes: a fact outside the window is still present
      with lower confidence, asserted by re-reading the doc
- [ ] `apply` then `revert` restores the pre-apply bytes **exactly** (byte comparison,
      not "looks right")
- [ ] consolidation never reorders the head of the projection: assert the projection's
      first N bytes are unchanged across a consolidation (`TOKEN-ECONOMICS.md` §3)
- [ ] no M7 path writes a vendor store, and no M7 path reads one unless the config
      opts in — assert with a temp `HOME` that `~/.gemini`, `~/.codex` and `~/.claude`
      are untouched after a full run
- [ ] `pie learn`, `pie memory` and `pie schedule` write no session-log row and spend
      no model quota (I7)
- [ ] a scheduled run that fails mid-apply leaves the memory doc **readable** — the
      journal is written before the doc is touched, so a crash is recoverable by
      `revert` rather than leaving a half-written file
- [ ] `no mod.rs files` still holds: `src/watch.rs` + `src/watch/{news,drift}.rs` is the
      M8 shape, mirroring `provider.rs` + `provider/*.rs`. This was an **unenforced**
      convention until M6 broke it and the suite stayed green — see the bug log

**Not verifiable here**: whether the learned preferences are *good*. Extraction is
rule-based and its output is inspectable, but "does this reflect how I actually work"
is a judgement for the user, and M7's verdict says so rather than claiming quality.

**Also not verifiable here**: importing the vendor session stores end-to-end. Checked
rather than assumed, 2026-10-12: `~/.gemini/antigravity-cli/history.jsonl` and
`conversation_summaries.db` exist with real data, `~/.codex/history.jsonl`,
`session_index.jsonl` and `sessions/` exist with real data, and `~/.claude/` exists but
holds only `backups/`, `cache/`, `ide/`, `sessions/`, `settings.json` — **no history and
no credential**, consistent with there being no Claude subscription on this machine.
So the agy and codex importers have real material to be verified against, and the
claude importer has **none**: it is fixture-tested only. Say so; do not let a green
suite imply a claude import that has never seen a claude session. Note also that
these imports are **opt-in** by design, so the shipped default never reads any of them
and the default-path test is `zero reads`, not `reads one of them`.

---

## 16. M8 — monthly watchtower

Contract: `DESIGN.md` §17. Done condition from `ROADMAP.md`: *"
`~/.pie/reports/watchtower-YYYY-MM.md` produced + notified; drift diff shows any
flag/schema/policy change"*.

> Appended as §16 rather than inserted after §9, to avoid renumbering
> cross-references.

**Acceptance, per stage.** Four stages, four distinct test binaries — for the same
reason M7 has four.

| Stage | Command | Shape |
|---|---|---|
| A drift | `cargo test --test drift` | `diff` over two snapshot fixtures: a renamed flag, a changed schema line and a changed version each yield a distinct finding **with the right class**; an identical pair yields **none**; `render_report` with `first_run` says "nothing to diff" and never "no drift found" |
| B news | `cargo test --test news` | source fixtures parsed (**no network in any test**); rule-based impact ranking against a configured stack — a stack match outranks a non-match, and an empty stack ranks nothing above zero; an unreachable source is a stated error, never an empty digest |
| C report | `cargo test --test watchtower` | the file is named from the **injected** month, not the system clock; the report contains every finding with its class; an unclassifiable finding is reported as `unclassified`, never as `compatible`; the REA mode is labelled |
| D command | `cargo test --test watch_cmd` | `pie watch` with the default config fetches nothing and spawns no model; `--dry-run` writes no file; a real run writes exactly one file under `$PIE_HOME/reports/`; a network failure is reported rather than swallowed |
| G5 | live, by hand | probe the **real** agy and codex, take a real snapshot, and diff a real `--help` change |

**Checks**

- [ ] `--help` probing costs **no** model tokens: assert the probe argv contains no turn flag, and that a probe run logs no session row (I7) — this is the claim that makes a *scheduled* drift job safe at all
- [ ] `render_report` never reads the clock; the month is a parameter, asserted by rendering two different months from one call site
- [ ] an unclassifiable drift entry is `Unclassified`, and `suggestions`/`Compatible` is never the fallback — assert with a deliberately unknown finding
- [ ] the first run (no previous snapshot) reports "first snapshot taken", and a test asserts it is **not** the string "no drift" (the two are indistinguishable in a findings-only report, and only one is true)
- [ ] a digest that lost a source says so in the report, and the run exits non-zero rather than writing an empty digest as success
- [ ] the report classifies findings as `compatible` / `pie-must-change` / `pie-must-stop`, and a **`pie-must-change`** finding names the pie file that must change, while a **`pie-must-stop`** finding contains no route around the measure (asserted by absence of "skip"/"disable"/"bypass"/"workaround" wording, so the requirement is enforced rather than remembered). The first draft put the file-naming clause on `pie-must-stop`, which is the wrong class: a policy change is where pie *stops*, and a shape change is where a file must follow
- [ ] **a probe that returns empty `--help` is a stated error naming the backend**, never a quiet snapshot — an empty capture is unrepresentable in `render_report` and would read as "no drift"
- [ ] **an empty capture and an empty feed are both stated errors, not empty successes** — the same rule M5's `ocr` shipped, applied to two new inputs. A valid feed that genuinely carries no `<item>`/`<entry>` is also an error, distinguished in its message from "answered with N items, none carrying a title", so a quiet source is diagnosable rather than invisible
- [ ] nothing reads or writes vendor **config** (`~/.codex/config.toml`, `~/.claude/settings.json`) — the §10/§7 static check covers `src/watch/**` too
- [ ] `pie watch` writes no session-log row (I7), and with the default config nothing is due
- [ ] the budget gate is shared with M7: one mechanism, two consumers, and a test proves the news digest cannot spawn a model without a configured budget
- [ ] `no mod.rs` holds: `src/watch.rs` beside `src/watch/{news,drift}.rs`

**Not verifiable here**: the full structural REA check, because no disassembler is
installed (`RISKS.md` §14). The shipped path is "REA-lite" and labels itself as such;
the structural branch is compiled and fixture-tested but has never run against a real
binary. Say so; do not let the report imply a structural comparison it did not make.

---

## 17. M9 — claude adapter + the tool loop

Contract: `DESIGN.md` §18. Done condition from `ROADMAP.md`: *"Implement claude adapter to
spec; verify live when a subscription exists. Build `tool_use`→`tool_result` tool-loop
(reused by HTTP backends)"*.

> Appended as §17 rather than inserted after §9, to avoid renumbering cross-references.

**The verdict for this milestone is `offline-verified`, never `done`.** `~/.claude/` exists
but holds no credential and no history (checked 2026-10-12), so there is no Claude
subscription on this machine and no live turn is possible. A green suite here is evidence
about pie's protocol handling, not about claude.

**Scope note, so the stages do not expand.** The milestone asks for the **loop**, not for a
tool suite: the loop takes a `ToolRunner` and ships no concrete tools. `PLAN.md` §2's
`agent/tools.rs` (read/bash/edit/write/grep/find/ls) is a separate concern with its own
design questions — what a `bash` tool is allowed to do unattended is not a decision to make
in passing — and inventing one here would put uninspected code on the paid path.

### The interface problem this milestone has to solve first

`StreamEvent::ToolStep { name, state, info }` cannot drive a loop: it carries no
`tool_use_id` and no tool `input`, and both are required to answer (`tool_result` must
name the id it answers, and the runner needs the arguments). The router therefore needs a
way to surface a call, and the candidates were:

| Option | Why not |
|---|---|
| Widen `ToolStep` | It is the *display* event the M6 TUI renders and its shape is asserted in `tui_app`/`tui_render`. Widening it to carry a JSON payload would put tool arguments into the transcript for no reader. |
| A side channel in the claude router | Invisible to `--json`, untestable through the public surface, and M10's backends would need a second one. |
| **Add `StreamEvent::ToolUse { id, name, input }`** | Chosen. Additive, serialised like every other event so `--json` carries it, and the compiler names every match that must handle it — which is exactly how the TUI's `apply_event` gets updated rather than silently ignoring calls. |

Adding a variant is a compile-time-visible change to a frozen enum, which is the good kind:
the four `StreamEvent` matches across the tree fail to compile until each has decided what a
call means to it. The TUI renders it as a tool row (the vendor's call, per
`VENDOR_TOOL_NOTE`); `driver` and the tests ignore it.

**Acceptance, per stage.** Three stages, three test binaries, one per stage.

| Stage | Command | Shape |
|---|---|---|
| A claude stream | `cargo test --test parse_claude` | the recorded `stream-json` envelope parses: `system`/`init` ⇒ `Init`, `assistant` content blocks ⇒ `TextDelta` **and** `ToolUse` (a message may hold both), `user`/`tool_result` ⇒ `Other` (pie does not answer its own calls), `result` ⇒ `Usage` + `Done`; a multi-byte character at an awkward offset does not panic; garbage tolerated (I5) |
| B the loop | `cargo test --test tool_loop` | against a scripted child: a `tool_use` is answered with a `tool_result` naming **its own id**; two calls yield two results in order; a result for an unknown id is a stated protocol error; the iteration cap stops a model that calls tools forever **and says so**; the loop is refused on a backend whose `capabilities().tool_loop` is false |
| C capability + wiring | `cargo test --test parse_claude` and `cargo test --test tool_loop` | `build_args` gains the `--input-format stream-json` shape the loop needs, without changing the `-p` one-shot path; `Claude::capabilities().tool_loop` is asserted to follow the **turn path**, not the loop |
| G5 | **n/a — stated, not skipped** | no Claude subscription on this machine. The live test exists and is `#[ignore]`d with that reason, so the day a subscription appears it runs rather than being reinvented |

**Checks**

- [ ] **`tool_loop: true` is set exactly when the backend's `stream` can answer a call.** Stage C originally said the flag flips once the *loop* exists; implementing it showed that rule was wrong. The loop exists and lives in `src/tool_loop.rs`, but `Claude::stream` still returns `Unsupported` — you cannot run a turn on claude at all — so the flag stays `false`, and the test asserts the **pair** (flag `false` *and* the turn path is the missing piece), because either half alone can be satisfied by a lie
- [ ] a `tool_result` names the `tool_use_id` it answers; an unknown id is a stated protocol error, never a dropped frame
- [ ] **the loop cannot deadlock**: the child's stdin is written while its stdout is being read, asserted against a stand-in that will not read stdin until it has written its whole output — the classic two-pipe deadlock, and the assertion is that the test *finishes*
- [ ] the loop is **bounded** and the cap's effect is reported, not swallowed
- [ ] asking for the loop on a delegate-mode backend is a **config error**, not a hang — asserted for agy and codex, since `RISKS.md` §1 is why they cannot accept an injected result
- [ ] no stage spawns a real `claude`: every test drives a stand-in, so the whole milestone is machine-independent (gate G3d)
- [ ] the `-p` one-shot path is unchanged: its argv is asserted byte-for-byte, because the loop's `--input-format` must not leak into the delegate path

**Not verifiable here**: everything about claude itself. The envelope in
`knowledge/claude.md` is documented as *expected* rather than captured, so the parser is
tested against a fixture derived from that document — not from a real session. Say so in
the verdict; a fixture written from a specification is not evidence that the specification
is right, and M9's row must not read as though claude had been exercised.

---

## 18. M10 — the API-key backends

Contract: `planning/DEFERRED-BACKENDS.md` (the design, written when the milestone was
deferred) and `DESIGN.md` §19 (the three decisions this milestone owes). Done condition
from `ROADMAP.md`: *"Uncomment the two HTTP backends (SSE + tool_calls + models + usage)"*,
constrained to *only backends where `cost_usd_micros` is real*.

> Appended as §18 rather than inserted after §9, to avoid renumbering cross-references.
> `DEFERRED-BACKENDS.md` asks for this section to exist **before** the code, which is what
> this is.

**The verdict here is `offline-verified` unless the user supplies keys.** A live turn spends
real API credit in a currency nobody opted into, so no stage makes one; the transport is
tested against a scripted client and the wire against recorded frames. G5 is therefore
`n/a — needs the user's key`, stated rather than skipped, with the live test written and
`#[ignore]`d.

### The three decisions, and the invariant they sit next to

These are the first backends that **legitimately** read an API key, and the first that are
**not** subprocesses. Both facts cut against invariants the other nine milestones hold:

| Decision | The rule |
|---|---|
| **A key is expected here, and only here** | I9 ("subscription-only auth, never an API key") is an invariant about the *CLI* backends. These need the key in the request and must never be handed a vendor CLI's credential. A **missing** key is a stated error naming the environment variable — never a fallback to another backend, because "answered with a different subscription than the one asked for" is the failure `resolve_target` was written to prevent |
| **`cost_usd_micros` is real again, so the column returns** | M4 removed the cost column because every subscription backend reports zero, and a permanent `$0.00` implies cost is measured when it is not (`TOKEN-ECONOMICS.md` §10c). That reasoning cuts the other way here: a real figure renders, and **an absent one stays absent** rather than being rendered as zero |
| **The tool loop comes from M9, not a second copy** | `RISKS.md` §8: this is the path delegate-mode CLIs never exercise, and it is why M9 built it. A backend that needs `tool_calls` and finds no loop is an error, not a degraded path |

**The feature boundary is part of the contract.** `reqwest` sits behind the default-on `http`
feature, and G6 runs `cargo check --no-default-features`. So the two backends — module,
registry entry, and id — are gated on that feature, and everything asserting the backend
list is feature-aware. Without this, G6 breaks the moment they are registered, and the
break would look like a cross-target problem rather than a feature-gating one.

**Acceptance, per stage.** Three stages, one test binary each.

| Stage | Command | Shape |
|---|---|---|
| A the wire | `cargo test --test sse` | SSE frames ⇒ `StreamEvent`s, **incrementally** (a frame boundary lands wherever the network puts it): `data:` payloads, `[DONE]`, comments and event names ignored; `delta.content` ⇒ `TextDelta`; `delta.tool_calls` ⇒ `ToolUse`, whose arguments arrive as a **string accumulated across frames** and must be parsed once complete; usage and `cost_usd_micros` mapped when reported; a mid-JSON cut-off, a non-JSON payload and a stream with no `[DONE]` each decided explicitly (I5) |
| B the transport | `cargo test --test http_transport` | M9's `Transport` over HTTP: `send` is a stateless POST carrying the conversation so far (the real difference from a child process); a non-2xx is a stated error naming the status and a **redacted** body; a failed request is an error, never an empty stream; **the key appears in no output, error, or formatted value** |
| C the backends | `cargo test --test acceptance` and `cargo test --lib provider::` | two `Provider` impls over **one** parameterised core (DEFERRED-BACKENDS calls DeepSeek "identical apart from the two constants", so two near-duplicate files would be two places to drift); **registered unconditionally**, because `http::live_client` has a `not(feature = "http")` arm that answers with a stated error naming the missing feature — so the backend is present and tells the truth rather than vanishing from a build where the user cannot tell why; `auth_status` reports `NeedsLogin` naming the variable |
| G5 | **`n/a — needs the user's key`** | stated, not skipped. The live test is written and `#[ignore]`d with that reason |

**Checks**

- [ ] **the key is read from the environment at call time**, never from `config.toml`, never written under `~/.pie`, and never in an error, log line, or `Debug` output — asserted by running a request with a recognisable key and grepping every string the code produces for it
- [ ] `tests/no_secrets.rs` still passes, which it does automatically for a new backend because it scans the whole tree
- [ ] a **missing** key is a stated error naming the environment variable, and **not** a fallback to another backend
- [ ] a non-2xx response names the status and a redacted body — never a header, never the key
- [ ] `cost_usd_micros` is non-zero when the endpoint reports a cost, and **absent rather than zero** when it does not
- [ ] the tool loop is M9's: assert the backend's turn path goes through `tool_loop`, not through a second implementation
- [ ] G6 still passes: the modules are declared unconditionally and `http.rs` gates `reqwest` internally, so `--no-default-features` compiles and a turn on a no-TLS build is a **stated error naming the missing feature** rather than a backend that answers nothing
- [ ] the backend table is honest about what each row is: an endpoint backend has **no binary**, and showing one as "not found" tells the reader two wrong things at once — that a binary was expected and that this machine lacks it
- [ ] no test reaches the network, and the whole suite is green with `HOME`/`USERPROFILE` pointed at an empty directory (gate G3d)

**Not verifiable here**: any live turn, because no key is configured on this machine and a
live one spends real credit. The transport is tested against a scripted client and the wire
against constructed frames, so the verdict says which half ran — and a constructed frame is
not evidence that a provider's real stream matches it.

---

## 19. M11 — the tool suite for the loop

Contract: `DESIGN.md` §20. New scope — the roadmap ends at M10 — so this section exists
**before** the code, like every other milestone's.

### The constraint that shapes every criterion

**M9's loop has no user in it.** There is no channel on which to approve a command mid-turn, so
a tool is enabled in config with a declared scope or it is not offered. Every criterion below
is downstream of that one: the tests are about *config* deciding, not about a prompt appearing.

**Acceptance, per stage.** Three stages, one test binary each.

| Stage | Command | Shape |
|---|---|---|
| A read-only tools | `cargo test --test tools` | `read`/`ls`/`grep`/`find` over a fixture workspace: a file reads; a directory listing is stable; a grep finds a line and reports its path and number; **a path resolving outside the configured workspace is refused**, asserted for `..` and for a symlink, because a textual prefix check passes both; a disabled tool is refused with a message naming the **config key** that would enable it |
| B bash, scoped | `cargo test --test tools_bash` | `bash` is **off by default**: with no config, a `bash` call is answered with an error result and **no spawn** — asserted by a counting stand-in, because "no error" is what a decorative gate returns; a command whose `argv[0]` is not in `bash_allow` is refused **before** the spawn; a command outliving its deadline is killed rather than waited on; an output that never ends is bounded rather than accumulated |
| C one spawn policy | `cargo test --test tool_loop` and `cargo test --test process` | `ChildTransport` is **deleted** and `process.rs` grows the piped-stdin case that made it necessary, so there is one spawn path rather than two; the I2 env-scrub assertion that M10 added to the loop's copy now covers the single path, and the count of spawn-policy copies is **one** — asserted, not asserted-by-absence |
| G5 | **`n/a` — stated, with the reason** | no backend that uses pie's loop has a working credential here: claude has none, and M10's backends need a key. The tool implementations are local, so they need no vendor at all — which is why this milestone's substance is fully testable offline and only the *end-to-end* turn is not |

**Checks**

- [ ] **with an empty config, no tool runs at all** — the default is exactly today's behaviour, so this milestone cannot make an existing setup suddenly executable; asserted by a counting stand-in, not by an error being absent
- [ ] a disabled tool is **answered**, never ignored: the loop must not leave a model waiting for a result that never comes (M9's rule), and the answer names the config key so the model can proceed with what it has
- [ ] `bash_allow` is checked on `argv[0]` **before** the spawn, and the refusal is a stated error rather than a `PermissionDenied` the model has to interpret
- [ ] an escaping path is refused on the **resolved** path, asserted for `..` and a symlink
- [ ] a command outliving its deadline is killed, not waited on (`RISKS.md` §6)
- [ ] a runaway output is bounded rather than accumulated (`B22`'s lesson, applied to a new input)
- [ ] the spawn policy exists **once**: `ChildTransport` is gone, and the test that once guarded its copy now guards the single path
- [ ] no tool writes outside the workspace, asserted by a fixture tree that is byte-compared before and after a full stage run
- [ ] nothing in the tool path reads a vendor store, a credential, or the network

**Not verifiable here**: a *live* end-to-end turn that calls a tool, because no backend with a
working loop has a credential on this machine. The tools themselves need no vendor, so the gap
is narrower than it looks — but it is a gap, and the verdict says so rather than implying a
model was driven.

**The two halves that were stubs are now real.** M10 shipped with `list_models` and `usage`
returning `Unsupported`, because the shared `HttpClient` was POST-only by construction and
neither vendor exposes a quota window. The follow-up added `get` to that trait (required, not
defaulted — a default would let a client silently lack it) plus a **bounded** body reader,
which unblocked both: `list_models` reads the vendor's own `GET /models`, and OpenRouter's
`GET /key` supplies the only quota-like figure either API-key vendor has — a **prepaid balance**
rather than a window, with no reset time, and `None` when the key is unlimited because a zero
would render as "nothing left". DeepSeek still reports no bucket, which is the honest answer
there rather than an invented one. The mapping is **pure** (`models_from_catalog`,
`credit_bucket`) so the shapes are tested without a network; the transport's own tests cover
the HTTP half.
