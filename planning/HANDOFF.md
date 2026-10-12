# Handoff — resuming `pie` development

> **Purpose.** A fresh session should be able to continue from here without
> re-deriving anything. Read this, then `planning/ROADMAP.md` for what to build and
> `planning/ACCEPTANCE.md` for how it is judged.

## 1. State at handoff

| | |
|---|---|
| Repo | `C:/Users/amiru/Local/Code/pie` |
| Branch | `feat/m0-m2-subscription-backends` (pushed; `origin` in sync) |
| Last commit | `9f8ed4a` |
| Tests | **392 passing**, 5 live correctly ignored |
| Gates | **15/16 green** — G5 is run by hand, never by the runner |
| Milestones | **M0, M1, M2, M3, M4, M5 done**, each with a run-log row and per-stage gates |
| Next | **M6 (TUI)** — dependencies added, contract frozen, no code written |

Build the binary before manual checks: `cargo build --release` →
`./target/release/pie.exe`.

## 2. What `pie` is

A Rust CLI that drives three subscription-locked vendor CLIs headlessly
(`agy --print`, `codex exec --json`, `claude -p`) and normalises their streams. It
**never** extracts credentials and **never** calls a vendor API for inference. agy
and codex are one-shot *delegate mode*: the vendor runs its own tools and returns a
final answer, so pie cannot inject a tool result (`RISKS.md` §1).

**Declined deliberately, do not build:** imitating the vendor clients to evade
detection. The user asked for it once; the answer is in `knowledge/9router-findings.md`
§0 and `planning/PLAN.md` §5. 9router's own authors mark those providers
`deprecated: true, deprecationNotice: "RISK_NOTICE"` = *"Account may be restricted or
banned."*

## 3. The working discipline (this is what produced the results)

1. **Freeze the contract before code.** `DESIGN.md` for the mechanism,
   `ACCEPTANCE.md` for the criteria. A milestone with no frozen criteria is one whose
   verdict gets written from memory — that is exactly how M4's verdict went missing
   for a while.
2. **Split a milestone into independently testable stages**, and give **every stage
   its own gate named after its own test binary.** A stage gate that runs another
   stage's tests is decorative; that happened twice in M5.
3. **Verify the agents' work yourself.** Gates plus the specific acceptance claims.
   An agent's summary describes intent, not outcome.
4. **Run G5 by hand** — it spends real subscription quota, on `agy/gemini-3.8-flash`
   and `codex/gpt-6-luna` only. `tests/live_smoke.rs` asserts the allowlist and
   refuses anything else.
5. **Commit per milestone** (or per wave when shared files interleave), with a message
   naming what was verified, not what was intended.
6. **Record ambiguity as an S-item, not as a silent choice.** `ACCEPTANCE.md` §14
   currently holds S1–S8.

### Up to 3 stages in parallel — and the boundary that matters

The user asked for 3-wide parallelism. It works, **but only across modules.** These
files are effectively **single-writer — the maintainer's**:

`src/cli.rs` · `src/commands.rs` · `src/lib.rs` · `src/config.rs` · `Cargo.toml` ·
`planning/*` · `scripts/*`

Agents may add **additively, minimally, never a whole-file rewrite** to `cli.rs` and
`commands.rs`. Every brief must say so, because a rewrite by one agent deletes
another's variant. And every brief must include: *"a transient compile error may not
be yours — check whether the failing file is one you own before reacting, and never
fix someone else's file."* Without that line, three agents start repairing each
other's work.

Also put `CARGO_TARGET_DIR=target/<stage>-agent` in every brief, or they contend on
the same build directory.

## 4. Traps that already cost time

- **Never pipe a gate.** `cargo clippy … | grep … ; git commit` always commits,
  because the pipe makes the exit status `grep`'s and `;` runs regardless. Use
  `scripts/qc.ps1`, which is fail-fast and exits non-zero.
- **Stage before gating.** `cargo fmt --all` rewrites files you did not ask about; if
  you stage a subset afterwards, the committed tree is red while your working tree is
  green (bug B7).
- **Re-run a live gate after any change to the payload it proves.** M3 changed
  agy's/codex's `build_args`, which invalidated M1/M2's live evidence. M0–M4 were
  re-validated for exactly this reason.
- **Manual `/usage` probes from git-bash need `MSYS_NO_PATHCONV=1`**, or MSYS rewrites
  `/usage` into `C:/Program Files/Git/usage`, the CLI receives it as *prompt text*, and
  you burn a ~20 700-token turn getting an answer about Git's disk space. Bug B10.
- **Security fixes over-correct.** Adding `/` to the redactor's charset made it eat
  file paths out of diagnostics (B23). Over-redaction looks like success, so the lens
  that finds under-redaction cannot find it — run the binary and read its output.
- **`scripts/qc.ps1` must stay pure ASCII.** Windows PowerShell 5.1 reads a `.ps1`
  without a BOM as Windows-1252, so one non-ASCII byte breaks parsing (B8).
- **A free probe reporting ~20 700 tokens spent is a harness bug, not CLI behaviour.**

## 5. Commands

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/qc.ps1        # 15 offline gates
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/qc.ps1 -Live  # + G5, spends quota
node scripts/check-docs.js                                               # markdown table shape
cargo test                                                               # 392
bash scripts/qc.sh                                                       # unix/WSL equivalent
```

Live gate pieces, run separately when a payload changed:
`PIE_LIVE=1 cargo test --test live_smoke -- --ignored --test-threads=1`

## 6. Next task — M6, three parallel stages

`DESIGN.md` §12 and `ACCEPTANCE.md` §9 hold the contract. Dependencies
(`ratatui 0.29`, `crossterm 0.29`) are committed and the G6 cross-target check is
clean. **The interfaces are frozen, so the three layers can be built in parallel:**

| Stage | Owns | Gate |
|---|---|---|
| A state + reducer | `src/tui/app.rs`, `tests/tui_app.rs` | `cargo test --test tui_app` |
| B render | `src/tui/render.rs`, `tests/tui_render.rs` | `cargo test --test tui_render` |
| C loop + wiring | `src/tui/mod.rs`, `src/commands/tui.rs`, `tests/tui_loop.rs` | `cargo test --test tui_loop` |

The maintainer wires `cli.rs`/`commands.rs` (`pie tui`, opt-in — a bare `pie` keeps
printing help so CI can never enter the TUI).

The one rule that makes M6 testable: **the reducer takes a neutral `Input` enum, never
a crossterm `KeyEvent`.** A `KeyEvent` parameter drags the terminal into every test.
`render` is asserted through ratatui's `TestBackend`; only the terminal loop is
untestable, so keep it thin.

Then G5: drive a **real** turn's `StreamEvent`s through `apply_event` and assert the
rendered buffer contains the answer — keystrokes cannot be automated, but "the TUI
renders a real vendor turn" is the substance of the Done condition.

## 7. Remaining milestones

| | Scope | Done condition |
|---|---|---|
| M6 | TUI (above) | interactive parity across agy + codex |
| M7 | `schedule.rs` + `learn.rs`: mine sessions → update profile/memory, provenance, decay, budgeted model use | profile reflects real habits; `/memory` shows what was learned |
| M8 | `watch/news.rs` + `watch/drift.rs` | `~/.pie/reports/watchtower-YYYY-MM.md`, drift diff |
| M9 | claude adapter + `tool_use`→`tool_result` loop | unverifiable live here — no Claude subscription |
| M10 | OpenRouter/DeepSeek from `DEFERRED-BACKENDS.md` | only backends where `cost_usd_micros` is real |

**Cross-cutting gates that become real at M7/M8:** background jobs must be budgeted
and opt-in, because an unbudgeted summarisation job can silently spend premium quota
(`RISKS.md` §13). That needs to be a *gate*, not prose.

## 8. Open items — decisions, not oversights

- **`wham/usage` — DECIDED 2026-10-12: off by default.** `pie usage codex` does
  not call the private vendor endpoint unless the user opts in with
  `codex.quota_probe = "http"` (`PIE_CODEX_QUOTA_PROBE`). The capability is kept,
  not deleted: the call is read-only using the user's own credential. The
  earlier `PLAN.md` §6 reading (default `http`) was reversed because "the plan
  asks for it" is not a reason for the *default* to carry the account risk
  `knowledge/9router-findings.md` §0 describes. See DESIGN §9, ACCEPTANCE §5,
  TOKEN-ECONOMICS §10d.
- **S1–S8** in `ACCEPTANCE.md` §14 — argv measured against the bin *name*; `println!`
  panics on broken pipe; `read_all` reads the whole log; version snapshot is a plain
  write; itsmini is env-only; `agent.compact_after_turns` named in the contract but
  not implemented; switch drops per-turn framing; `--switch`/`--model` use different
  error variants.
- **Coverage gap to keep stating**: a multi-line switched payload is not delivered
  end-to-end on Windows, because batch stand-ins reject multi-line argv. Payload-tested
  plus a cfg-gated half; Linux CI gets the stronger half.
- **Task-class routing** (PLAN §8's `pie route <task>`) — `default_backend` landed in
  M3, the router did not.
- **`pie -p` does not auto-`--continue`** — deliberate; resume must be asked for.

## 9. Verification habits worth keeping

- Prove a red tree *at the commit*: `git stash` and re-run the failing gate rather
  than trusting the working tree.
- When an observation contradicts a documented contract, **suspect the observer
  first.** Three corrupted probes nearly deleted a working free feature (B10).
- Prefer a test that would fail if the behaviour regressed over one that passes
  either way. `a_switched_turn_never_hands_the_old_vendor_id_to_the_new_backend` has
  a *resumed control* proving the flag **is** built when an id exists — without it, a
  `build_args` that never emitted `--conversation` would have passed.
- 23 bugs are logged in `ACCEPTANCE.md` §13 with symptom, how each was caught, the fix
  and the regression test. Read it before assuming something is unexplored.
