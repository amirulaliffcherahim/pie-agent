#!/usr/bin/env bash
#
# scripts/qc.sh — pie quality-control gate runner for Linux / WSL / macOS.
#
# The Windows equivalent is scripts/qc.ps1. Both run the SAME gates in the SAME
# order so the two platforms cannot drift apart.
#
# Gates (planning/ACCEPTANCE.md §1):
#
#   G1  cargo fmt --check                             no diff
#   G2  cargo clippy --all-targets -- -D warnings      zero warnings
#   G3  cargo test                                     all pass
#   G4  offline acceptance — M0: cargo run -- --list-backends
#   G5  live acceptance                                ONLY with --live
#
# G4 is the milestone's offline acceptance command. Only the M0 command is
# wired in here; as M1/M2 land, their acceptance commands (`cargo test --test
# parse_agy`, `cargo test --test parse_codex`) are added next to it. CI runs the
# same command (.github/workflows/ci.yml) so local and CI agree.
#
# FAIL-FAST: the first failing gate prints `FAIL: <gate>` on stderr and exits
# immediately with that gate's exit status. Later gates do not run — a red G2
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
#     `codex/gpt-6-luna` — do not substitute a pricier model,
#   * must NEVER be run in CI, and
#   * is skippable only by explicit user decision, in which case the milestone
#     is recorded as `offline-verified` rather than `done`.
#
# Use it deliberately, not as part of the edit/run loop. Run without `--live`
# after every code change.
#
# ---------------------------------------------------------------------------
# Executable bit: if a Windows checkout or file copy drops it, run
# `bash scripts/qc.sh …` or restore it with
# `git update-index --chmod=+x scripts/qc.sh`.

set -euo pipefail

# Run from the repository root regardless of where the script is invoked from.
REPO_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd -- "$REPO_ROOT"

LIVE=0
GATES_RUN=0

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
      exit 2   # 2 = usage/config error, per DESIGN.md §5
      ;;
  esac
  shift
done

# Running this under git-bash/MSYS on Windows works, but it is not the
# supported path: Windows has scripts/qc.ps1, and this script exists to prove
# the unix build. Say so rather than silently giving a weaker signal.
case "$(uname -s 2>/dev/null || printf unknown)" in
  MINGW*|MSYS*|CYGWIN*)
    printf 'qc.sh: warning: running under %s — use scripts/qc.ps1 for the real Windows gate\n' \
      "$(uname -s)" >&2
    ;;
esac

if ! command -v cargo >/dev/null 2>&1; then
  printf 'FAIL: preflight (cargo not found on PATH — install the Rust toolchain)\n' >&2
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

printf 'pie QC gates — %s\n' "$REPO_ROOT"
printf 'live smoke: %s\n' "$([ "$LIVE" -eq 1 ] && printf 'ENABLED (real quota will be spent)' || printf 'disabled (pass --live to enable)')"

# --- G1 ---------------------------------------------------------------------
run_gate 'G1 format'  cargo fmt --check

# --- G2 ---------------------------------------------------------------------
run_gate 'G2 lint'    cargo clippy --all-targets -- -D warnings

# --- G3 ---------------------------------------------------------------------
# Live smoke tests are `#[ignore]`d and gated on PIE_LIVE=1, so a plain
# `cargo test` never touches the network or spends quota.
run_gate 'G3 tests'   cargo test

# --- G4 (M0 offline acceptance) ---------------------------------------------
run_gate 'G4 acceptance (M0)' cargo run -- --list-backends

# --- G5 (live acceptance) ---------------------------------------------------
if [ "$LIVE" -eq 1 ]; then
  printf '\n!! --live: the next gate spends REAL subscription quota on the\n' >&2
  printf '!! approved models only (agy/gemini-3.8-flash, codex/gpt-6-luna).\n' >&2
  # Exported for the child only; the rest of the script stays quota-free.
  # `--test-threads=1` keeps real CLI turns serialised so quota accounting and
  # any rate limiting stay legible.
  PIE_LIVE=1 run_gate 'G5 live smoke' cargo test --test live_smoke -- --ignored --test-threads=1
fi

printf '\n==> verdict: PASS — %d/%d gates green%s\n' \
  "$GATES_RUN" "$((GATES_RUN + (LIVE == 1 ? 0 : 1)))" \
  "$([ "$LIVE" -eq 1 ] && printf '' || printf ' (G5 live smoke not run — milestone would be `offline-verified`)')"
exit 0
