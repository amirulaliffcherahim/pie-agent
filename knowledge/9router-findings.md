# 9router — what we learned, and what we may not copy

Reference notes from a read-only sweep of [`decolua/9router`](https://github.com/decolua/9router),
reviewed on 2026-10-12 as prior art for two questions: how does it handle
Gemini/Codex/Claude plans, and how does it make token usage cheaper?

The clone lived in the gitignored `.scratch/9router` and is **not** part of this
repository. Findings come from three independent sweeps (auth/quota, token
economy, transports) and are cited as `path:line` against that clone.

> **REFERENCE MATERIAL.** Not a contract, not a plan. `planning/DESIGN.md` is
> authoritative for pie. This file exists so the same research is never repeated
> and so nobody re-derives the compliance boundary by accident.

## 0. Read this first: 9router is a compliance anti-pattern

9router is a **credential broker and reverse proxy**. It does not drive the
vendor CLIs at all — it calls the vendors' HTTP APIs with CLI-impersonating
headers, holding its own OAuth clients and tokens in its own database.

| What 9router does | Why pie must never do it |
|---|---|
| Own OAuth clients per provider, `accessToken`/`refreshToken` in its own SQLite `providerConnections` table | `PLAN.md` §5: pie drives each CLI's *own* login and stores no tokens. This is the pattern that triggered Google's Feb 2026 suspension sweeps |
| Reads no CLI credential store — it *writes* `~/.codex/config.toml` and `~/.claude/settings.json` to redirect the CLIs at itself | We may not repoint a vendor CLI at a third party, but the *inverse* (never reading their token files) is exactly our rule |
| MITM TLS proxy rewriting `cloudcode-pa` → `daily-cloudcode-pa` | Intercepting vendor traffic. Categorically out |
| Renames client tools with a `_cc` suffix inside subscription requests, plus decoy tools, to look like the official client | Deliberate detection evasion. This is an anti-ban measure, and the reason the whole approach is risky |
| `quotaAutoPing` sends real requests to *warm* the 5h rate-limit window after a reset | Wasting vendor capacity on purpose; indistinguishable from abuse |
| Calls private endpoints: `chatgpt.com/backend-api/codex/responses`, `chatgpt.com/backend-api/wham/usage`, `api.anthropic.com/api/oauth/usage`, `cloudcode-pa.googleapis.com/v1internal:*`, `daily-cloudcode-pa.googleapis.com/v1internal:*` | `PLAN.md` §12 / `internals.md` "Critical do-nots": drive the CLI, never the private API |
| Every subscription provider is flagged `deprecated: true, deprecationNotice: "RISK_NOTICE"` **by its own authors** | The authors know. That is the loudest possible warning |

Two consequences for pie:

1. If a future contributor proposes "just call the endpoint directly, it's
   faster", this section is the answer. It is not a performance question.
2. `/api/oauth/usage` and `wham/usage` are the *same* private quota endpoints
   `PLAN.md` §6 contemplates for `pie usage codex`. **This is a live design
   tension worth an explicit decision**: 9router demonstrates the shape of the
   risk. Reading `~/.codex/auth.json` to call `wham/usage` is the least-bad
   version of it (the vendor document is the user's own credential and the
   endpoint is read-only), but it is not obviously inside "drive the CLI only".
   Treat it as an open question, not a settled design — see §4.

## 1. What transfers — token economy

Savings marked *(vendor claim)* come from 9router's README/CHANGELOG, not from
independent measurement. They are directional, not promises.

| # | Technique | Survives our constraint? | Where it lands in pie |
|---|---|---|---|
| 1 | **Resume instead of resending history.** Derive a stable session id (client header → hash of accumulated assistant text → workspace) so the vendor's own automatic prefix cache keeps hitting; one long-lived conversation per task rather than one-shotting with a rebuilt transcript *(20–40% input tokens, vendor claim)* | **Yes** — pie already builds `--conversation` / `exec resume` argv and has a `resume` capability flag | **M5** (session persist/resume), and the argv is already in place from M1/M2 |
| 2 | **Terseness directive inside the prompt.** ~120 tokens of output-shaping instruction appended to the system prompt, made *idempotent* and inserted *inside* the stable prefix so it does not break cache hits *(up to 65% output tokens, vendor claim)* | **Yes** — pie owns the prompt text it composes (M3 profile projection) | **M3** |
| 3 | **Answer trivial turns locally.** Recognise warmup/title/"count this"/naming requests and answer them without calling any provider, returning a synthetic minimal response | **Yes** — pure client-side pattern matching; no vendor involvement | **M5** (a guard in the loop / pre-flight classifier) |
| 4 | **Route by task class to the cheapest model, and lower reasoning effort for cheap classes.** Effort dominates billed reasoning tokens; their Codex default is `low`, not `high` | **Yes** — pie chooses which CLI and which effort per call | **M3/M4** (`pie route <task>`), with the catalog from M1/M2 |
| 5 | **Compress/truncate *at the boundary*, fail-open, never grow.** Only replace text when the result is strictly shorter; skip inputs below a size floor; never compress error output | **Partly** — pie cannot reach inside a vendor's tool loop, but it fully controls what *it* pastes in (diffs, file dumps, logs) | **M3/M5** |
| 6 | **Per-model, not per-account, cooldown** for capacity errors, with a persisted expiry and a status-code *and* message rule table | **Yes** — capacity is per model, and pie spawns per model | **M5** (retry policy) |
| 7 | **Do not retry a 429.** Their config is literally `429: {attempts: 0}` while 502/503/504 get 2–3 attempts | **Yes** | **M5** (retry policy) |
| 8 | **Refresh dedup + lead time** — one in-flight refresh shared by concurrent callers, with a per-provider lead (Codex: 10 min, not 5) | **N/A** — pie never refreshes a token; the CLI does. Useful only as a caution about *our* retry storms | — |
| 9 | **Cap runaway output.** They clamp `max_tokens` (defaults 64 000 / floor 32 000) | **No** — pie cannot set `max_tokens` on a CLI subprocess | — |
| 10 | **Tool-schema dedup.** Drop built-ins shadowed by MCP tools; exact-name dedup | **Partly** — only relevant once pie owns a tool inventory (`ocr` MCP server, M5/M9) | **M9** |

## 2. Boundary-truncation numbers worth borrowing

9router's `rtk/constants.js` is a set of hard-won caps for the exact content pie
also has to paste. Directly reusable as pie's defaults:

| Limit | Value |
|---|---|
| `git diff` max lines | `500` (`maxLines` default) |
| git diff hunk max / git log max | `100` / `200` lines |
| smart-truncate head / tail / minimum before truncating | `120` / `60` / `250` lines |
| per-file grep results / per-dir find results / total find dirs | `10` / `10` / `20` |
| `git status` files listed | `10` |
| directory tree lines | `200` |
| compression size floor (do not bother below) | `500` bytes |
| "phantom savings" gate — reject a shrink below this ratio | `5 %` |
| token estimate (no real tokenizer) | `chars / 4` |

Two rules that matter more than the numbers: **never emit a longer payload than
you were given**, and **fail open** — if compression errors, pass the original
through. A compression step that can lose content or blow up the request is worse
than no compression step.

## 3. Transport lessons (for M9 / M10 only)

- **Claude**: the stream is `event:`-framed SSE with `message_stop` as the
  terminal. Cache-token counts ride `message_start.usage`; `message_delta` carries
  output tokens only. Parsing usage from the delta alone silently loses cache
  tokens — a trap for our M9 adapter.
- **Codex**: 9router speaks the ChatGPT **Responses** API (`response.completed`,
  `response.output_text.delta`), *not* `codex exec --json`. It therefore cannot
  corroborate our `item.completed` / `agent_message` / `turn.completed` contract —
  that contract stays CLI-verified only (`knowledge/codex.md`), which is what we
  claimed anyway.
- **Antigravity**: 9router only has the IDE HTTP transport
  (`v1internal:streamGenerateContent`), so it cannot corroborate agy's
  `init` / `step_update` / `result` envelope either. Our contract remains
  live-verified from `agy --print` itself.
- **Robustness patterns worth copying regardless of transport**: split on whole
  lines only and keep the trailing partial in a buffer; treat a short or
  non-`data:` line as `null` rather than an error; `switch` on event type with no
  `default` (unknown events are silently ignored); re-parse the tail buffer on
  flush to catch a terminal frame that arrived without a trailing newline; reject
  a non-SSE body (an HTML error page) before treating it as a stream.
- **Timeouts**: connect 60 s, and a stall watchdog on *raw upstream bytes* (360 s)
  rather than on transformed output. pie's equivalent is the whole-turn deadline
  in `src/process.rs`; per-byte stall detection is a possible M5 refinement.

## 4. Open questions this raised

1. **Is `wham/usage` acceptable?** `PLAN.md` §6 plans `pie usage codex` via the
   ChatGPT usage endpoint using the token from `~/.codex/auth.json`. 9router shows
   the endpoint and the risk. Options: (a) keep it, documented as read-only and
   credential-local; (b) drop it and report codex quota as unavailable; (c) derive
   what we can from `turn.completed` usage in the session log. Decide explicitly
   and record it in `DESIGN.md`, do not drift into it.
2. **Does resume actually beat a fresh turn?** Recommendation 1 is the largest
   claimed saving and the cheapest to test. Once M5 exists, measure
   `input_tokens` for the same task with and without resume using the frozen
   session log — no new instrumentation needed.
3. **Is a terseness directive worth the prompt cost?** It is ~120 input tokens per
   turn to cut output tokens. Measure it, do not assume it; a one-word answer
   costs more with the directive than without.
