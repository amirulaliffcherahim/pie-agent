# Reverse-engineered CLI knowledge

Reference notes for the three proprietary-subscription CLIs that `pie` drives
headlessly. Each file records the binary, auth, print-mode invocation, output
schema, model listing, and quota access — captured from `--help` plus live
print-mode runs. These are the implementation inputs for roadmap M1–M3. Deep internal structure
(auth, config, sessions, model catalogs, wire targets, tool protocols) is in
[internals.md](internals.md).

| CLI | Version | Auth | Print mode | Verified? |
|---|---|---|---|---|
| [agy](agy.md) | 1.3.3 | agy OAuth (internal) | `agy --print … --output-format stream-json` | ✅ live run |
| [codex](codex.md) | 0.162.0 | `~/.codex/auth.json` | `codex exec … --json` | ✅ live run |
| [claude](claude.md) | 2.1.296 | `~/.claude/.credentials.json` (not logged in) | `claude -p … --output-format stream-json` | ⚠️ flags only |

Common discipline across all three: **email-locked subscriptions only — use each
CLI's own login/OAuth, never an API key.** Spawn hidden (`CREATE_NO_WINDOW`),
bound the prompt argv (≤ ~20k chars, else stdin where supported), parse
NDJSON/JSONL line-by-line, enforce a timeout + kill-tree, and never store/echo
tokens.

## Other reference material

| File | What it is |
|---|---|
| [internals.md](internals.md) | packaging, auth stores, config paths, session storage, model catalogs, wire targets, tool protocols — sourced from on-disk state and binary strings |
| [token_optimization_and_agent_engineering.md](token_optimization_and_agent_engineering.md) | external study on token economics, context optimisation and agent-loop overhead. Distilled for pie in `planning/TOKEN-ECONOMICS.md` |
| [9router-findings.md](9router-findings.md) | prior-art sweep of `decolua/9router`: the compliance anti-patterns to avoid, and the token-economy techniques that do transfer |

Both non-CLI files are reference, not contract. `planning/DESIGN.md` remains
authoritative.
