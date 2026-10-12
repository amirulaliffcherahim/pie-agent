# pie — Platform support and verification

> **REFERENCE MATERIAL.** This file is a convenience summary, not a contract.
> `planning/DESIGN.md` is authoritative: if the two ever disagree, DESIGN.md
> wins and this file is wrong. `planning/ACCEPTANCE.md` owns the gates.

## 1. Target platforms

| Platform | Status | What "supported" means |
|---|---|---|
| **Windows 10/11 x86_64 (`x86_64-pc-windows-msvc`)** | **Primary.** Developed and tested here. | Full feature parity, G1–G5 green locally, all vendor CLIs (agy, codex, claude) spawnable. Bugs found here are fixed here. |
| **Linux x86_64 (glibc)** | **Required.** Cannot be booted locally (see §3). | Builds, tests, and `--list-backends` pass; vendor CLIs resolved from `PATH` by their unix launchers. Not expected to be nicer than Windows — just not broken. |
| **WSL2 (distro `FedoraLinux-44`)** | **Required, same code path as Linux.** | Treated as Linux. WSL is the developer's intended local Linux, but it cannot boot on this machine (`HCS_E_SERVICE_NOT_AVAILABLE`), so CI is the real check. |
| macOS (arm64) | **Not a support target.** | Not promised to work, but must not be *actively* broken. `macos-latest` stays in the CI matrix as a regression tripwire: an accidental "unix means Linux" assumption usually breaks Linux too. `macos-latest` is arm64; x86_64 macOS is deliberately untested. |

No platform-specific packaging, installers, or release artifacts are in scope
for M0–M2.

## 2. Platform-specific behaviours and where they are handled

| Behaviour | Windows | Linux / WSL / macOS | Handled in |
|---|---|---|---|
| Hidden child console | `CREATE_NO_WINDOW` = `0x0800_0000` creation flag; no console flash, ever | No-op — the flag does not exist; spawn normally | `src/process.rs` (invariant I1; unit test asserts the constant is applied) |
| Process-tree kill | `taskkill /PID <pid> /T /F` — kills the shim's whole tree | Process-group kill (own group + signal the group), since the tree otherwise outlives the parent | `src/process.rs` (DESIGN §6.5); `kill_on_drop(true)` on both |
| Binary resolution | Search `PATH` + `PATHEXT` (`.exe`, `.cmd`, `.bat`); a bare `codex` may resolve to a shim that cannot be spawned directly | Search `PATH` only; unix launcher scripts are executable and spawn directly | `src/process.rs::resolve_binary`, consumed by each `Provider::binary(cfg)` |
| Home directory | No `HOME` by default → fall back to `USERPROFILE` | `HOME` | `src/paths.rs` — resolve `HOME`, then `USERPROFILE` |
| State directory | `%USERPROFILE%\.pie` unless overridden | `$HOME/.pie` unless overridden | `src/paths.rs`; `PIE_HOME` overrides the root on **both** platforms (DESIGN §7) |
| argv length | `CreateProcess` limit ~32 767 chars | `execve` limit is much larger (per-arg `MAX_ARG_STRLEN` ~128 KiB) | pie bounds the *portable* case, not the Windows one: `Capabilities::max_prompt_argv = 20_000` for agy → `PieError::PromptTooLong`, exit 2 (I4). The 20 000 cap is deliberately OS-independent so a prompt cannot pass on Linux and fail on Windows. |
| codex `.cmd` shim | **`codex.cmd` cannot be spawned directly without a shell.** Resolve `node <…>/@openai/codex/bin/codex.js`, or the native `codex.exe` | `codex` on `PATH` spawns directly | `src/provider/codex.rs` (DESIGN §9, INVARIANT: never spawns `codex.cmd`); resolve once and cache in memory |
| Path separators in argv | `--add-dir <CWD>` receives a Windows path | unix path | `PathBuf` end-to-end; never string-concatenate paths |
| Env scrubbing | API-key deny-list removed from the child env; `GOOGLE_APPLICATION_CREDENTIALS` preserved (agy's own auth path) | identical - no OS branch | `src/process.rs` / `src/config.rs` (I2) |
| Gate-runner scripts | `scripts/qc.ps1` must be **pure ASCII**: Windows PowerShell 5.1 reads a `.ps1` without a UTF-8 BOM as Windows-1252, so one non-ASCII character inside a string is mojibake and the script fails to *parse* | `scripts/qc.sh` is UTF-8 and runs under `bash`; no constraint | CI step "Gate runner self-check"; documented in the `qc.ps1` header. This was bug B8 - it broke the Windows runner silently |
| Manual CLI probes | Any probe passing a leading-slash argument (`agy --print /usage`) **must** set `MSYS_NO_PATHCONV=1`, or git-bash rewrites `/usage` into `C:/Program Files/Git/usage` and the CLI receives it as prompt text | not applicable - the trap is Windows-only | `knowledge/agy.md` "Probing slash commands from git-bash". This was bug B10: three probes produced misleading results and nearly dropped a working, free feature. **pie's own spawn path is unaffected** - `std::process::Command` does no rewriting, so pie's argv was always correct; do not "fix" pie for a shell artifact |

## 3. How each platform is verified

| Channel | Command | What it proves | What it cannot prove |
|---|---|---|---|
| Local Windows (primary) | `cargo fmt --check` · `cargo clippy --all-targets -- -D warnings` · `cargo test` · `cargo run -- --list-backends` — or `scripts/qc.ps1` | G1–G4 on the real target, including `cfg(windows)` paths: hidden spawn, `taskkill`, `PATHEXT`, `USERPROFILE` | Nothing about unix |
| Local Windows, live | `scripts/qc.ps1 -Live` (**spends real subscription quota**) | G5: real agy/codex turns, usage probes, secret redaction | Linux |
| Local cross-target | `cargo check --target x86_64-unknown-linux-gnu --all-targets --no-default-features` (after `rustup target add x86_64-unknown-linux-gnu`) | `cfg`-gated compile errors in **pie's own** code — catches a `#[cfg(unix)]` block that does not type-check besides a Windows-only one. Type-checking links nothing. | Linking, runtime behaviour, anything needing a Linux kernel |
| Local cross-target, full features | `cargo check --target x86_64-unknown-linux-gnu` (no `--no-default-features`) | Nothing extra on this machine | **Unavailable here.** The `http` feature pulls `rustls` → `ring`, which compiles C for the target and needs a `x86_64-linux-gnu-gcc` cross toolchain that does not exist on a Windows dev box. The failure is **inconclusive, not a pie bug** — that is exactly why the `http` feature exists and why G6 passes `--no-default-features`. Full-feature Linux compilation is proven by the ubuntu CI job. |
| WSL2 | `bash scripts/qc.sh` | Everything Linux does, locally | **Currently impossible**: WSL cannot boot on this machine (`HCS_E_SERVICE_NOT_AVAILABLE`). Not a verified channel today. |
| Linux | `.github/workflows/ci.yml`, job `gates (ubuntu-latest)` | G1–G4 on real Linux with a real linker — the authoritative Linux check | G5 (a subscription-account operation, never run in CI) |
| macOS | `.github/workflows/ci.yml`, job `gates (macos-latest)` | The unix code paths build and pass tests on a second unix kernel | Any macOS-specific behaviour, and x86_64 macOS |

CI is the **only** real Linux verification channel for this project
(`.github/workflows/ci.yml` header). A red `ubuntu-latest` job is the answer to
"does this work on Linux?" — not a local claim.

## 4. What is NOT verified

- **Linux at runtime, locally.** Zero local Linux executions happen on this
  machine. Every Linux statement is an inference from CI or from reading
  `cfg`-gated code.
- **WSL specifically.** WSL is not a distinct code path from Linux (same
  `(unix)` `cfg`), so it is not separately verified at all.
- **Live (G5) on any non-Windows platform.** Live smoke needs a logged-in
  vendor CLI and a paid subscription; CI is quota-free by design.
- **macOS beyond "tests pass".** No packaging, no code signing, no arm64 vs
  x86_64 distinction, no GUI/terminal behaviour.
- **Anything relying on a vendor CLI that is absent.** `--list-backends` must
  report `binary-missing` rather than fail, but CI runners have none of the
  three CLIs installed — so CI proves the *degraded* path, not the happy path.
- **Cross-compilation of the binary.** `cargo check --target` type-checks only;
  no Linux binary is produced or executed locally.

## 5. Rules for platform-specific code

1. **Prefer one portable path with no `cfg` at all** (env scrubbing, argv
   building, the prompt cap). Fewer branches means fewer platforms to verify.
2. When a branch is unavoidable, write the Windows path under `#[cfg(windows)]`
   and a **compiling** fallback elsewhere (DESIGN §1). Nothing may be
   `cfg`-excluded into silence on unix.
3. **Never let a bound be OS-dependent.** The 20 000-char prompt cap and the
   turn deadline apply on every platform, so a Windows-shaped input is
   reproducible on Linux and vice versa.
4. Every new platform-specific behaviour gets a row in the §2 table above and,
   where it is an invariant, an assertion that runs in `cargo test` (I1 no
   console window · I2 env scrub · I4 prompt cap).
5. **CI stays quota-free.** No test spends model tokens unless it is
   `#[ignore]`d and gated on `PIE_LIVE=1`.
6. If a platform-specific behaviour changes in code, update this table in the
   same commit. An out-of-date summary here is worse than no summary at all,
   because DESIGN.md is then the only truth and nobody knows it.

## 6. Related files

- `planning/DESIGN.md` — §1 platform facts, §6 process contract, §7 state,
  §8 config/env scrubbing, §9 backend argv. **Authoritative.**
- `planning/ACCEPTANCE.md` — §1 the six gates (G1–G6), §2 global invariants I1–I11, §9 the bug log.
- `.gitattributes` — line-ending normalisation; LF is pinned for source and
  scripts so a Linux/WSL/macOS checkout is byte-identical to a Windows one.
- `scripts/qc.sh` · `scripts/qc.ps1` — the local gate runners. CI runs the same
  commands.
