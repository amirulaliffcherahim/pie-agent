#!/usr/bin/env bash
#
# scripts/qc.sh - pie quality-control gate runner for Linux / WSL / macOS.
#
# The Windows equivalent is scripts/qc.ps1. Both run the SAME gates in the SAME
# order so the two platforms cannot drift apart.
#
# Gates (planning/ACCEPTANCE.md section 1):
#
#   G1  cargo fmt --check                             no diff
#   G2  cargo clippy --all-targets -- -D warnings      zero warnings
#   G3  cargo test                                     all pass
#   G3b every #[ignore] carries a reason
#   G3c module layout: no mod.rs files
#   G3d no vendor state: the whole suite with HOME redirected to an empty dir
#   G4  offline acceptance - M0: cargo run -- --list-backends
#       ... one per milestone, and one per M5/M6 stage
#   G4  `pie tui` on a pipe refuses with a message and no escape codes
#   G6  cargo check --target x86_64-unknown-linux-gnu --all-targets
#       --no-default-features  (cross-target; SKIPped loudly if the target is
#       not installed). `--no-default-features` is required, not optional: the
#       default `http` feature needs a C compiler for the target.
#   G5  live acceptance                                ONLY with --live
#
# G4 is the milestone's offline acceptance command, one gate per milestone:
# M0 `cargo run -- --list-backends`, M1 `cargo test --test parse_agy`,
# M2 `cargo test --test parse_codex`, M3 `cargo test --test profile` +
# `cargo run -- profile`, M4 `cargo run -- auth` + `cargo run -- usage`,
# M5 sessions `cargo test --test sessions`. CI runs the same commands, through this
# same script (.github/workflows/ci.yml), so local and CI cannot drift apart.
# Add the next milestone's command when it lands. `tests/acceptance.rs` asserts
# what the M0 table actually prints, because this gate only sees the exit status.
#
# FAIL-FAST: the first failing gate prints `FAIL: <gate>` on stderr and exits
# immediately with that gate's exit status. Later gates do not run - a red G2
# means the G3 result would be meaningless.
#
# ---------------------------------------------------------------------------
# --live: THIS SPENDS REAL SUBSCRIPTION QUOTA.
# ---------------------------------------------------------------------------
# `--live` additionally runs the `#[ignore]`d smoke tests in
# `tests/live_smoke.rs` against the real vendor CLIs. It:
#   * spends real subscription quota on the developer's own paid accounts,
#   * requires `PIE_LIVE=1` (this script exports it for the child only),
#   * may only use the approved models `agy/gemini-3.8-flash` and
#     `codex/gpt-6-luna` - do not substitute a pricier model,
#   * must NEVER be run in CI, and
#   * is skippable only by explicit user decision, in which case the milestone
#     is recorded as `offline-verified` rather than `done`.
#
# Use it deliberately, not as part of the edit/run loop. Run without `--live`
# after every code change.
#
# ---------------------------------------------------------------------------
# Executable bit: if a Windows checkout or file copy drops it, run
# `bash scripts/qc.sh ...` or restore it with
# `git update-index --chmod=+x scripts/qc.sh`.

set -euo pipefail

# Run from the repository root regardless of where the script is invoked from.
REPO_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd -- "$REPO_ROOT"

LIVE=0
GATES_RUN=0
GATES_SKIPPED=0

usage() {
  cat >&2 <<'USAGE'
usage: scripts/qc.sh [--live] [--help]

  (no flags)  run G1 fmt, G2 clippy, G3 test, G4 M0 acceptance; stop at the
              first failure; print a PASS line per gate and a final verdict
  --live      additionally run the live smoke tests. SPENDS REAL SUBSCRIPTION
              QUOTA and requires network + logged-in vendor CLIs. Never in CI.
  --help      print this message
USAGE
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --live)  LIVE=1 ;;
    -h|--help) usage; exit 0 ;;
    *)
      printf 'qc.sh: unknown argument `%s`\n' "$1" >&2
      usage
      exit 2   # 2 = usage/config error, per DESIGN.md section 5
      ;;
  esac
  shift
done

# Running this under git-bash/MSYS on Windows works, but it is not the
# supported path: Windows has scripts/qc.ps1, and this script exists to prove
# the unix build. Say so rather than silently giving a weaker signal.
case "$(uname -s 2>/dev/null || printf unknown)" in
  MINGW*|MSYS*|CYGWIN*)
    printf 'qc.sh: warning: running under %s - use scripts/qc.ps1 for the real Windows gate\n' \
      "$(uname -s)" >&2
    ;;
esac

if ! command -v cargo >/dev/null 2>&1; then
  printf 'FAIL: preflight (cargo not found on PATH - install the Rust toolchain)\n' >&2
  exit 1
fi

# run_gate <label> <command> [args...]
#
# `cmd || { ... }` keeps `set -e` from exiting before the FAIL line is printed.
run_gate() {
  local label="$1"
  shift
  printf '\n==> %s: %s\n' "$label" "$*"
  "$@" || {
    local status=$?
    printf '\nFAIL: %s\n     command: %s\n     exit: %d\n' "$label" "$*" "$status" >&2
    exit "$status"
  }
  GATES_RUN=$((GATES_RUN + 1))
  printf 'PASS: %s\n' "$label"
}

printf 'pie QC gates - %s\n' "$REPO_ROOT"
printf 'live smoke: %s\n' "$([ "$LIVE" -eq 1 ] && printf 'ENABLED (real quota will be spent)' || printf 'disabled (pass --live to enable)')"

# --- G1 ---------------------------------------------------------------------
run_gate 'G1 format'  cargo fmt --check

# --- G2 ---------------------------------------------------------------------
run_gate 'G2 lint'    cargo clippy --all-targets -- -D warnings

# --- G3 ---------------------------------------------------------------------
# Live smoke tests are `#[ignore]`d and gated on PIE_LIVE=1, so a plain
# `cargo test` never touches the network or spends quota.
run_gate 'G3 tests'   cargo test

# --- G3b (every `#[ignore]` carries a reason) --------------------------------
# ACCEPTANCE section 1 makes "0 ignored-without-reason" part of G3's pass
# condition, but `cargo test` reports an unreasoned ignore as merely ignored, so
# nothing enforced it. A bare `#[ignore]` is a test that silently stopped running.
check_ignore_reasons() {
  local offenders
  offenders="$(grep -rn --include=*.rs -E '^[[:space:]]*#\[ignore\][[:space:]]*$' src tests || true)"
  if [ -n "$offenders" ]; then
    printf '%s\n' "$offenders" >&2
    return 1
  fi
  return 0
}
run_gate 'G3b ignore reasons' check_ignore_reasons

# --- G3c (module layout: no `mod.rs`) ---------------------------------------
# ACCEPTANCE section 3 requires "no `mod.rs` files; `provider.rs` sits beside
# `provider/`" - one idiom for a parent module and its directory (DESIGN section 2).
#
# Nothing enforced it: no test, no script, no CI job. M6's own plan then named
# `src/tui/mod.rs`, the scaffold created exactly that file, and the whole suite
# stayed green - the same family as B20 (a rule with nothing able to contradict
# it) and the same shape as the G6 gap found in the same session. A convention
# with no check is a convention that lasts until the next agent does not read the
# section.
#
# Cheap to check and impossible to argue with: if the directory holds a `mod.rs`,
# the parent module file is in the wrong place.
check_no_mod_rs() {
  local offenders
  offenders="$(find src -name mod.rs -print || true)"
  if [ -n "$offenders" ]; then
    printf 'no mod.rs files - the parent module file belongs BESIDE its directory (e.g. provider.rs + provider/)\n' >&2
    printf '%s\n' "$offenders" >&2
    return 1
  fi
  return 0
}
run_gate 'G3c module layout' check_no_mod_rs

# --- G3d (no test may depend on machine vendor state) -----------------------
# The whole suite is run again with `HOME` and `USERPROFILE` redirected to an empty
# directory, because **that is what a CI runner is**.
#
# This exists because four tests in `tests/parse_agy.rs` were passing only on a
# machine with agy logged in. `Agy::stream` refuses without a subscription
# credential, their helper pointed `PIE_HOME` at a scratch directory but left `HOME`
# alone, and the developer's real `~/.gemini` satisfied the gate. On every CI runner
# it did not, so `G4 acceptance (M1)` - M1's own named gate - was red on ubuntu,
# windows and macos while the local gate run was green and the docs claimed CI was
# the real Linux evidence. Nothing could have caught it locally.
#
# A green suite whose greenness comes from the machine is not evidence, and it is
# invisible from the machine. This gate is the general form of that observation.
#
# `RUSTUP_HOME`/`CARGO_HOME` are carried over explicitly only because rustup cannot
# find a toolchain once `HOME` moves; they are toolchain plumbing, not vendor state.
check_no_vendor_state() {
  local real_home empty status
  real_home="$HOME"
  empty="$(mktemp -d)"
  printf '\n==> G3d no vendor state: cargo test (HOME redirected to an empty directory)\n'
  # `set +e` around the call: under `set -e` a failing cargo test would abort the
  # script here instead of returning, and the FAIL line below would never print.
  set +e
  HOME="$empty" USERPROFILE="$empty" \
  RUSTUP_HOME="${RUSTUP_HOME:-$real_home/.rustup}" \
  CARGO_HOME="${CARGO_HOME:-$real_home/.cargo}" \
    cargo test
  status=$?
  set -e
  rm -rf "$empty"
  if [ "$status" -ne 0 ]; then
    printf 'a test passed only because this machine has a vendor CLI logged in, or other state\n' >&2
    printf 'under HOME. Every test must pass where none of that exists, because that is what\n' >&2
    printf 'CI is. Have the test establish the state it asserts about.\n' >&2
  fi
  return "$status"
}
run_gate 'G3d no vendor state' check_no_vendor_state

# --- G4 (offline acceptance, one gate per milestone) ------------------------
# The per-milestone acceptance commands from planning/ACCEPTANCE.md sections 3-8.
# G3 already runs every test binary, so these are partly redundant on purpose:
# the point is that a milestone's own acceptance command is named and fails
# individually, so a red gate says *which* milestone regressed rather than just
# "tests failed". Add the next milestone's command here as it lands.
run_gate 'G4 acceptance (M0)' cargo run -- --list-backends
run_gate 'G4 acceptance (M1)' cargo test --test parse_agy
run_gate 'G4 acceptance (M2)' cargo test --test parse_codex
run_gate 'G4 acceptance (M3 tests)' cargo test --test profile
run_gate 'G4 acceptance (M3 profile)' cargo run -- profile
run_gate 'G4 acceptance (M4 auth)' cargo run -- auth
run_gate 'G4 acceptance (M4 usage)' cargo run -- usage
# M5's stages share one test binary (`cargo test --test sessions`), so its gate is
# named for the store rather than for the stage that happened to land first: a
# label naming only stage A would lie once B (continue), C (compact) and F
# (switch) are covered by the same command.
run_gate 'G4 acceptance (M5 sessions)' cargo test --test sessions
run_gate 'G4 acceptance (M5 compact)' cargo test --test compact
run_gate 'G4 acceptance (M5 ocr)' cargo test --test ocr
# Stage E's server has its own binary, so its gate is its own command: the stdio
# handshake, driven through the built binary rather than through the library.
run_gate 'G4 acceptance (M5 mcp)' cargo test --test mcp

# M6's stages each have their OWN test binary, so a red gate names the stage that
# broke rather than just "the TUI failed" (ACCEPTANCE sections 8 and 9: M5 shipped
# two gates that could not fail their own stage). Stage D gets its own binary even
# though it refines stage A's subject matter, for exactly that attribution reason.
run_gate 'G4 acceptance (M6 tui_app)' cargo test --test tui_app
run_gate 'G4 acceptance (M6 tui_render)' cargo test --test tui_render
run_gate 'G4 acceptance (M6 tui_loop)' cargo test --test tui_loop
run_gate 'G4 acceptance (M6 tui_keys)' cargo test --test tui_keys

# M7's stages each have their own binary, for the same attribution reason as M6's.
run_gate 'G4 acceptance (M7 learn)' cargo test --test learn
run_gate 'G4 acceptance (M7 consolidate)' cargo test --test consolidate
run_gate 'G4 acceptance (M7 schedule)' cargo test --test schedule
run_gate 'G4 acceptance (M7 commands)' cargo test --test memory_cmd

# M8's stages, one binary each, same reason.
run_gate 'G4 acceptance (M8 drift)' cargo test --test drift
run_gate 'G4 acceptance (M8 news)' cargo test --test news
run_gate 'G4 acceptance (M8 report)' cargo test --test watchtower
run_gate 'G4 acceptance (M8 command)' cargo test --test watch_cmd

# M9's stages. Its G5 is `n/a` rather than skipped: there is no Claude credential on this
# machine, so no live turn is possible - recorded rather than silently absent.
run_gate 'G4 acceptance (M9 claude stream)' cargo test --test parse_claude
run_gate 'G4 acceptance (M9 tool loop)' cargo test --test tool_loop

# M10's two layers. Its G5 is `n/a - needs the user's key`: a live turn spends real credit
# in a currency nobody opted into, so it is stated rather than skipped.
run_gate 'G4 acceptance (M10 sse)' cargo test --test sse
run_gate 'G4 acceptance (M10 http transport)' cargo test --test http_transport

# --- G4 (M6 tui non-TTY) ----------------------------------------------------
# ACCEPTANCE section 9: `pie tui` on a non-TTY stdin must fail with a clear message
# rather than emitting escape codes into a pipe.
#
# A property of the BUILT BINARY, so it cannot be a cargo test, and `'' |` makes
# stdin a pipe deterministically rather than depending on how the runner was
# invoked. Both halves are asserted: the exit code alone would pass for a run that
# printed escapes and then failed, and the no-escapes half alone would pass for one
# that failed for an unrelated reason.
run_gate 'G4 acceptance (M6 tui build)' cargo build
# Resolve the binary once: on unix it is `pie`, and under git-bash/MSYS (where this
# script runs only as a courtesy) it is `pie.exe`. Hardcoding either name makes the
# check fail for the *wrong* reason on the other platform, which is worse than not
# running it.
PIE_BIN='./target/debug/pie'
if [ -f "$PIE_BIN.exe" ]; then
  PIE_BIN="$PIE_BIN.exe"
fi
check_tui_refusal() {
  local out status
  # `|| status=$?` rather than a bare assignment: under `set -e` a failing command
  # substitution in an assignment aborts the script, so this refusal - a NON-ZERO
  # exit is the pass condition - would have killed the runner before it printed
  # anything. Same trap as the PowerShell runner's NativeCommandError.
  status=0
  out="$(printf '' | "$PIE_BIN" tui 2>&1)" || status=$?
  if [ "$status" -eq 0 ]; then
    printf 'pie tui must NOT succeed with stdin on a pipe\n' >&2
    return 1
  fi
  if printf '%s' "$out" | grep -q $'\033\['; then
    printf 'escape codes reached the pipe before the refusal: %s\n' "$out" >&2
    return 1
  fi
  case "$out" in
    *terminal*) return 0 ;;
    *) printf 'the refusal must name the reason; got: %s\n' "$out" >&2; return 1 ;;
  esac
}
run_gate 'G4 acceptance (M6 tui non-TTY)' check_tui_refusal

# --- G6 (cross-target check) ------------------------------------------------
# ACCEPTANCE section 1. Linux cannot be *run* locally (WSL2 is installed but
# cannot boot - PLATFORMS.md F3), so this catches cfg-gated compile errors
# without linking. CI's ubuntu job is the real Linux evidence; this is the local
# pre-flight that catches those errors before a CI round trip.
#
# TWO things this has to get right, and until M6 it got both wrong:
#
#   1. It had NO RUNNER AT ALL. ACCEPTANCE section 1 lists G6 as a required gate
#      and every run-log row reports it green, but neither this script nor
#      scripts/qc.ps1 ever ran it - it was a hand-run command and a claim, which
#      is B20's shape ("a gate ran but could not fail") in its other direction:
#      a gate that could not run. The run-log G6 column rested on nothing that
#      could have contradicted it.
#
#   2. The documented command COULD NOT PASS AS WRITTEN.
#      `cargo check --target x86_64-unknown-linux-gnu --all-targets` fails with
#      "failed to find tool x86_64-linux-gnu-gcc", because the default `http`
#      feature pulls reqwest -> rustls -> ring, which needs a C compiler for the
#      target. The flag that makes it work is `--no-default-features`. Same class
#      as B12: a criterion derived from a document rather than from running the
#      command it names.
#
# The cost of that second point is real and stated rather than hidden:
# `--no-default-features` means this check does NOT cover cfg(feature = "http")
# code on Linux. The CI ubuntu job covers it natively, and is stronger evidence.
#
# A missing target is reported as SKIP, loudly and counted separately, never as a
# pass: a silent skip is exactly how B20 happened.
G6_TARGET='x86_64-unknown-linux-gnu'
G6_INSTALLED=0
if command -v rustup >/dev/null 2>&1; then
  if rustup target list --installed 2>/dev/null | grep -qx -- "$G6_TARGET"; then
    G6_INSTALLED=1
  fi
fi
if [ "$G6_INSTALLED" -eq 1 ]; then
  run_gate 'G6 cross-target (linux, no-default-features)' \
    cargo check --target "$G6_TARGET" --all-targets --no-default-features
else
  printf '\nSKIP: G6 cross-target - %s is not installed\n      rustup target add %s\n' \
    "$G6_TARGET" "$G6_TARGET" >&2
  GATES_SKIPPED=$((GATES_SKIPPED + 1))
fi

# --- G5 (live acceptance) ---------------------------------------------------
if [ "$LIVE" -eq 1 ]; then
  printf '\n!! --live: the next gate spends REAL subscription quota on the\n' >&2
  printf '!! approved models only (agy/gemini-3.8-flash, codex/gpt-6-luna).\n' >&2
  # Exported for the child only; the rest of the script stays quota-free.
  # `--test-threads=1` keeps real CLI turns serialised so quota accounting and
  # any rate limiting stay legible.
  PIE_LIVE=1 run_gate 'G5 live smoke' cargo test --test live_smoke -- --ignored --test-threads=1
fi

if [ "$GATES_SKIPPED" -gt 0 ]; then
  SKIP_SUFFIX=", $GATES_SKIPPED SKIPPED"
else
  SKIP_SUFFIX=''
fi
printf '\n==> verdict: PASS - %d/%d gates green%s%s\n' \
  "$GATES_RUN" "$((GATES_RUN + (LIVE == 1 ? 0 : 1)))" "$SKIP_SUFFIX" \
  "$([ "$LIVE" -eq 1 ] && printf '' || printf ' (G5 live smoke not run - milestone would be `offline-verified`)')"
exit 0
