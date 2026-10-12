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
| G6 Cross-target check | `cargo check --target x86_64-unknown-linux-gnu --all-targets` | compiles |

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
- No API/HTTP backend may appear (neither is in the registry).
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
- `usage codex` prints the quota window(s) from `wham/usage`
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
- [ ] `codex.quota_probe = "off"` constructs **no** HTTP request and still lets
      `pie usage codex` print session-log totals
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
| D picker + keys | `cargo test --test tui_app` | the documented keybindings each produce their action; choosing a model updates `App::model`; every keybinding is documented in help |
| G5 | live, by hand | drive a **real** turn's `StreamEvent`s through `apply_event` and assert the rendered buffer contains the answer, then `pie tui` starts and quits cleanly |

**Checks**

- [ ] the reducer takes no crossterm type — `src/tui/app.rs` has zero `crossterm` imports
- [ ] `render` is asserted through `TestBackend`, so it is tested without a terminal
- [ ] the UI does not block on a turn: events arrive while the loop still accepts input
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
