# Changelog

All notable changes to `pie` are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versioning follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html) as constrained by
`planning/VERSIONING.md`: 0.0.0 at scaffold, `fix:` → PATCH, `feat:` → MINOR, and
never 1.0.0 without explicit consent.

## [Unreleased]

## [0.1.0] - 2026-10-12

M0 — scaffold, provider contract, and the three CLI backends as stubs with
verified argv contracts (`planning/ROADMAP.md`).

### Added

- `Provider` trait with `Capabilities { tool_loop, resume, stdin_prompt,
  max_prompt_argv }`, a registry, and `provider/model` resolution. Every CLI
  backend reports `tool_loop: false` — `agy --print` and `codex exec` are
  one-shot delegate backends and cannot accept an injected tool result.
- `provider/cli.rs`: a single turn driver (`EventRouter` + `drive`) shared by all
  backends, plus argv-cap enforcement.
- `proc.rs`: hidden child spawn (`CREATE_NO_WINDOW` on Windows, own process group
  on unix), concurrent bounded stderr capture, whole-turn deadline with
  process-tree kill, secret redaction, and `PATH` + `PATHEXT` binary resolution.
- `config.rs`: `~/.pie/config.toml` with CLI flag → `PIE_*` env → file → default
  precedence, and the API-key scrub deny-list that keeps a subscription CLI from
  silently switching to pay-as-you-go billing.
- `jsonl.rs`: tolerant line parsing, so a banner or an unknown event never aborts
  an otherwise healthy run.
- `auth.rs`: credential classification (`subscription` vs `api-key-only` vs
  absent) per backend. The legacy `~/.pie/auth.json`, which holds extracted OAuth
  tokens, is deliberately never read or written.
- `sessionlog.rs`: append-only `~/.pie/usage.jsonl` using the frozen row schema
  shared with the earlier JS implementation.
- `usage.rs`: quota-report rendering and session-log totals.
- CLI: `--list-backends`, `models`, `auth`, `usage`, `doctor`, and
  `-p/--print` delegate mode with `--json` event output.
- `scripts/qc.sh` and `scripts/qc.ps1` gate runners, a GitHub Actions matrix, and
  `planning/PLATFORMS.md` documenting the Windows / WSL / Linux contract.
- `planning/DESIGN.md` (implementation contract) and `planning/ACCEPTANCE.md`
  (frozen acceptance and QC gates, plus the bug log).

### Fixed

Bugs found by the M0 gates — each now has a regression test
(`planning/ACCEPTANCE.md` §9):

- `proc::redact` failed to redact secrets shaped `token=<60+ char blob>`, because
  the `=` broke the token split. Rewritten as a two-pass redactor.
- `StreamEvent` used serde internal tagging, which cannot represent a newtype
  variant holding a string — `pie -p --json` would have failed at runtime.
  Switched to adjacent tagging with pinned tags.
- codex's version probe reported **node's** version instead of codex's, because
  it ignored the `node <codex.js>` launcher prefix.
- `#[async_trait]` was missing on all three `impl Provider` blocks; the crate did
  not compile.
- Env-mutating tests raced on `HOME`/`PIE_HOME`; added a shared `EnvGuard`.

### Changed

- `knowledge/agy.md`: agy observed at **1.3.3** (was 1.3.2 when first
  reverse-engineered). Flag and stream schema notes unchanged.
