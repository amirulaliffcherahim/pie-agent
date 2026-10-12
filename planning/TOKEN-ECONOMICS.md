# Token economics for pie — extraction and plan

Reference: `knowledge/token_optimization_and_agent_engineering.md` (652 lines). Study sections are cited by heading below. This file is the pie-specific extraction — a planning document, not a contract, and it does not amend the frozen `DESIGN.md` §7 schema.

## 1. What this is

The study describes how systems that **own the request payload and the inference engine** buy token efficiency: cache breakpoints on the wire, grammar-constrained decoding, per-token pruning with a local evaluator LM, KV-cache reuse in a serving runtime, cascades through a router the operator controls. pie owns none of that. pie spawns `agy`, `codex`, or `claude` as a hidden subprocess and parses its stream (`PLAN.md` §2, `DESIGN.md` §6), so the vendor's API shape, tokenizer, serialization, decoding constraints, and prefix-cache policy are all outside pie's reach. Every technique below is therefore scored on one question: *does the lever sit on pie's side of the subprocess boundary?*

What is on pie's side is narrow but real: the text pie composes into the prompt (M3), which model and `--effort` it selects, whether it delegates or splits, whether it starts fresh or resumes a stored conversation id, how much of its own tool output or file content it re-inserts, what it caches on disk, and — from M5 — its own loop ceilings, cycle detection, and budget fences. The honest headline: pie's wins come from **avoiding self-inflicted damage** (breaking a stable prefix, re-sending history, bloating its own prompt), not from claiming discounts the study attributes to payload ownership.

## 2. Constraint table

| Technique family (study section) | Transfers to a CLI-driving tool? | Closest achievable equivalent in pie | Milestone |
|---|---|---|---|
| Prompt / prefix caching — § *Mechanics of Prefix Caching and KV-Cache Reuse* | **No** — breakpoints are a wire-level field (`cache_control: {"type":"ephemeral"}`); TTLs, minimums, and block sizes are server policy | Never *destroy* the prefix the vendor already caches: byte-stable profile head, volatile data last, no timestamp in the projection | M3 |
| System-prompt dieting — § *Structural System Prompt Optimization and Schema Dieting* | **Yes, fully** — the one prompt pie owns end to end | Imperative-compress the `~/.pie/AGENTS.md` / `profile.toml` projection; keep the system prompt minimal (`PLAN.md` §9 already commits) | M3 |
| Deterministic prompt ordering — § *Deterministic Prompt Ordering for Maximum Prefix Cache Hit Rates* | **Partly** — pie cannot set the cache tier, but it does decide element order in the text it composes | Static persona/rules → format conventions → durable memory → task text; every volatile value at the tail | M3 |
| Structured-output minimisation — § *Structured Output Minimization* | **No** — the vendor CLI emits the tokens, in its own format | Instruct line-oriented output in the M3 projection; pass a vendor flag through (`claude --json-schema`) — a passthrough, unverified | M9 |
| Algorithmic context compression — § *Information-Theoretic and Distillation-Based Compression* | **No** — needs a local evaluator LM and control of the payload | Only the human variant: imperative compression of pie's own text. Not LLMLingua-style token pruning | M3 (text), never (algorithm) |
| Memory compaction / summarisation — § *Memory Compaction: Sliding Windows and Semantic Summarization* | **Partly** — pie cannot compact the vendor's transcript, though it can forward a vendor compaction flag | Compact what pie owns: session JSONL summarised on resume; a semantic-bookmark state block (`<200` tokens) instead of replayed history | M5 |
| Semantic caching — § *Semantic Caching and Vector Retrieval Deduplication* | **No — and must not** | Disk caches only for deterministic artifacts pie fetched (model catalogs, version snapshots, OCR results). Never a cached model answer | M0+ (already), refuse otherwise |
| Loop guardrails — § *Loop Guardrails: Breaking Cycles and Bounded Budgets* | **Yes** — loop control is pie's own code | Iteration ceiling, repeat-action hash, token/wall-clock fence, escalate-on-ceiling; recorded via the existing `stop_reason` | M5 |
| Tool-schema pruning — § *The Hidden Token Tax of Large Tool Inventories*, § *Dynamic Tool Indexing and Semantic Tool Retrieval* | **Yes, in the M9 loop only** | Micro-short descriptions for pie's 7 tools; expose exactly **one** MCP tool (`ocr`), opt-in | M9 |
| Code-as-action — § *Code-as-Action vs. Atomic Tool Calling* | **Yes, in the M9 loop only** | Prefer one `bash` script over N `read`/`grep`/`edit` round-trips so intermediates never re-enter context | M9 |
| Model cascading / routing — § *Automated Model Cascading and Semantic Routing* | **Partly** — pie can pick provider/model/effort per request, but cannot cascade mid-turn | `pie route <task>`-style heuristic selection (`PLAN.md` §8); `--effort` as the reasoning-budget stand-in | M3 (route), M4 (quota-aware) |
| Reasoning / effort budgeting — § *Classification of Token Streams: Prompt, Completion, and Reasoning* | **Partly** — no thinking-budget parameter, but effort is exposed | agy `--effort low\|medium\|high\|xhigh\|max`; codex reasoning levels. Default to the lowest effort that passes | M1, M2 |
| Token-explosion growth curve — § *First Generation Architectures: AutoGPT, BabyAGI, and Unbounded ReAct*, § *The Mathematical Reality of Token Explosion* | **Only where pie owns the loop** — in delegate mode those iterations happen inside the CLI, over observations the CLI owns | pie's only lever is whether it starts *another* delegated turn; ceilings and fences apply from M5 (see §5) | M5 / M9 |
| Context encapsulation / sub-agent isolation — § *Third and Modern Generation Architectures* | **Already satisfied structurally**; the runtime parts do not transfer | Delegate mode *is* the isolated-scratchpad pattern: the CLI burns thousands of tokens internally and pie receives only the final answer | M0–M2 (free), M3/M5 for the supervisor layer |
| Context density over volume — § *Strategic Syntheses and Future Trajectories* | **No direct lever** — pie cannot see or reshape the vendor's context window | Reduce only what pie itself inserts: profile text, re-sent history, tool output | M3 / M9 |
| Layered efficiency architecture (4 tiers) — § *The Production Enterprise Blueprint* | **Partly** — only the gateway/routing tier and the loop tier are pie's | Tier 1 (routing): M3/M4 model+effort selection. Tier 3 (loop): M5 guardrails, M9 offloading. Tiers 2 and 4 are the vendor's | M3–M9 |
| Inference-server tuning — § *Attention Mechanics and Context Memory Footprints*, § *Production Cost-Fidelity Trade-off Analysis* | **No** | Nothing. pie has no serving runtime, no KV cache, no batching, no tokens-per-second knob | — |

## 3. Tier 0 — measurable now, no new machinery

Everything here reads `~/.pie/usage.jsonl`, whose row is frozen (`DESIGN.md` §7): `ts, provider, model, input_tokens, cached_input_tokens, output_tokens, reasoning_tokens, cost_usd_micros, credits_consumed, credits_remaining, duration_ms, ok, error_kind, stop_reason`. No new fields are required for any of these.

| # | Change | File it touches | How you measure it |
|---|---|---|---|
| T0-1 | **Per-turn and per-provider cache-hit view.** `Totals::cache_hit_rate()` already exists in `src/report.rs` and already renders in `pie usage`; the aggregate hides the signal. Add a per-provider breakdown and a *cacheable-but-not-cached* count: rows where `input_tokens >= 1024` (the study's stated minimum cacheable prefix) and `cached_input_tokens == 0` | `src/report.rs`, `src/commands/usage.rs` | Count and share of cacheable-but-uncached rows per provider per window. The number to drive down; it is unmeasured today |
| T0-2 | **Failure-class breakdown in totals.** `Totals` counts `failed_turns` but not `error_kind`, so a prompt-cap failure is invisible beyond "a failed turn". `SessionLogRow::from_error` already stores the class (`src/session_log.rs`) and `print.rs` already writes a row for every failed turn | `src/report.rs`, `src/commands/usage.rs` | Rows by `error_kind`; specifically the `prompt_too_long` count and the `duration_ms` sum burned on those rows. Target: zero |
| T0-3 | **Argv-cap pressure warning before the cap, not after.** The cap is fatal today (`PieError::PromptTooLong`, `src/error.rs`; agy has no stdin path, so it cannot degrade to stdin) and the effective cap is `Capabilities::max_prompt_argv` tightened by `general.prompt_argv_cap` (`src/config.rs`, default 20 000 chars). Warn at ~80 % of the effective cap and name the composition: profile text vs. user text vs. folded context | `src/commands/print.rs`, `src/config.rs`, `src/provider/agy.rs` | T0-2's `prompt_too_long` count → zero, plus the `input_tokens` distribution moving off the top end. **Not measurable:** the *quality* cost of a near-cap prompt has no field and no signal |
| T0-4 | **Degenerate-resume detector (read-only report).** The study's retention table (5 min ephemeral on Anthropic, 5–10 min dynamic on OpenAI, hourly on Gemini) gives the heuristic: a resume whose gap since the last row for the same provider exceeds the TTL *and* whose `cached_input_tokens == 0` is paying full prefill for history it cannot cache | `src/report.rs` (report now), `src/commands/print.rs` (enforcement in M5) | `ts` delta vs. `cached_input_tokens == 0` vs. `input_tokens`, per provider session. **Caveat:** pie records no resume marker, so attribution is a heuristic — see §8 |
| T0-5 | **Prefill-bound vs decode-bound classification per turn.** A slow turn with large `input_tokens` and small `output_tokens` is a re-send (fix: fresh start or compaction); a fast-prefill turn with large `reasoning_tokens` is an effort-selection problem (fix: lower `--effort`) | `src/report.rs`, `src/commands/usage.rs` | Correlation of `duration_ms` against (`input_tokens`) vs. (`output_tokens + reasoning_tokens`), per provider. Makes T1/T2 changes attributable |
| T0-6 | **Wasted-wall-clock line.** `duration_ms` summed over `ok == false` rows, separated by `error_kind`. A stall that hits the whole-turn deadline (`PLAN.md` §9 watchdog, 600 s default) is pure loss and currently invisible in `pie usage` | `src/report.rs`, `src/commands/usage.rs` | Sum and share of wall clock spent on failed rows. Should fall once T0-3 and M5's stall handling land |
| T0-7 | **Cross-check pie's stream-derived totals against the vendor's own quota report.** `PLAN.md` §9 commits to taking token totals from stream events rather than re-reading transcripts, which means a vendor stream change silently drifts pie's numbers. `pie usage <backend>` already fetches buckets, so the comparison is free | `src/commands/usage.rs`, `src/report.rs` | Session-log `input_tokens` totals per window vs. the quota delta reported by the backend. A persistent divergence is a protocol-drift signal (`RISKS.md` §3) |
| T0-8 | **Baseline before optimising.** A `pie usage --tokens` view printing the T0-1…T0-7 metrics over the existing log, so later claims have a "before" | `src/commands/usage.rs` | The tier-0 metrics per provider, all time / 7 d / 30 d. Without this, no Tier-1 or Tier-2 claim is falsifiable |

## 4. Tier 1 — belongs to M3 (profile / memory projection)

This is the only prompt text pie fully owns, so it is where the study's prompt-family advice actually lands. The study's own framing is a discount pie cannot invoke: `cache_control` breakpoints, the 90 % cached-input price, the 85 % TTFT cut, and 128-token block matching are wire- or server-level. pie **cannot set a breakpoint**; it can only avoid **destroying** whatever stable prefix the vendor already has — which, by the study's own ordering section, is worth real money.

- **Static-first ordering.** Adopt the study's tier order for the projection: persona/rules → format conventions → durable memory → task text. The volatile part goes last, always.

- **No volatile data in the cacheable prefix.** The study's named anti-pattern — a timestamp (or session id, hostname, cwd, model id, "current date", quota snapshot) in the first line of the system prompt — is entirely pie's choice, and the cost is not just tokens: it invalidates the prefix on *every* invocation.

- **Determinism is a caching requirement, not a style preference.** The cacheable unit is a leading byte run, so the projection must be byte-identical between runs with identical inputs. In Rust this rules out `HashMap`/`HashSet` iteration order (randomised per process), locale- or timezone-dependent formatting, and any "most recent N sessions" query whose membership shifts between runs. This is the most likely way M3 destroys its own prefix while looking correct in review.

- **Two projections per profile: stable block and volatile tail.** `memory/*.md` facts are session-invariant and belong in the stable head; session digests, recent-outcome summaries, and quota notes belong in the tail. Mixing them makes the whole file uncacheable.

- **Per-backend projection is a separate prefix.** agy folds the profile into the *user prompt* (no system role, `PLAN.md` §8) while codex/claude take a system prompt, so byte-stability must hold per backend independently. A single "canonical" profile ordered for claude may sit in the wrong position for agy.

- **The agy argv interaction.** Because agy receives profile text inside argv, a fat profile does not merely cost tokens — it competes with the user's own text for the same 20 000-char budget and makes `PromptTooLong` likelier. Profile size is a correctness constraint for agy, not only a cost.

- **Imperative compression** (§ *Structural System Prompt Optimization and Schema Dieting*): the study's 28-token directive → 11-token imperative example. Apply to `~/.pie/AGENTS.md`; replace transcript-style few-shot examples with one compact input→output pair.

- **Minimal system prompt** stays as `PLAN.md` §9 commits. Every line added to the head of the projection is paid on every turn and is a prefix-breaking risk.

- **M7 interaction (cross-milestone).** The daily "know me" job rewrites `profile.toml` / `AGENTS.md` / `memory/*.md`. If consolidation rewrites the *head* of the stable block, it invalidates the vendor's prefix once a day. Rule: stable persona first; consolidated memory appended into a later, still-stable section; never reorder the head during routine consolidation.

- **What this does not buy.** No discount is claimed. The measurable claim is narrower: fewer cacheable-but-uncached turns (T0-1) and lower `input_tokens` per turn at equal task success.

## 5. Tier 2 — belongs to M5 (agent loop)

M5 is where pie gains a loop it fully controls (`PLAN.md` §10, `ROADMAP.md` M5): sessions, resume, compaction. The study's § *Loop Guardrails: Breaking Cycles and Bounded Budgets* maps directly.

- **Iteration ceiling.** The study names 10–15; make it config (`agent.max_steps`), with the ceiling producing the best partial answer rather than a silent failure. Record it in the existing `stop_reason` field (`step_ceiling`) — that string is already in the frozen schema, so the ceiling becomes visible in `pie usage` at no schema cost.

- **Repeat-action detection.** Hash tool name + arguments; the study's sliding window is three turns. On a repeat, inject an explicit warning, then escalate. Fully meaningful only once pie's own tool loop exists (M9); in M5 it applies to re-delegation decisions.

- **Budget fences.** The study fences in dollars; pie has no per-token spend (`cost_usd_micros` is 0 for subscriptions). Fence on **tokens and wall clock** per task session using frozen fields, and additionally on `credits_consumed` / `credits_remaining` when a backend reports them. Record the fence as a `stop_reason` value.

- **The stall watchdog's single resume retry must count against the ceiling.** `PLAN.md` §9 permits one resume retry after a stall; unrecorded, that allows a stuck task to burn two full 600 s deadlines invisibly. Record it (`stop_reason = "stall_retry"`) and count it as an iteration.

- **Observation offloading.** In M5, delegate mode means the vendor owns the observations, so there is nothing for pie to offload — the study's pointer pattern has no target yet. It becomes real in M9 (pie's `bash`/`read` output → temp file, inject path + summary) and is *already* the design of the `ocr` MCP tool (`~/.pie/ocr-cache/`, `PLAN.md` §10: ~1–2 k vision tokens → ~200–500 text tokens), which is an observation-offloading device by another name.

- **Resume policy — the Tier-0 detector becomes an action.** M5 owns the fresh-vs-resume choice. Rule: resume when the gap is within the vendor's cache lifetime and the history is long; start fresh with a compacted seed when the gap predicts a cold prefix, because a cold resume re-sends the whole history at full prefill. This is the one place pie can move `input_tokens` a lot.

- **Escalation rule on a ceiling.** Ceiling, fence, or repeat → emit the partial answer, set `stop_reason`, record the turn. Never silently retry the same strategy; unbounded retry is precisely the study's first-generation failure mode.

- **Compaction** (§ *Memory Compaction*): M5 already commits to it. Prefer the study's semantic-bookmark shape (a compact structured state block) over narrative summarisation, and keep the compacted block in a stable position so it does not itself become a prefix-breaking variable.

## 6. Tier 3 — belongs to M9 / M10

**Vendor passthrough is the only wire-level lever pie has.** Everything in the study that needs a request field reaches pie, if at all, as a CLI flag:

| Vendor knob pie can forward | What it buys | Where |
|---|---|---|
| `--model` + `--effort` (agy, effort mandatory with model) / `--model` (codex) | Reasoning-budget control — the study's thinking-budget lever in discrete form | M1 / M2 |
| `--output-format stream-json` / `--json` (already used) | Tolerant machine-readable stream; no prose parsing overhead on pie's side | M0–M2 |
| `--print-timeout` / `--timeout` | Bounds worst-case spend by wall clock | M1 |
| `claude --json-schema` | The only structured-output *constraint* reachable at all (unverified) | M9 |
| `claude --autocompact` | Vendor-side compaction — delegates § *Memory Compaction* to the vendor (unverified) | M9 |
| MCP config (`--mcp-config`, `config.toml`, `mcp_config.json`) | Adds pie's `ocr` — and adds schema overhead to every vendor turn | M5 |

- **Tool-schema discipline.** pie's own loop exposes seven tools (`read`, `bash`, `edit`, `write`, `grep`, `find`, `ls`). At the study's 250–600 tokens per definition, that is ~1.8 k–4.2 k tokens of static overhead on every turn of the tool loop: write short, flat, single-line-parameter descriptions and refuse nested object schemas. This does not apply to M0–M2, where the vendor supplies its own inventory and pie sends none.

- **MCP overhead is a real cost pie would be adding.** The study's hidden-tax section says each advertised tool costs tokens on *every* request whose context it appears in. pie's `mcp_ocr` server therefore raises the vendor's `input_tokens` even on turns that never call `ocr`. Rules: expose exactly one tool, do not bundle a second, make enabling it opt-in per invocation, and measure the delta (§9, experiment 4) before making it always-on.

- **Code-as-action.** In pie's loop, prefer one `bash` invocation that reads, filters, and prints over four tool round-trips, so intermediate payloads never re-enter context (study: four turns / ~15 k tokens → one turn / ~600). This is a prompt- and tool-design decision, not a sandbox project; pie must not build the study's Docker/gVisor isolation, since `bash` already runs unsandboxed by design.

- **Dynamic tool retrieval is premature.** The study's target is 40–200 tools (10 k–20 k schema tokens, "90 % wasted on irrelevant schemas"). Seven tools do not justify a vector index, a BM25 index, or an embedding model. Revisit only if the M9 toolset grows past roughly 20.

- **M10 — the boundary moves.** The OpenRouter / DeepSeek path is the only place pie *builds* the request, so `max_tokens`, response format, serialization, prompt ordering, and provider-side cache flags become controllable there — part of §7 flips to "transfers". That is an argument for keeping those backends commented out only while API-key-mode risk outweighs their benefit (`RISKS.md` §10), not for treating them as permanently irrelevant.

## 7. What does NOT transfer

Unflinching list. Each item is a study claim pie cannot act on at all, with the reason.

1. **Cache breakpoints and cache discounts.** `cache_control: {"type":"ephemeral"}`, up to four breakpoints, the 90 % cached-input price, the 85 % TTFT reduction, OpenAI's automatic 128-token block matching, Gemini's 32 768-token minimum threshold — all request- or server-level (§ *Mechanics of Prefix Caching and KV-Cache Reuse*). pie is a client of a client.
2. **The study's headline "0 % token reduction, 50–90 % cost reduction, byte-identical output".** That is the prefix-caching row of § *Production Cost-Fidelity Trade-off Analysis*. pie cannot claim it, because it cannot change what the vendor caches or what it pays.
3. **KV-cache mechanics.** The `M_KV` formula, GQA's 4–8x footprint reduction, PagedAttention block tables, radix-tree longest-prefix matching, memory pooling (§ *Attention Mechanics and Context Memory Footprints*, § *Mechanics of Prefix Caching*). pie has no serving runtime and no GPU. This part of the study explains *why* vendors cache — nothing more.
4. **Tokenizer choice and the chars-per-token tables.** The vendor's CLI tokenizes the prompt; pie cannot choose BPE vs SentencePiece vs Unigram, and cannot change the wire encoding of anything it did not write. The tables are usable only as an *estimator* (§9, experiment 3).
5. **Constrained / grammar decoding.** Outlines, Guidance, logit-level CFGs (§ *Structured Output Minimization*) require the logits. pie is text in, text out; the nearest thing is passing a vendor's own schema flag through, which is a passthrough, not an optimisation.
6. **`max_tokens` and thinking budgets.** § *Classification of Token Streams* and the "thinking budget" material require the request. pie's only reachable approximation is `--effort` (agy `low…max`, discrete; codex reasoning levels). The study's specific reasoning-token figures are not reproducible by pie and must not be quoted as pie's expected gain.
7. **Algorithmic prompt compression (LLMLingua / LLMLingua-2).** Requires a local 500 M–1.5 B evaluator model scoring the prompt before inference, plus the ability to replace the payload (§ *Information-Theoretic and Distillation-Based Compression*). pie has neither, and in delegate mode most of the payload is not pie's text. Only the hand-written compression variant transfers.
8. **Semantic caching that returns a cached completion.** Cosine-similarity interception and "zero marginal token cost" (§ *Semantic Caching and Vector Retrieval Deduplication*). For pie this is not merely infeasible but wrong: it would mean answering without a model, fabricating a result for a task pie was asked to delegate. The study's own boundary criteria (temporal, personalised, non-deterministic queries) describe pie's workload. Disk caching stays limited to deterministic artifacts pie fetched itself.
9. **DSPy-style prompt compilation** (MIPROv2, GEPA, synthetic few-shot discovery). Requires a validation set, a metric-optimisation loop, and a stable model identity. pie's prompt is *projected* onto three vendors with three prompting contracts (`PLAN.md` §8), so a prompt compiled against one is not transferable to another — and the optimisation itself spends tokens to save tokens. Only the objective's shape (`accuracy − β·cost`) transfers, as logging.
10. **Stateful-graph runtime features.** LangGraph-style typed state channels, functional reducers, transactional HITL interrupts with persisted graph state (§ *Third and Modern Generation Architectures*, § *Loop Guardrails*). pie has no graph engine. The flip side: for M0–M2 pie gets the *benefit* the study credits to third-generation designs (isolated sub-agent context, only a summary returned) for free, by construction of delegate mode.
11. **The token-explosion math as pie's problem.** § *The Mathematical Reality of Token Explosion* derives the quadratic growth curve over *observations* — and in M0–M2 those observations live inside the vendor's CLI, where pie can neither prune, offload, nor even count them. pie sees a final answer and a usage total. The study's central warning is addressed to pie's vendors, not to pie; it becomes actionable for pie only in the M9 tool loop, which is why §5 and §6 exist.
12. **"Context density over context volume".** § *Strategic Syntheses and Future Trajectories* states the maxim, and it is right — but pie cannot see or reshape the vendor's context window, so there is no lever behind the aspiration beyond reducing what pie itself inserts.
13. **Cost accounting in dollars.** The study's entire optimisation arithmetic is per-million-token pricing with input/output/cached tiers. Every live pie backend is subscription-locked, so `cost_usd_micros` stays 0 and pie's unit of optimisation is tokens and wall clock, never money. Any "cost reduction" figure from the study is untranslatable.
14. **Provider-side knobs and research trajectories**: cache TTL configuration, minimum cacheable sizes, batching, concurrency, hardware KV memory tiering, learned end-to-end compression, dynamic test-time compute allocation (§ *Strategic Syntheses*). Vendor-controlled or research-stage; nothing for pie to decide.
15. **The study's token-reduction percentages as pie's targets.** They are measured on systems that control the payload. pie's realistic ceiling is "stop wasting what the vendor already gives you" — a smaller, separately measured number (§8), not "90 % off".

## 8. Measurement plan

Rule: prove each change with `~/.pie/usage.jsonl` and **only** fields already in the frozen row. Where a field is missing, say so rather than inventing a proxy.

| Change | Fields used | Claim to prove | Gap |
|---|---|---|---|
| T0-1 cacheable-but-uncached count | `input_tokens`, `cached_input_tokens`, `provider`, `ts` | The count falls after M3 lands | None |
| T0-2 / T0-3 argv-cap failures | `error_kind`, `ok`, `duration_ms` | `prompt_too_long == 0`; wasted `duration_ms` falls | None |
| T0-4 degenerate resume | `ts`, `cached_input_tokens`, `input_tokens`, `provider` | Rows matching "cold gap + zero cache + large input" trend to zero | **Resume provenance is not recorded**; attribution stays heuristic (below) |
| T0-5 prefill-vs-decode classification | `input_tokens`, `output_tokens`, `reasoning_tokens`, `duration_ms` | Slow turns move from the prefill class to the decode class after M5 | None |
| T0-6 wasted wall clock | `duration_ms`, `ok`, `error_kind` | Failed-turn `duration_ms` share falls | None |
| T0-7 vendor cross-check | `input_tokens`, `provider`, `ts` vs. the backend's own bucket report | Divergence stays within noise (a protocol-drift alarm) | None, but the bucket source is fetched, not logged |
| T1 profile diet (M3) | `input_tokens` per turn, same task, same backend | `input_tokens` per turn drops at equal `ok` | **Fidelity is not measurable** from the log (below) |
| T1 ordering (M3) | `cached_input_tokens` / `input_tokens` via the existing `cache_hit_rate()` | Cache-hit rate rises on repeated-task runs | Assumes the vendor caches across *separate* CLI invocations — unproven (§9, experiment 1) |
| T2 ceiling / fence / stall retry (M5) | `input_tokens`, `output_tokens`, `duration_ms`, `stop_reason`, `ok` | Resumed long tasks cost less than fresh ones at equal success; ceilings and retries appear as `stop_reason` values | None (deliberately using `stop_reason`, an existing free-form `String`) |
| T3 MCP `ocr` cost delta (M9) | `input_tokens` medians, same prompt, MCP on/off | Whether exposing `ocr` raises the per-turn floor | Small N; needs the controlled A/B in §9 |
| T3 tool-schema diet (M9) | `input_tokens` before/after the description rewrite | Static overhead falls by roughly the schema delta | None |

**Cannot be measured with existing fields.** *Task fidelity.* Every table in the study has a fidelity column; pie's log has `ok` and `stop_reason` only. A cheaper turn that answers worse looks identical, so any M3 claim of "compression without quality loss" is unprovable from the log and needs a separate human- or model-judged eval. Stated plainly so nobody reads a falling `input_tokens` as a win by itself. *Attribution of a turn's `input_tokens` to pie's own prompt.* `input_tokens` includes the vendor's own system prompt, its tool definitions, and any replayed history; pie cannot tell how much of it, if any, came from the text pie composed.

**Smallest schema additions — these require an ADR-style decision**, because `DESIGN.md` §7 is explicitly frozen and cross-readable with the pre-existing JS pi. These are proposals, not edits:

1. `resume_of: Option<String>` (or a boolean `resumed`) — makes T0-4 attributable instead of heuristic, and is the field that lets M5 *prove* "resume beat fresh start" rather than assert it.
2. `prompt_chars: u64` — the size of the text pie composed (the argv it actually passed, minus vendor-side prefixes). Without it the argv cap (chars) and the token budget (tokens) cannot be related, and T0-3 stays heuristic.

Both are additive, must carry `#[serde(default)]` so older rows still parse, and must respect the append-only / never-rewrite rule. Until an ADR lands, encode loop outcomes in the existing free-form `stop_reason` (as T2 does) and keep T0-4 a report, not a policy.

## 9. Open questions — experiments, not decisions

1. **Does any of these CLIs reuse a prefix cache across separate `pie -p` invocations at all?** The whole Tier-1 argument rests on it, and pie cannot see the vendor's prompt assembly — each CLI prepends its own system prompt and tool definitions, which may vary per run and may sit *before* pie's text. Experiment: identical prompt three times (30 s, 10 min, 1 h apart), compare `cached_input_tokens`. Decides whether Tier 1 is a cost play or a latency play.

   **Partially answered 2026-10-12** (see `knowledge/agy.md` "Resume, and cache-token accounting"): `cached_input_tokens` is non-zero on agy and codex, so caching happens. But the *severity* of the question changed — `cache_read_tokens` for agy accumulates across its internal tool-loop steps and can exceed `input_tokens` (104 041 against 13 556 on one measured turn), while codex uses OpenAI subset semantics. **The field means two different things**, so the remaining experiment is per-backend: define what `cached_input_tokens` denotes for each CLI before any hit-rate target is set against it. Also measured: resume preserves context but did **not** reduce agy latency (41.2 s resumed versus 40.5 s fresh), so resume is a continuity feature, not the speed fix.
2. **What is the real cache lifetime for agy and codex?** The study gives 5 min (Anthropic ephemeral), 5–10 min (OpenAI), hourly (Gemini), but pie's backends are CLIs on subscription plans. Experiment: sweep the resume gap to find where `cached_input_tokens` collapses to zero. This constant feeds T0-4 and M5's fresh-vs-resume rule.
3. **What is the actual chars-per-token ratio for pie's composed prompts?** The study's table is modality-level, not backend-level. Experiment: for N turns, record composed prompt chars locally against reported `input_tokens` and fit a per-backend constant. Without it the argv cap and any token budget are not conversable, and T0-3's threshold is arbitrary.
4. **Does exposing the `ocr` MCP server raise the per-turn token floor for turns that never call it?** The study's hidden-tax argument says yes. Experiment: A/B the same prompt with and without the MCP config, compare `input_tokens` medians. Decides default-off vs always-on in M5/M9.
5. **What is the minimum cacheable prefix per backend?** The study lists 1 024 tokens (Anthropic/OpenAI) and 64 (DeepSeek); T0-1 assumes 1 024. Experiment: sweep prompt sizes and find where `cached_input_tokens` first becomes non-zero.
6. **Is a dieted profile still a correct profile?** Needs a judged eval, not the log. Experiment: a fixed task set run against pre-diet and post-diet projections, success scored independently, `input_tokens` compared. The only experiment that can defend an M3 change as an improvement rather than a regression.
7. **Does `--effort` behave monotonically in cost for the same task?** The study's reasoning-budget material implies diminishing returns; pie's effort axis is discrete. Experiment: one task × effort levels, comparing `reasoning_tokens`, `output_tokens`, `duration_ms`, and success. Produces the default-effort policy the M3 router needs.
8. **Does a vendor-side compaction flag remove the need for pie's own M5 compaction?** `claude --autocompact` (unverified) would delegate § *Memory Compaction* to the vendor. Experiment: a long session with and without the flag, comparing the `input_tokens` trajectory and fidelity. If the vendor's compaction is good enough, M5 should not build a second one.
9. **Does a mid-session backend switch cost more than it saves?** M3 re-bootstraps from pie's transcript, so switching backends re-sends history into a cold prefix on the new backend. Experiment: the same task on agy throughout vs. switched mid-way, comparing total `input_tokens`. Decides the M3 switch policy.

## 10. Cross-check against the 9router prior-art sweep

A second, independent source was reviewed on the same day:
`knowledge/9router-findings.md` (a read-only sweep of `decolua/9router`, a
credential broker and reverse proxy for the same three vendors). It was not read
for the study's material and the study was not read for it, so where the two
agree, that is genuine convergence rather than restatement.

### 10a. Convergent conclusions — two sources, one lever

The largest saving is the same in both: **stop re-sending history.**

- The study reaches it from caching mechanics: a byte-stable leading prefix is
  what makes prefix caching hit at all, and any variation in the leading tokens
  invalidates the whole cached block (section 2, and section 4's Tier-1 rules).
- 9router reaches it from measured delivery: its session manager derives a stable
  id from the caller's session header, falling back to a hash of accumulated
  assistant text, specifically so the vendor's automatic prefix cache keeps
  hitting without client cooperation — reporting a 20-40 % input-token band.

That raises confidence in T0-4 and the M5 resume policy from "one source argues"
to "two independent implementations agree". It does **not** promote the 20-40 %
figure to a pie target: those are 9router's own README claims, on a different
transport, and pie must measure its own number (section 8).

### 10b. What the study does not cover, and 9router does

Four techniques are absent from the study and present in 9router. All four
survive pie's constraints; the numeric caps are tabulated in
`knowledge/9router-findings.md` section 2 rather than duplicated here.

| Technique | Where it lands | Why it matters |
|---|---|---|
| **Boundary truncation that fails open and never grows.** Only replace content when the result is strictly shorter; skip tiny inputs; never compress error output; reject a shrink below a threshold as "phantom savings" | M3/M5 | The study's compression section is about a local evaluator LM; this is the hand-written subset that actually applies to text pie pastes in, with a hard safety rule the study does not state |
| **Never retry a 429.** 9router's config is literally `429: {attempts: 0}` while 502/503/504 get 2-3 attempts | M5 retry policy | Distinguishes "you are over quota, re-sending burns more" from "upstream hiccup, re-sending helps". The study has no vocabulary for this distinction |
| **Cooldown per model, not per account.** A persisted `modelLock_<model>` expiry, keyed on a status-code *and* message rule table | M5 retry policy | Capacity is a property of one model. Cooling the whole backend because one model is saturated throws away the others |
| **Answer trivial turns locally.** Recognise warmup, title-generation and naming requests and answer them with no provider call at all | M5 | A whole turn of quota saved per probe, by pure client-side matching. The study's bypass equivalent is semantic caching, which section 7 rejects for pie as *wrong* — this is the benign version: never ask |

### 10c. A correction to the study's framing

The study measures everything in money. **pie has no money to measure.** Every
backend is subscription-locked, so `cost_usd_micros` is permanently `0` in the
frozen row, and the study's headline "0 % tokens, 50-90 % cost, byte-identical
output" is doubly unclaimable: pie controls neither the payload nor the invoice.

pie's optimisation targets are therefore **tokens, wall clock, and quota
headroom** — which is what sections 3-5 already assume, but worth stating
plainly so nobody imports a dollar-denominated target. A 30 % token reduction on
a subscription plan buys back quota, not cash.

### 10d. The compliance boundary — read this before borrowing anything

The techniques in 10b are separable from the way 9router is built, and only they
may be taken. 9router's own authors flag every subscription provider
`deprecated: true, deprecationNotice: "RISK_NOTICE"`, because the cheapness is
bundled with holding vendor OAuth tokens in its own database, MITM-proxying
vendor traffic, renaming tools to impersonate the official client, and pinging
vendors to warm rate-limit windows. `PLAN.md` sections 5 and 12 forbid all of it.

This is also why the codex quota probe now sits behind `codex.quota_probe =
"http" | "off"` (`DESIGN.md` section 9): 9router is the demonstration of what that
endpoint costs in posture, and the switch is what makes it a decision rather than
a default.
