# pie — user guide

Practical, task-first. For *why* any of it works this way, the authority is
`planning/DESIGN.md` (the mechanism), `planning/ACCEPTANCE.md` (the criteria, the gate
run log, and the bug log) and `planning/HANDOFF.md` (current state). This file is about
how to *use* the thing.

Everything below assumes PowerShell on this machine and the build in this repository:

```powershell
cd C:\Users\amiru\Local\Code\pie
cargo build --release
```

---

## 0. Read this first: there are two different `pie`s on this machine

This is the single most confusing thing about using `pie` here, and it is what stops
`pie tui` from working.

| | This project | The other one |
|---|---|---|
| Source | `C:\Users\amiru\Local\Code\pie` | `C:\Users\amiru\pie` |
| Crate | `pie` (single crate) | `pie-cli` (10-crate workspace) |
| On `PATH`? | **no** | **yes** — `~\.cargo\bin\pie.exe` |
| Installed | never | Oct 10, by `cargo install --path ~/pie/crates/pie-cli` |
| Has `tui`? | **yes** | **no** |
| Has `chat`, `login`? | no | yes |
| Version-controlled? | yes (git) | **no** — not a git repo |

So **a bare `pie` in PowerShell runs the other project.** That is why there is no TUI:
that binary has no `tui` subcommand at all.

### Tell them apart in one command

```powershell
pie tui --help
```

```
error: unrecognized subcommand 'tui'     ← you are on the OTHER pie
```

```powershell
pie --help
```

The other one lists `chat`, `models`, `doctor`, `auth`, `agy`, `usage`, `login`.
This one lists `models`, `auth`, `usage`, `doctor`, `profile`, `sessions`, `tui`,
`learn`, `memory`, `schedule`, `watch`, `ocr`, `mcp`.

### Run this project's pie

Always works, no `PATH` changes:

```powershell
C:\Users\amiru\Local\Code\pie\target\release\pie.exe tui
C:\Users\amiru\Local\Code\pie\target\release\pie.exe --list-backends
```

Or put a shortcut in your PowerShell profile (`$PROFILE`):

```powershell
function pie2 { & 'C:\Users\amiru\Local\Code\pie\target\release\pie.exe' @args }
```

### Two cautions about making it the real `pie`

**`cargo install --path .` will overwrite the other project's binary.** Both install a
binary named `pie.exe`, so a plain install clobbers it. If you want this one on `PATH`
without that, use a separate root and put it *before* `~\.cargo\bin`:

```powershell
cargo install --path . --root .\.install
# then prepend C:\Users\amiru\Local\Code\pie\.install\bin to PATH
```

**Both projects share `~\.pie` as their home directory, and they disagree about it.**

- The other project keeps OAuth tokens in `~\.pie\auth.json` (`crates/pie-auth`).
  **This project must never read, write or recreate that file** — invariant I11, with
  tests (`tests/no_secrets.rs`). If it is present it is left byte-identical.
- `~\.pie\usage.jsonl` is shared. The row schema is the frozen one this project also
  writes, so `pie usage` totals in *either* tool can include rows the other one wrote.
- Both read `~\.pie\config.toml`, with **overlapping but incompatible** table layouts.
  The other project uses `[deepseek]`, `[codex] base_url/originator`, `[agy] binary/models`;
  this one uses `[general]`, `[agy] bin/...`, `[codex] bin/quota_probe`, `[jobs]`, `[tools]`.
  There is no `config.toml` right now, which is why nothing has broken yet. **Write this
  project's config to a copy and point `PIE_HOME` at it if you want to be safe:**

  ```powershell
  $env:PIE_HOME = "C:\Users\amiru\.pie-local"
  ```

---

## 1. Bare `pie` prints help, on purpose

Even with the right binary, `pie` on its own prints help. The TUI is opt-in:

```powershell
pie tui
```

This is deliberate (`DESIGN.md` §12): a bare `pie` must not take over a terminal, so no
script and no CI job can enter the TUI by accident. `pie tui` also **refuses to start
unless stdin and stdout are both terminals**, so piping it fails with a message rather
than spraying escape codes into the pipe.

---

## 2. First checks — all of these spend zero model quota

```powershell
pie --list-backends     # the five backends, their binaries, versions and auth state
pie auth                # just the auth column
pie doctor              # the above plus versions, local paths, and the env scrub in effect
pie models              # every model pie knows, per backend
pie models --backend agy --refresh   # ignore the cache and ask the CLI
pie usage               # pie's own session-log totals
pie usage agy           # one backend's quota buckets
pie profile             # the persona projection, and where each part came from
pie sessions            # recorded sessions, newest first
```

Exit codes, if you script around this: **2** config/usage, **3** auth, **4** protocol,
**5** timeout, **6** binary missing, **1** any other backend failure.

---

## 3. One-shot turns (delegate mode)

```powershell
pie -p --model agy/gemini-3.8-flash "summarise this repo"
pie -p --model codex/gpt-6-luna --json "list the TODOs"
pie -p "what does this project do?"      # uses default_backend
```

**Delegate mode** is the only mode `agy` and `codex` support: they are one-shot, run
their *own* tools, and cannot accept a tool result injected by pie (`RISKS.md` §1).

- Human answer → **stdout** (so it pipes). Progress → **stderr**. `--json` → one
  normalised event per line.
- `--effort <EFFORT>` — reasoning effort, backend-specific (`agy` requires it alongside
  `--model`).
- `--timeout <SECONDS>` — whole-turn deadline. A turn that outlives it is killed, not
  waited on.
- `--cwd <DIR>` — working directory handed to the vendor CLI.

Models on this machine, as recorded:

| Backend | State | Notes |
|---|---|---|
| `agy` | ready | `agy/gemini-3.8-flash` is the approved live-test model |
| `codex` | ready | `codex/gpt-6-luna` is the approved live-test model |
| `claude` | **no-subscription** | no credential here, so no live claude turn is possible |
| `openrouter` | needs-login | set `OPENROUTER_API_KEY` |
| `deepseek` | needs-login | set `DEEPSEEK_API_KEY` |

Piping is safe: `pie -p "hello" | head -1` exits **0** (a closed pipe no longer panics —
that was S2, fixed).

---

## 4. The TUI

```powershell
pie tui
```

Same persona, same routing and same session recording as `pie -p`, so a turn taken here
costs the same and is learned from the same way.

Keys (this table is the source of truth — the help footer is generated from it, and a
test asserts they agree):

| Key | Prompt ready | Picker open |
|---|---|---|
| char | type at the cursor | — |
| backspace | delete a character | — |
| left / right | cursor left / right | — |
| enter | send prompt | take model |
| tab | open picker | no-op |
| up / down | scroll transcript | previous / next model |
| esc | cancel turn; quit when nothing to cancel | cancel turn, close picker |
| ctrl-c | quit always | quit always |
| ctrl-x | ignored (documented no-op) | ignored |

Slash commands typed into the prompt are handled locally and start no turn:
`/memory` shows what pie learned.

---

## 5. Sessions and resume

Every turn prints `[session] <id>`. Resume is **opt-in and never automatic** — a plain
`pie -p` starts a fresh conversation, because reusing a conversation the user did not ask
for can answer in the wrong context.

```powershell
pie -p "start something"                    # prints [session] <id>
pie -p --continue "keep going"              # newest session for this backend + directory
pie -p --session <id> "back to that one"
pie -p --resume <vendor-id> "..."           # a vendor's own conversation id
pie -p --switch codex --continue "..."      # same session, different backend
pie sessions                                # list them
```

`--switch` re-sends pie's **own** transcript to the new backend, compacted once it is
long enough. A vendor conversation id belongs to the backend that minted it, so it is
never handed to a different one.

Combining `--continue` with `--session`, or `--switch` with `--resume`, is a
configuration error naming both.

---

## 6. Persona, memory and learning

The persona is one projection assembled from three places, in this order:

```
~/.pie/profile.toml      ·  ~/.pie/AGENTS.md  ·  ~/.pie/memory/*.md
```

`pie profile` shows the assembled text and each part's contribution. agy and codex
receive it folded into the prompt; claude and the HTTP backends get it as a system
prompt. The projection is byte-identical across backends for the same inputs, which is
a test, not a hope.

```powershell
pie learn --report          # extract from your own history; rule-based, spends nothing
pie learn --dry-run         # decide and report, write nothing
pie memory                  # what it learned, with provenance
pie memory revert <id>      # undo one learning pass, restoring the previous bytes
```

Reading the vendor CLIs' own history is a **separate opt-in** (`read_vendor_history`),
off by default. Asking a model to review candidates needs both `--model` and a non-zero
token budget; without a budget it is refused before anything spawns.

Background jobs are opt-in twice over — default cadence `never`, default budget `0`:

```powershell
pie schedule              # what is due and what budget is left; spends nothing
pie schedule run learn    # run it now, budget permitting
```

---

## 7. The watchtower

```powershell
pie watch                 # probe the CLIs, fetch the sources, write the monthly report
pie watch drift           # probe + snapshot only; writes no report
pie watch news            # print the digest only; writes no report
pie watch --dry-run       # decide and report; spawns nothing, fetches nothing, writes nothing
pie watch --month 2026-11
```

Probing a CLI for its version and `--help` **costs no model tokens** — that is why the
drift half is safe to schedule. The news half fetches nothing at all unless you configure
sources (`[jobs] news_sources`), and it ranks headlines against `[jobs] watch_stack`.

Reports land under `$PIE_HOME/reports/watchtower-YYYY-MM.md`. The report states its own
comparison mode at the top: it is **REA-lite** — version + `--help` + selected binary
strings, no structural comparison, because no disassembler is installed.

---

## 8. OCR and MCP

```powershell
pie ocr scan.png                  # convert through itsmini
pie ocr scan.pdf --lang en --refresh
pie --json ocr scan.png           # --json is global: either side of the subcommand
pie mcp ocr                       # host it as an MCP server on stdio
```

`itsmini` is configured by `PIE_ITSMINI_URL` alone (default `http://127.0.0.1:8000`,
120 s timeout) — the one setting that is env-only today. The answer is cached by content
hash under `$PIE_HOME/ocr-cache/`, keyed on the parameters that change it, so the same
image converts once.

`pie mcp ocr` advertises **exactly one** tool on purpose: an advertised tool costs the
vendor input tokens on *every* turn of the session, including turns that never call it.
In delegate mode, pointing `agy`/`codex` at this server through their own MCP config is
the only route that serves all three backends uniformly.

---

## 9. Tools — the model running things (M11)

**Off by default, and the loop's only caller is an API-key backend**, so read this before
expecting them to work.

```toml
# ~/.pie/config.toml
[tools]
workspace  = "C:/Users/amiru/Local/Code/pie"   # the ceiling for every tool; absent = NO tools
read       = true                              # read, ls, grep, find  (default true)
bash       = false                             # opt-in, and the decision
bash_allow = ["cargo", "git", "rg"]            # argv[0] allow-list, checked BEFORE the spawn
timeout_secs     = 30
max_output_bytes = 1048576
```

Rules that matter:

- **A tool is enabled in config or it is not offered.** The loop has no user in it, so
  there is no prompt to approve a command mid-turn — no per-call confirmation, and no
  timeout-then-proceed.
- **The workspace root is a ceiling, not a default**, checked on the **resolved** path, so
  `..` and a symlink are both refused.
- **`bash` runs no shell.** The line is tokenised and `argv[0]` is exec'd directly, so
  no pipes, no redirection, no `&&`, and a `;` is a literal argument. If it ran a shell,
  `bash_allow` would be checking *the shell* and would admit anything.
- **`write` and `edit` are declared and NOT built** — every call is refused with a message
  saying so (S18). The keys exist so a config does not fail to parse, not because they work.
- A disabled or unknown tool is **answered**, never ignored: the model gets a sentence
  naming the config key that would enable it, so it can proceed instead of waiting.

**The availability caveat.** `ToolSet` is wired into the tool loop, and the loop's only
caller is `provider/api.rs` — the OpenRouter/DeepSeek backends. Claude's `tool_loop` is
`false` and agy/codex are one-shot delegate mode, so **a live tool-calling turn needs
`OPENROUTER_API_KEY` or `DEEPSEEK_API_KEY`**. Without one, the tools are exercised by the
test suite only. That is what M11's `offline-verified` verdict records.

---

## 10. Configuration reference

`~/.pie/config.toml`. Precedence, highest first: **CLI flag → `PIE_*` env → file →
built-in default**.

| Table | Key | Default | Notes |
|---|---|---|---|
| `general` | `default_model` | — | `provider/model`; `PIE_DEFAULT_MODEL` |
| | `default_backend` | `codex` | empty string turns routing off; `PIE_DEFAULT_BACKEND` |
| | `timeout_secs` | `600` | `PIE_TIMEOUT_SECS` |
| | `prompt_argv_cap` | `20000` | `PIE_PROMPT_ARGV_CAP` |
| | `color` | `auto` | `auto`\|`always`\|`never`; `PIE_COLOR` |
| | `extra_scrub_env` | `[]` | extra env vars removed from every child |
| `agy` | `bin` | `agy` | `PIE_AGY_BIN` |
| | `default_effort` | — | injected when a model is chosen without `--effort` |
| | `print_timeout_secs` | — | passed as `agy --print-timeout` |
| | `model_cache_ttl_secs` / `usage_cache_ttl_secs` | — | cache lifetimes |
| `codex` | `bin` | `codex` | `PIE_CODEX_BIN` |
| | `use_node_shim` | — | codex is `node codex.js` here |
| | `quota_probe` | `off` | `http` opts into the private quota endpoint |
| `claude` | `bin` / `enabled` | `claude` / `false` | `PIE_CLAUDE_BIN` |
| `jobs` | `learn_cadence` | `never` | `never`\|`daily`\|`monthly`; `PIE_JOBS_LEARN_CADENCE` |
| | `learn_budget_tokens` | `0` | per **window**, not per run |
| | `read_vendor_history` | `false` | read `~/.gemini`, `~/.codex`, `~/.claude` |
| | `news_sources` | `[]` | bare URLs; empty = fetch nothing |
| | `watch_stack` | `[]` | terms the digest ranks against |
| `tools` | see §9 | all off | `workspace` absent = no tools |

Environment-only: `PIE_HOME` (default `~/.pie`), `PIE_STRIP_ENV=0` (disable the API-key
scrub — it warns on stderr, loudly, because it is a debugging switch for a *vendor* CLI).

---

## 11. Troubleshooting

| Symptom | Cause and fix |
|---|---|
| `pie tui` → `unrecognized subcommand 'tui'` | You are running `~/pie`'s binary, not this repo's. Use the full path — §0. |
| Bare `pie` prints help, no TUI | By design. The TUI is `pie tui`. |
| `pie tui` → `needs a terminal on stdin and on stdout, but neither stdin nor stdout is a terminal` | You piped it, or it is running under a captured stdout. Use a real terminal, or use `pie -p <PROMPT>` for a single turn. |
| `claude` shows `no-subscription` | True: there is no claude credential on this machine. No live claude turn is possible. |
| `pie -p` says the turn path is not implemented | The backend cannot accept an injected tool result, or has no credential — see the row above. |
| Totals in `pie usage` look bigger than your turns | `~/.pie/usage.jsonl` is shared with the other `pie`. See §0. |
| A `/usage` probe from git-bash returns nonsense | Set `MSYS_NO_PATHCONV=1`; git-bash rewrites a leading-slash argument into a Windows path (bug B10). |
| `PIE_STRIP_ENV is set` warning | Something in your environment set it; API-key vars are **not** being stripped from children. Unset it. |
| A tool call is refused | The message names the config key. `tools.workspace` absent means no tools at all. |

---

## 12. Where the truth lives

| File | What it is |
|---|---|
| `planning/DESIGN.md` | the implementation contract — mechanisms and why |
| `planning/ACCEPTANCE.md` | frozen criteria, §12 the gate run log, §13 the bug log (B1–B41), §14 open items (S1–S22) |
| `planning/HANDOFF.md` | current state, the working discipline, and the traps that already cost time |
| `planning/ROADMAP.md` | what each milestone was for |
| `knowledge/*.md` | reverse-engineered notes on the vendor CLIs |
| `README.md` | the short version, plus the safety posture |

Run the gates before trusting any number in any of them:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/qc.ps1          # offline
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/qc.ps1 -Live    # + G5, spends quota
```
