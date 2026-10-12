# pie

One profile over the subscription-locked coding-agent CLIs — **agy** (Google
Antigravity), **codex** (OpenAI Codex) and **claude** (Claude Code) — driven
headlessly from a single Rust binary.

`pie` is a Rust reimplementation of the pi coding-agent loop, rebuilt around one
goal: combine three proprietary subscription plans behind one persona, one
config and one memory, instead of keeping three separate CLIs, three sets of
credentials and three habits.

## Safety note — read this before using it

`pie` works by **driving the official CLIs** (`agy --print`, `codex exec --json`,
`claude -p`) using each vendor's own login and quota. That is the whole design.

It deliberately does **not**:

- extract, store, or replay OAuth tokens,
- call any vendor's private API directly,
- accept an API key for a subscription backend,
- upload or retain transcripts.

For that reason it never reads `~/.pie/auth.json` — the extracted-token file left
behind by an earlier JavaScript implementation. That file, if present, is left
byte-identical. See `planning/PLAN.md` §5 and `planning/RISKS.md` §10.

This is a personal/experimental tool. Stay within your own subscriptions.

Three invariants are enforced by tests, not by good intentions
(`planning/ACCEPTANCE.md` §2, `tests/no_secrets.rs`):

- **I2** the API-key environment variables are stripped from every child process,
  so a CLI cannot silently switch from a subscription to pay-as-you-go billing;
- **I10** no credential is hardcoded, printed, or written to disk — a static scan
  of the tree plus a behavioural run of the binary with keys in the environment;
- **I11** the legacy `auth.json` is never touched.

## Status

| Milestone | Scope | State |
|---|---|---|
| **M0** | scaffold, `Provider` contract, process layer, argv contracts for all three CLIs | **done** — see `planning/ACCEPTANCE.md` §8 |
| **M1** | agy backend: stream parsing, model catalog, quota probe | in progress |
| **M2** | codex backend: stream parsing, model catalog, quota probe | in progress |
| M3–M10 | profile/memory, unified auth+usage, agent loop, TUI, learn, watchtower, claude, HTTP backends | not started |

Until M1 and M2 land, `pie --list-backends`, `pie auth`, `pie usage` and
`pie doctor` work and spend no quota; `pie -p …` reports that the backend's turn
path is not implemented yet rather than pretending otherwise.

## Build and run

```sh
cargo build --release
```

```sh
# What is installed, and is it logged in?
pie --list-backends
pie auth
pie doctor          # adds versions, local paths, and the env scrub in effect

# Models and quota (neither spends model tokens)
pie models
pie usage agy

# One delegate turn
pie -p --model agy/gemini-3.8-flash "summarise this repo"
pie -p --model codex/gpt-6-luna --json "list the TODOs"
```

`-p/--print` hands the whole task to the vendor CLI, which runs its own tools and
returns a final answer. That is **delegate mode**, and it is the only mode these
CLIs support: `agy --print` and `codex exec` are one-shot and cannot accept an
injected tool result (`planning/RISKS.md` §1). Human answers go to stdout so they
can be piped; tool progress goes to stderr; `--json` emits one normalised event
per line.

## Configuration

Optional — `~/.pie/config.toml`, or `$PIE_HOME/config.toml`. Precedence is
CLI flag → `PIE_*` environment → file → built-in default.

```toml
[general]
default_model   = "agy/gemini-3.8-flash"
timeout_secs    = 600
prompt_argv_cap = 20000

[agy]
bin            = "agy"
default_effort = "medium"   # agy rejects --model without --effort

[codex]
bin           = "codex"
use_node_shim = true        # never spawn codex.cmd; use node + codex.js
quota_probe   = "off"       # http | off — off is the default; see the note below
```

`codex` has no CLI subcommand that reports quota, so `pie usage codex` is the one
place pie can reach a vendor endpoint directly (the ChatGPT usage endpoint, with
the credential from `~/.codex/auth.json`). It is read-only and local to your own
credential, and it is **off by default**: nothing is requested unless you opt in
with `quota_probe = "http"` or `PIE_CODEX_QUOTA_PROBE=http`. With the default
`off` no request is constructed at all, and session-log totals still work. See
`knowledge/9router-findings.md` for why the default is off.

## Speed — where the time actually goes

Measured on the development machine, 2026-10-12:

| | Time | Why |
|---|---|---|
| `pie` startup | **0.079 s** | pie's own cost, negligible |
| `pie auth` | 0.091 s | no subprocess at all |
| `pie --list-backends` | 0.143 s | served from the version snapshot |
| `pie usage agy` (cached) | **0.063 s** | agy's quota moves slowly |
| `pie usage agy` (probe) | 5–10 s | agy's own initialisation, not pie's |
| a codex turn | ~3.6 s | |
| an agy turn | ~40 s | ~10 s of it is agy's init, the rest inference |

Two conclusions worth knowing before you try to make it faster:

1. **pie is not the slow part.** Its startup is 79 ms. Everything else is the vendor
   CLI. Do not micro-optimise pie; the lever is not spawning, or spawning less.
2. **`agy` is the slow backend by a wide margin** — a real turn is 40 s against
   codex's 3.6 s, and roughly 10 s of that is agy's fixed per-invocation setup,
   which pie cannot remove. Prefer codex when latency matters.

Two caches exist for exactly this reason, and both are labelled so a cached value
is never mistaken for a live one:

- `agy.usage_cache_ttl_secs` (default 300) — the `/usage` probe. Turn it off with
  `0`.
- the version snapshot written by `pie doctor`, reused by `pie --list-backends`
  for up to six hours. `doctor` always re-probes, so run it when you want the
  truth.

Note that running `cargo run -- …` costs ~0.4 s of cargo overhead on every
invocation, six times pie's own startup. Use the built binary:
`./target/release/pie.exe`.

Parallelising the three version probes was tried and **does not help** (1.54 s
sequential versus 1.46 s concurrent): the cost is Windows process creation plus
antivirus inspection, not something pie controls. Caching is the fix.

## Platform support

Built and tested on Windows (`x86_64-pc-windows-msvc`), required to work on Linux
and WSL. WSL2 is installed on the development machine but cannot currently boot,
so Linux is verified by the CI matrix and by a cross-target check rather than
locally. Details, including the per-behaviour `cfg` table and what is *not*
verified: `planning/PLATFORMS.md`.

## State

Everything pie owns lives under `~/.pie` (override with `PIE_HOME`):

```
~/.pie/
├── config.toml
├── cache/            # model catalogs, CLI version snapshots — no credentials
└── usage.jsonl       # append-only, one row per turn (frozen schema)
```

`usage.jsonl` records token counts per turn — including `cached_input_tokens`,
so cache-hit rate is measurable — and is deliberately compatible with the
earlier JS implementation's file. It never contains a credential.

## Documentation

| File | What it is |
|---|---|
| `planning/PLAN.md` | scope, architecture, and the safety framing |
| `planning/ROADMAP.md` | M0–M10, in order, with a "done" condition each |
| `planning/DESIGN.md` | **the implementation contract** — types, traits, process rules, argv, on-disk state |
| `planning/ACCEPTANCE.md` | frozen gates G1–G6, invariants I1–I11, the run log, and the bug log |
| `planning/RISKS.md` | known future breaking points, by blast radius |
| `planning/PLATFORMS.md` | Windows / WSL / Linux: behaviours and how each is verified |
| `planning/DEFERRED-BACKENDS.md` | the deferred API-key backends, as a design rather than dead code |
| `knowledge/` | reverse-engineered facts about each CLI, plus reference studies |
| `tests/README.md` | what each test proves, and what runs when |

## Development

The gates are `planning/ACCEPTANCE.md` §1. Run them after every change:

```sh
bash scripts/qc.sh          # Linux / WSL / macOS — G1 fmt, G2 clippy, G3 test, G4 acceptance
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/qc.ps1   # Windows
bash scripts/qc.sh --live   # adds G5, which SPENDS REAL SUBSCRIPTION QUOTA
```

G5 uses only `agy/gemini-3.8-flash` and `codex/gpt-6-luna`; the harness refuses
any other model. Everything else — including the whole parser suite — runs
against recorded fixtures and spends nothing.
