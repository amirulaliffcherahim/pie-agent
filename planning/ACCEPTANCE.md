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
| I2 | API-key env vars never reach a child | `env_hygiene` test: deny-list removed, `GOOGLE_APPLICATION_CREDENTIALS` and `CLOUD_CODE` preserved |
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
- `usage agy` prints quota buckets with remaining fraction + reset time, and the
  run makes **no** model call (it is a slash expansion)
- `--model` without `--effort` is auto-filled from config (`medium`), never errors

**Checks**

- [ ] `--disable-slash-commands` present for turns, **absent** for `/usage` (deliberate asymmetry)
- [ ] `--print-timeout <N>s` always passed; `0` means wait for completion
- [ ] model catalog read from `agy models` and cached with 24h TTL; a cache hit makes no CLI call
- [ ] hidden spawn — no console flash on the live run (I1)
- [ ] timeout ⇒ process tree killed, `PieError::Timeout`, exit 5 (test with a tiny timeout)

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
- `usage codex` prints 5h + weekly buckets from `wham/usage`
- the token appears in **no** output, error, or log line (I3 — grep the captured output)
- refusal path: with an API-key-only credential the backend reports
  `NoSubscription`, exit 3, instead of silently billing (I9)

**Checks**

- [ ] never spawns `codex.cmd`; resolves `node <…>/codex/bin/codex.js` (or native `codex.exe`) (DESIGN §9)
- [ ] `--approve-for-me` and `--sandbox` are never both passed (they are mutually exclusive)
- [ ] `--skip-git-repo-check` always passed (pie runs in non-repo workspaces)
- [ ] auth gate reads `auth_mode` from `~/.codex/auth.json` and requires subscription
- [ ] `~/.pie/auth.json` (legacy, holds extracted tokens) is never read — assert with a test that the path is untouched/no reader exists

## 6. Bug protocol

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

## 7. Independent verification

Each milestone, after G1–G3 are green and before it is called done, dispatch an
independent reviewer subagent with **no shared context** and give it: the design
contract, the CLI knowledge file, and the diff. It must be told to try to refute
the milestone, not to confirm it. Findings come back as rows with severity; each
is either fixed (with a test) or logged as accepted with a written reason.

The reviewer must not be the same agent that wrote the code.

## 8. Run log

Appended as gates pass. Evidence = command + observed result, not a claim.

| Milestone | G1 | G2 | G3 | G4 | G5 | G6 | Verdict | Evidence |
|---|---|---|---|---|---|---|---|---|
| M0 | ✅ | ✅ | ✅ | ✅ | n/a | ✅ | **done** | 113 tests pass (100 lib + 6 `no_secrets` + 7 `process`), 5 live-smoke tests correctly ignored; `--list-backends` prints agy/codex/claude with agy `1.3.3`, codex `codex-cli 0.162.0`, claude `2.1.296` + `no-subscription`; `cargo check --target x86_64-unknown-linux-gnu --all-targets --no-default-features` clean |
| M1 | — | — | — | — | — | — | not started | — |
| M2 | — | — | — | — | — | — | not started | — |

Verdict values: `done` (all applicable gates) · `offline-verified` (G5 skipped by
user decision) · `blocked` (a gate fails) · `not started`.

## 9. Bug log

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

### Findings that are not code bugs

| # | Observation | Action |
|---|---|---|
| F1 | **CLI drift**: agy reports `1.3.3`, but `knowledge/agy.md` documents `1.3.2`. This is precisely what I8 exists to catch | `knowledge/agy.md` and `knowledge/README.md` version updated; the flag/schema notes were left unchanged because nothing in the stream contract is known to have moved |
| F2 | codex resolves to `node.exe` + `codex.js` on this machine (no native `codex.exe`, and the `codex.cmd` shim cannot be spawned directly). The BINARY column therefore shows node | Documented in `codex.rs` and `PLATFORMS.md`; the launch path is the verified one |
| F3 | WSL2 is installed but cannot boot (`HCS_E_SERVICE_NOT_AVAILABLE`) | Linux is verified by G6 locally and by the CI matrix; see `PLATFORMS.md` |
