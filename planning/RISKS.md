# pie — Review: future breaking points & hiccups

Pass over PLAN / ROADMAP / knowledge. Ordered by blast radius. **New** marks a
gap not already captured in PLAN §12.

## P0 — decide before M6/M7

### 1. Tool-loop asymmetry (biggest) — NEW
- agy `--print` and codex `exec` are **one-shot**: each runs its *own* tools
  (sandboxed) and returns a final answer. They do not accept pie-injected
  `tool_result` mid-turn. agy's `--input-format stream-json` is "one turn per
  stdin line", not mid-turn tool replies.
- claude `-p --input-format stream-json --output-format stream-json` is SDK mode
  and **does** support `tool_use` → `tool_result` round-trips (verify live).
- Impact: "keep pi's read/bash/edit/write" only works for claude (and the
  commented HTTP backends). For agy/codex, pie must run **delegate mode**
  (whole task → CLI → final answer, no pie tool cards).
- Fix: add `supports_tool_loop` to the Provider trait; the agent loop picks
  delegate vs tool-loop per backend.

### 2. Provider trait needs a capability flag — NEW
- `stream()` alone can't express "final answer only" vs "tool_use events".
- Add `fn capabilities(&self) -> Capabilities { tool_loop: bool, resume: bool }`.

## P0/P1

### 3. CLI auto-update breaks parsers
- `agy update`, `codex update`, npm claude updates change flags/JSONL silently.
- `pie doctor` records versions; parse tolerantly; unknown events pass through;
  surface a version-mismatch warning instead of hanging.

### 4. claude is unverified and not logged in
- knowledge/claude.md is "expected" schema only; real field names may differ.
- M3 first step: `claude /login`, then one live `claude -p "hi"
  --output-format stream-json` to confirm the envelope.

### 5. agy has no stdin prompt path
- Long prompts have no file/stdin fallback for agy (argv only) → Windows ~32k cap.
- codex: stdin `-`; claude: `--input-format stream-json` stdin.
- Mitigation: cap agy prompt (~20k) + warn, or split via `--conversation` resume.

## P1

### 6. Resume id lifecycle
- `--conversation` / `--resume` ids can be purged/rotated (codex archive, agy db,
  claude retention). Resume failure → re-bootstrap from pie's transcript.

### 7. Quota exhaustion + token-spending usage probes
- 5h/weekly caps; auto-routing + retries burn quota fast. Gate retries on quota.
- claude `/usage` may be a model turn → don't poll it; use per-turn `result.usage`.

### 8. HTTP backends deferred but need tool-loop
- Re-enabling OpenRouter/DeepSeek later needs the SSE `tool_calls` path that
  delegate-mode CLIs don't exercise. Keep the tool-loop code path shared from M0.

### 9. OAuth expiry / re-login surfacing
- CLIs refresh internally; pie must map auth failure → "re-login", not crash/hang.

### 10. API-key env vars silently switch billing — NEW
- If `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` / `GOOGLE_API_KEY` are present in the
  process env, claude/codex/agy may flip from subscription to pay-as-you-go.
- pie must scrub these from the child env before spawning, require codex
  `auth_mode` = subscription, and refuse a CLI backend when only an API-key
  credential is available.

## P2

### 11. OCR itsmini blocked
- itsmini (Baidu) server is a separate unbuilt project; `ocr` is a stub until it
  exists (stand-in: a clear "itsmini not reachable" error).
- Reachability: expose `ocr` over **MCP**, not as a pie-native tool — otherwise
  delegate-mode CLIs (agy/codex) can never call it.

### 12. Windows shims / argv / hidden windows
- Already gates: node launcher for codex, `CREATE_NO_WINDOW`, argv cap. Keep green.

## Automation (watchtower / know-me)

### 13. Background jobs can burn sub quota — NEW
- LLM-summarizing sessions/news can silently spend premium sub quota. Budget it,
  default to a cheap/local model, opt-in cadence, skippable.

### 14. REA drift needs a disassembler (unavailable) — NEW
- No Hopper/Ghidra → degrade to version + `--help` + binary-strings diff
  ("REA-lite"); full REA only after `rea setup`.

### 15. Privacy of "know me" — NEW
- Reads all sessions; must be local-only, opt-in, and never send raw transcripts off-machine.

### 16. Memory drift / staleness — NEW
- Consolidate + provenance + decay so learned preferences don't rot or contradict;
  keep memory reviewable and revertible.

## Already covered (PLAN §12)
ToS gray area · protocol fragility · argv limit · claude install.
