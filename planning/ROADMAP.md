# pie — Roadmap (move one milestone at a time)

> **Status lives in `ACCEPTANCE.md` §12's run log, not here.** This file says what to
> build and what each milestone's Done condition is; §12 records which milestones
> passed a gate, at which commit, with what evidence. Do not infer progress from this
> file. (An earlier version opened with "Nothing is built yet", which stayed there for
> six milestones — a stale line at the top of the entry-point document is how a fresh
> session concludes there is nothing to continue.)

Work in order; don't start M(n+1) until M(n)'s checks pass.
Read `RISKS.md` before M0/M3/M5/M7/M8. Versioning per `VERSIONING.md`.

**Live path = agy + codex.** claude is future-proofing (no subscription); the HTTP
backends are commented out. Neither blocks anything below.

## M0 — Scaffold + provider trait
- `cargo init pie`, deps: `tokio`, `clap`, `anyhow`, `thiserror`, `tracing`.
- `~/.pie/config.toml` resolution; `Provider` trait + `Capabilities { tool_loop, resume }` + registry.
- Stubs: agy, codex (live) · claude (future-proof) · openrouter/deepseek (commented).
- Env hygiene: scrub `ANTHROPIC_API_KEY`/`OPENAI_API_KEY`/`GOOGLE_API_KEY` from child processes.
- **Done:** `cargo run -- --list-backends` lists agy/codex/claude.

## M1 — agy backend (delegate) — LIVE
- Port verified agy stream-json (`init`/`step_update`/`result`); hidden spawn; `--effort` default; prompt cap.
- Models from `agy models` + cache.
- **Done:** `pie -p --model agy/gemini-3.8-flash "hi"`; `pie usage agy`.

## M2 — codex backend (delegate) — LIVE
- Port `codex exec --json --approve-for-me` (`item.completed` → `agent_message`).
- Require `~/.codex/auth.json` `auth_mode` = subscription.
- **Done:** `pie -p --model codex/gpt-6-luna "hi"`; `pie usage codex`.

## M3 — Unified profile, memory, routing
- `~/.pie/profile.toml` + `AGENTS.md` + `memory/*.md`; projection per backend.
- Backend switch mid-session re-bootstraps from pie's transcript.
- **Done:** same persona on agy and codex; no per-CLI config.

## M4 — `pie auth` + `pie usage` unified
- `pie auth`: agy + codex live; claude "no subscription"; API rows commented.
- `pie usage`: unified weekly/monthly + session-log token/cost totals (no token spend).
- **Done:** both render without a selected model.

## M5 — Agent loop (delegate), tools, sessions
- Delegate mode for agy/codex; session JSONL persist/resume; compaction.
- `ocr`: `pie ocr <file>` + **MCP server** so delegate CLIs can call it (itsmini stub).
- **Done:** `pie -p "audit this repo"` delegates + persists; resume works.

## M6 — ratatui TUI
- Model picker, streaming output, keybindings.
- **Done:** interactive parity across agy + codex.

## M7 — Daily "know me" (personalization)
- `schedule.rs` + `learn.rs`: mine pie + CLI sessions → profile/memory updates.
- Consolidation + provenance + decay; local-only; cheap/local model budget.
- **Done:** after a few sessions, `profile.toml`/`memory/*.md` reflect real habits;
  `/memory` shows what pie learned and lets you revert it.

## M8 — Monthly watchtower (news + CLI drift)
- `watch/news.rs`: AI news digest ranked by impact on your stack.
- `watch/drift.rs`: version + `--help` + strings diff per CLI ("REA-lite"); REA
  structural checks when a disassembler is available.
- **Done:** `~/.pie/reports/watchtower-YYYY-MM.md` produced + notified; drift diff
  shows any flag/schema/policy change.

## M9 — (future-proof) claude backend + tool-loop
- Implement claude adapter to spec; verify live when a subscription exists.
- Build `tool_use`→`tool_result` tool-loop (reused by HTTP backends).

## M10 — (optional) re-enable OpenRouter / DeepSeek
- Uncomment the two HTTP backends (SSE + tool_calls + models + usage).

## M11 — the tool suite for the loop

M9 built the `tool_use` → `tool_result` loop and shipped **no tools**: every call is answered
with a stated error. `PLAN.md` §2's `agent/tools.rs` (read/bash/edit/write/grep/find/ls) is this
milestone. It is new scope — the roadmap above ends at M10 — so it gets its own frozen
contract (`DESIGN.md` §20, `ACCEPTANCE.md` §19) **before** any code, like every other one.

- Read-only tools first: `read`, `ls`, `grep`, `find` are safe by construction.
- Mutating tools (`bash`, `write`, `edit`) are **opt-in and scoped**, and the decisive
  constraint is that M9's loop has **no user in it** — there is no channel on which to approve
  a command mid-turn, so a tool is either enabled in config with a declared scope or not
  offered at all.
- The turn's spawn policy (hidden window, env scrub, a deadline, a bounded output, a kill
  tree) must come from **one** owner, not a third copy: M10 already recorded a second spawn
  path as a risk to I1/I2/I3.
- **Done:** with an empty config, no tool runs at all; with a read-only tool enabled, a
  scripted turn reads a file and answers; with `bash` enabled and scoped, a scripted turn runs
  one command, and a command that outlives its deadline is killed rather than waited on.
- **Done as built (M11, verdict `offline-verified`).** The read-only tier and a scoped `bash` are
  implemented, and the spawn policy has **one** owner: `ChildTransport` is deleted and
  `process::ChildSession` is the only way a child is created, asserted rather than assumed.
  **`write` and `edit` are not built** — their keys exist and every call is refused with a sentence
  saying so. `ACCEPTANCE.md` §19 freezes three stages and gives the mutating tier no criterion at
  all, so building it would have produced a feature with nothing defining it; recorded as **S18**.
  The scripted-turn half is satisfied at the tool boundary rather than through a model, because no
  backend with a working loop has a credential here — which is why the verdict is
  `offline-verified` and not `done`.

## M12 — the TUI shell and the `/` palette

M6 shipped a working but deliberately minimal TUI, opt-in so no script or CI job could enter
it by accident. M12 makes the TUI the default interface and redesigns its surface, following
how `agy`, `codex` and `pi` look and are operated: a fixed composer with a scrolling
transcript, a footer carrying folder/session/model/usage/cost, tool calls as collapsed
blocks, and a `/` command palette.

It is a **surface** change, not a rewrite: the reducer stays pure (no crossterm type reaches
it), the renderer stays assertable through `TestBackend`, and only the terminal loop stays
non-automatable. Keeping that seam is what carries M6's tests forward.

- Bare `pie` opens the TUI **when stdin and stdout are both terminals**, and prints help
  otherwise. M6's guarantee moves from "the TUI is not the default" to "it requires a
  terminal on both streams" — and the pipe half becomes a gate, because it is now the only
  thing between a CI job and a full-screen UI.
- The `/` palette is a **const registry**, one row per command, and the menu, the filter, the
  completion, the footer and the tests all read that one table (the B26 rule).
- `/tools [list|run]` reports each answered tool's **effective** state and runs one
  **directly, with no model turn** — which is the only interactive path to M11's tier on a
  machine whose loop-capable backends have no credential.
- Enabling a tool stays in `config.toml`. A TUI that rewrote it would be making the
  enable-a-tool decision repeatedly, which §20 exists to prevent.
- **Out deliberately**, because pie's architecture never receives the data: live tool output
  while a command runs, approval prompts, `/tree`/`/fork`/`/clone`, diff and artifact
  viewers, subagent trees, Vim mode, custom status-line scripts.
- **Done:** `pie` with no arguments on a terminal shows the transcript, composer, footer and
  `/` palette; `/tools run read {"file_path":"Cargo.toml"}` returns the file with no model
  turn and no quota; `pie` on a pipe prints help and exits 0.

## Cross-cutting gates (green every milestone)
- No console popups; latency logged; `pie doctor` health; bounded prompt argv;
  version recorded for drift detection; subscription-only auth (no API keys);
  background jobs budgeted + opt-in.
