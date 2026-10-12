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

## Cross-cutting gates (green every milestone)
- No console popups; latency logged; `pie doctor` health; bounded prompt argv;
  version recorded for drift detection; subscription-only auth (no API keys);
  background jobs budgeted + opt-in.
