# Changelog

All notable changes to `pie` are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versioning follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html) as constrained by
`planning/VERSIONING.md`: 0.0.0 at scaffold, `fix:` → PATCH, `feat:` → MINOR, and
never 1.0.0 without explicit consent.

## [Unreleased]

M1-M6 plus the defects found while closing M6. **Not yet promoted or tagged** — see
`planning/VERSIONING.md` and ACCEPTANCE §14's S12: promoting this block and tagging are
one release action, and the version in `Cargo.toml` has not moved since M0's `0.1.0`
despite six `feat:` milestones, so the bump is a deliberate decision rather than an
automatic one.

### Added

- **M1/M2** — the `agy` and `codex` delegate-mode backends: stream normalisation for
  both vendor formats, model catalogs cached with a 24h TTL, quota reporting, and a
  subscription gate that refuses an API-key-only or absent credential.
- **M3** — one profile projected into every backend: `profile.toml`, `AGENTS.md` and
  `memory/*.md`, assembled deterministically so agy and codex receive byte-identical
  text, plus `pie profile` to show the projection and where each part came from.
- **M4** — `pie auth` and `pie usage`: unified tables that work with no model selected
  and spend no model quota.
- **M5** — sessions and tools: a turn is persisted and resumable (`--continue`,
  `--session`, `--switch <backend>`), compaction is extractive and spawns nothing,
  and `pie ocr` is available both as a command and as an MCP server for the
  delegate CLIs.
- **M6** — an interactive ratatui TUI (`pie tui`): transcript and streaming output,
  a model picker, a keymap help footer, and per-turn recording identical to `pie -p`.
  Opt-in, so a bare `pie` still prints help and no existing script or CI job can
  enter it. The reducer, the renderer and the terminal loop are separate, so all but
  the loop itself is tested without a terminal.
- New gate checks: **G6** (cross-target) is now actually wired into both runners,
  **G3c** asserts the `no mod.rs` module layout, and **G3d** re-runs the whole suite
  with `HOME` redirected to an empty directory so no test can pass only because the
  developer's machine has vendor state.

### Changed

- **`codex.quota_probe` now defaults to `off`.** `pie usage codex` no longer calls
  the private `chatgpt.com/backend-api/wham/usage` endpoint unless it is opted in
  with `quota_probe = "http"` or `PIE_CODEX_QUOTA_PROBE=http`. The capability is
  kept because the call is read-only using the user's own credential; the default is
  off because an account risk nobody opted into is not a reasonable thing for a
  default to spend. Session-log totals are unaffected.

### Fixed

- **`pie -p --json` now reports what a turn cost.** `StreamEvent::Usage` was never
  emitted by any backend, so the frozen criterion "`turn.completed` ⇒ `Usage` +
  `Done`" was unmet and a `--json` consumer could not see token accounting at all.
  The event is emitted from the finalised outcome, after `Done`, and suppressed when
  usage is all-zero (zero means "not reported", not "nothing was spent").
- **Four `agy` tests no longer depend on the machine being logged in**, which had made
  M1's own gate — and therefore every CI job — fail while the local run was green.
- **`EnvGuard` panics with the fix instead of deadlocking** when a test takes it
  twice on one thread; the deadlock hung the whole suite with no timeout to break it.
- TUI: a second turn can be submitted after the first completes (previously `Enter`
  was inert forever once a turn had finished); `Esc` no longer erases a failure
  reason; a repeated `Tab` no longer discards the model catalog; and a large scroll
  offset no longer panics inside ratatui.
- The non-TTY refusal message for `pie tui` reads correctly when both streams are
  redirected.

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
