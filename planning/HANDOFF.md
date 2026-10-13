# Handoff — resuming `pie` development

> **Purpose.** A fresh session should be able to continue from here without
> re-deriving anything. Read this, then `planning/ROADMAP.md` for what to build and
> `planning/ACCEPTANCE.md` for how it is judged.

## 1. State at handoff

| | |
|---|---|
| Repo | `C:/Users/amiru/Local/Code/pie` |
| Branch | `feat/m0-m2-subscription-backends` |
| Last commit | M11 — pushed; `origin` in sync |
| Tests | see the run log (§12) — do not trust a number here, run `scripts/qc.ps1` |
| Gates | **38/39 offline green** — 6 standing checks (G1, G2, G3, G3b, G3c, G3d) + **31** G4 + G6. The 39th is G5, which spends real quota. M11's G5 is `n/a` and *stated*, which is why M11's verdict is `offline-verified` rather than `done` |
| Milestones | **M0–M11 built.** M9, M10 and M11 are `offline-verified` (no Claude credential / no API key / no live turn that could call a tool); every other milestone is `done`. Each has a run-log row in `ACCEPTANCE.md` §12 |
| Next | `ACCEPTANCE.md` §14's **S1–S22** — S2 is the cheapest remaining panic, S15 and S17 batch, and **S19 + S21 are one follow-up**. Then the release, which needs the user's version decision |

**Delegation works again; `advisor` does not.** The `402 Insufficient Balance` reported at the end of
the previous session has cleared — the `Agent` tool and the `agy-agent` skill both answer — but
`advisor` is still quota-blocked (resets ~72 h), so M11 was dispatched **and verified** without it.
Its three stages took two waves: **A and C in parallel** (disjoint files, so genuinely independent),
then **B**, which needs C's `process::ChildSession` — built before C, its `bash` would have become the
*third* copy of the spawn policy, which is the exact thing the milestone exists to delete.

Dispatching it was worth it for one reason worth repeating: **half of M11's defects were found by
breaking finished work, not by a stage's own gate passing.** A stage's report describes what it
looked for.

**Gate count changed four times across M6–M11, and every change was a defect being closed rather
than a feature being added.** G6 was a required gate with *no runner at all* and a command that
could not pass as written (B25); G3c (no `mod.rs`) and G3d (no test may depend on machine vendor
state, B32) are new; M11 added two more (its two stage binaries), and the milestone stages together
contributed **31** G4 rows. The current census is **38 offline gates green + G5**, and the only thing
that knows it is the runner. If you remember "15/16 gates", that number is four rewrites stale —
**run the runner rather than trusting any count in any document.**

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

- **A green suite whose greenness comes from the machine is not evidence, and it is
  invisible from that machine.** Four `parse_agy` tests passed only because the
  developer's real `~/.gemini` satisfied agy's subscription gate, so M1's own gate —
  and therefore every CI job — was red while the local run was green (B32). Gate G3d
  now runs the whole suite with `HOME` redirected to an empty directory. **If you add
  a test that touches vendor auth, give it a scratch home; do not inherit one.**
- **An outer `#[cfg(windows)]` on a function that also has inner `#[cfg(not(windows))]` arms
  deletes the function on unix while its callers still reference it.** M11's `kill_plan` did exactly
  that: every Windows gate was green (G1–G3, clippy, 852 tests) and `cargo check --target
  x86_64-unknown-linux-gnu` went **red on its first run**. The same shape recurs in every stand-in
  with a `.bat` half and a `.sh` half. **Put G6 in every stage brief**, and read "the unix arm is not
  compiled here" in a stage's report as a pointer to where to look rather than as a reassurance —
  stage C flagged the risk and the bug was sitting in the same report.
- **A claim this platform cannot observe needs a witness or a confession.** M11 needed both twice:
  removing `process_group(0)`/`CREATE_NO_WINDOW` reddens **nothing** here, because a console window
  is not observable from inside a console-attached test process; and "`bash` spawns no shell" is
  behavioural only on unix, because a `.bat` stand-in *is* a cmd script and re-parses its own
  command line. Both are now **source-text assertions with the gap stated** (S20, S22) rather than
  behavioural tests that do not exist.
- **A new test binary needs its own G4 row, added in the same commit as the binary.** `ACCEPTANCE`
  §19 names `cargo test --test tools` as stage A's *gate*, and a named gate with no runner is B25
  exactly. Adding the row before the binary exists makes a verified tree red; adding it later leaves
  the criterion resting on a claim.
- **`2>&1` on a native command's stderr aborts PowerShell** under
  `$ErrorActionPreference = 'Stop'`, via `NativeCommandError`. The M6 non-TTY gate
  failed *silently* because of it — no FAIL line, just exit 1 — which is the exact
  defect it was written to catch. Relax the preference around the invocation and
  report a named failure on every path. The bash runner has the twin trap: a failing
  command substitution in an assignment trips `set -e` before anything prints.
- **`EnvGuard` is not re-entrant and now says so.** A second `EnvGuard` on the same
  thread used to deadlock the whole test binary — no timeout, no output, just wall
  clock (B24). Scope guards in their own blocks.
- **A `const` table can be the single source of truth for a keymap.** `Input` is
  `Copy`, so `app::KEYBINDINGS` works as one; the help footer and the reducer read it,
  and the assertion is behavioural (each row produces its documented `Action` through
  `reduce`) rather than a string comparison.
- **Cargo.lock contains two `crossterm` versions** — `ratatui 0.29` depends on
  `crossterm 0.28.1` while pie depends on 0.29 directly. Recorded, not changed: the
  pair is the verified one, and the two never share a type.
- **Never pipe a gate.** `cargo clippy … | grep … ; git commit` always commits,
  because the pipe makes the exit status `grep`'s and `;` runs regardless. Use
  `scripts/qc.ps1`, which is fail-fast and exits non-zero.
- **Stage before gating.** `cargo fmt --all` rewrites files you did not ask about; if
  you stage a subset afterwards, the committed tree is red while your working tree is
  green (bug B7). `cargo test` compiles the **working tree**, never the index, so a
  commit is only verified if the working tree equals what you committed — which is why
  a wave whose shared files interleave becomes one commit rather than several.
- **When a frozen table is corrected after a stage has written against it, grep for
  the old row before declaring the milestone green.** Correcting the M6 keybinding
  table mid-flight left stage A's test helper pinning the *old* `Tab` row, so the
  corrected code failed a stale assertion (B26). Anything derived from a contract's
  earlier version is a derived artefact.
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

## 6. Where this stands

**Every milestone on `ROADMAP.md` is built.** Two of them carry a verdict other than `done`, and
the reason is a missing credential rather than unfinished work:

| | Verdict | Why |
|---|---|---|
| M0–M8 | `done` | every applicable gate, including live ones |
| M9 | `offline-verified` | no Claude credential, so no live turn is possible; the envelope is documented rather than captured |
| M10 | `offline-verified` | no API key configured, and a live turn spends real credit nobody opted into |

### The open work, in the order I would pick it up

1. **`ACCEPTANCE.md` §14's S1–S22.** S2 is the cheapest remaining panic: `println!`/`print!` panic on
   a write failure, so `pie -p … | head -1` exits 101. It is 78 sites across 15 files and the fix is
   one `out::` helper plus a **process-level** test — spawn the real binary, close the read end of
   its stdout *before* it writes, and assert exit 0 rather than 101. Closing early rather than
   overfilling the pipe is what makes that test cheap and deterministic. S15 and S17 batch well.
   **S19 and S21 are one follow-up**: both want the same change — `ToolSet` carrying resolved
   context (the ceiling, the composed scrub list) — so do them together rather than as the two
   separate patches each describes.
2. **The release**: promote `[Unreleased]`, choose the version, and tag **in the same commit**
   (`VERSIONING.md`). `Cargo.toml` is still `0.1.0` after eleven `feat:` milestones and `git tag` is
   still empty, so the first tag is a deliberate choice rather than an arithmetic one: `0.1.0`
   because nothing has ever been released is as defensible as a bump, and either way it is the
   user's call.
3. **Judge the TUI by hand** — `./target/release/pie.exe tui`. Still the user's, and still not
   automatable.
4. **Nothing else.** `DEFERRED-BACKENDS.md`'s two halves were closed by the M10 follow-up, and M11's
   residues are recorded as S18–S22 rather than left inside a stage report.

### Testing it hands-on

`cargo build --release` then `./target/release/pie.exe`. The TUI is `pie tui`; the interactive
verdict was left to you by design (`ACCEPTANCE.md` §9 — keystrokes are covered through the
reducer and rendering through the buffer, but "does this feel usable" is not automatable).

## 7. Remaining milestones

None. `ROADMAP.md`'s M0–M11 are all implemented; the run log in `ACCEPTANCE.md` §12 records each
one's verdict and the evidence behind it. New work belongs in `ROADMAP.md` as M12+, with its
acceptance criteria frozen in `ACCEPTANCE.md` **before** the code — the discipline that produced
every result below.

| | Scope | Done condition |
|---|---|---|
| M9 | claude adapter + `tool_use`→`tool_result` loop | **unverifiable live here** — no Claude credential. Verdict is `offline-verified`, never `done` |
| M10 | OpenRouter/DeepSeek from `DEFERRED-BACKENDS.md` | only where `cost_usd_micros` is real. Verdict depends on whether the user supplies keys |
| M11 | the tool suite: read-only tier, scoped `bash`, one spawn policy | **the tools are fully verified offline; the end-to-end turn is not**, because no backend with a working loop has a credential. Verdict is `offline-verified` |

## 8. Open items — decisions, not oversights

- **`wham/usage` — DECIDED 2026-10-12: off by default.** `pie usage codex` does
  not call the private vendor endpoint unless the user opts in with
  `codex.quota_probe = "http"` (`PIE_CODEX_QUOTA_PROBE`). The capability is kept,
  not deleted: the call is read-only using the user's own credential. The
  earlier `PLAN.md` §6 reading (default `http`) was reversed because "the plan
  asks for it" is not a reason for the *default* to carry the account risk
  `knowledge/9router-findings.md` §0 describes. See DESIGN §9, ACCEPTANCE §5,
  TOKEN-ECONOMICS §10d.
- **S1–S22** in `ACCEPTANCE.md` §14 — argv measured against the bin *name*; `println!`
  panics on broken pipe; `read_all` reads the whole log; version snapshot is a plain
  write; itsmini is env-only; `agent.compact_after_turns` named in the contract but
  not implemented; switch drops per-turn framing; `--switch`/`--model` use different
  error variants; `Done`'s vendor status string discarded on the TUI path; `scroll`
  has no ceiling in the reducer; three-stage checkout compile noise; the changelog
  and tagging rules are unenforced; the picker overlay has no scroll affordance;
  `require_subscription` precedes pure argv validation; `render_report`'s signature has
  no channel for failed news sources; `cargo fmt --all` rewrote an in-flight stage's
  file; the fused flag table is kept twice. **S18–S22 are M11's five**, and they are the
  ones to read before touching `src/tools/` or `src/process.rs`: the declared-but-unbuilt
  `write`/`edit` tier; `bash` takes no `cwd`; I1's evidence is source text; the tool child
  gets `BUILTIN_SCRUB` rather than the composed list; and "no shell" is behavioural only
  on unix. S19 and S21 are one follow-up, not two.
- **`crossterm` appears twice in `Cargo.lock`** — `ratatui 0.29` needs `0.28.1`, pie
  depends on `0.29`. Not a defect today (the two never share a type) and not changed,
  because the pair is what was verified. Deduplicating means pinning pie's direct dep
  to `0.28` and re-running G6 plus the M6 tests.
- **Not pushed.** `588967c` is local. The user's own workflow pushed each wave; confirm
  before pushing, since it is the one step here that is not locally reversible.
- **Coverage gap to keep stating**: a multi-line switched payload is not delivered
  end-to-end on Windows, because batch stand-ins reject multi-line argv. Payload-tested
  plus a cfg-gated half; Linux CI gets the stronger half.
- **Task-class routing** (PLAN §8's `pie route <task>`) — `default_backend` landed in
  M3, the router did not.
- **`pie -p` does not auto-`--continue`** — deliberate; resume must be asked for.

## 9. Verification habits worth keeping

**The single most valuable habit, learned the hard way in M6: for every claim, name the
thing that would fail if the claim were false. If the answer is "nothing, but it is
written down", the claim is not evidence.** M6 found six instances of that shape — a
required gate with no runner, an unenforced convention, an unused dependency feature
documented as required, a frozen criterion never asserted on the wire, a changelog rule
with no signal, and a contract table nobody walked through. None was found by a stage's
own gate passing; every one was found by attacking an artefact that already looked green.

- **Attack the thing that already passed.** Stage B found one panic in `render` and
  passed its own tests; a grid sweep with adversarial payloads found a second one in the
  same function (B29). "Tested" is not "tested by someone trying to break it".
- **Run the suite where the developer's machine does not exist.** Redirect `HOME` to an
  empty directory (gate G3d). That is what found B32, and it is the only way to see a
  green suite that is green because of the machine.
- **Prove a red tree *at the commit*:** `git stash` and re-run the failing gate rather
  than trusting the working tree. `cargo test` compiles the working tree, not the index.
  This is also why a wave is one commit when shared files interleave.
- **When an observation contradicts a documented contract, suspect the observer first.**
  Corrupted probes nearly deleted a working free feature (B10).
- **Prefer a test that would fail if the behaviour regressed** over one that passes
  either way. `a_switched_turn_never_hands_the_old_vendor_id_to_the_new_backend` has a
  *resumed control* proving the flag **is** built when an id exists.
- **Ask each stage to break its own work and say which test caught it.** Every M6 brief
  required it, and each stage reported specific named tests going red. A gate nobody has
  seen fail is a gate nobody knows works.
- **Read each stage's report, then test its claims independently.** An agent's summary
  describes intent, not outcome. Three of M6's six defects came from something a stage
  mentioned *in passing* — including the parenthetical that turned out to be B32.
- 30 bugs are logged in `ACCEPTANCE.md` §13 with symptom, how each was caught, the fix
  and the regression test. Read it before assuming something is unexplored.
