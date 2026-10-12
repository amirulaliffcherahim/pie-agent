<#
.SYNOPSIS
    pie quality-control gate runner for Windows (PowerShell).

.DESCRIPTION
    The Windows equivalent of scripts/qc.sh. Both run the SAME gates in the
    SAME order so the two platforms cannot drift apart.

    Gates (planning/ACCEPTANCE.md section 1):

      G1  cargo fmt --check                              no diff
      G2  cargo clippy --all-targets -- -D warnings       zero warnings
      G3  cargo test                                      all pass
      G3b every #[ignore] carries a reason
      G3c module layout: no mod.rs files
      G3d no vendor state: the whole suite with HOME redirected to an empty dir
      G4  offline acceptance - M0: cargo run -- --list-backends
          ... one per milestone, and one per M5/M6 stage
      G4  `pie tui` on a pipe refuses with a message and no escape codes
      G6  cargo check --target x86_64-unknown-linux-gnu --all-targets
          --no-default-features (cross-target; SKIPped loudly when the target
          is not installed). `--no-default-features` is REQUIRED, not a
          preference: with the default `http` feature the check needs a C
          compiler for the target and fails.
      G5  live acceptance                                  ONLY with -Live

    G4 is the milestone's offline acceptance command, one gate per milestone
    (M0: `cargo run -- --list-backends`, M1: `cargo test --test parse_agy`,
    M2: `cargo test --test parse_codex`, M3: `cargo test --test profile` and
    `cargo run -- profile`, M4: `cargo run -- auth` and `cargo run -- usage`,
    M5 sessions: `cargo test --test sessions`). CI runs the same commands
    (.github/workflows/ci.yml) so local and CI agree.

    FAIL-FAST: the first failing gate writes `FAIL: <gate>` to stderr and exits
    immediately with that gate's exit code. Later gates do not run - a red G2
    means the G3 result would be meaningless.

    -Live: THIS SPENDS REAL SUBSCRIPTION QUOTA. It additionally runs the
    `#[ignore]`d smoke tests in tests/live_smoke.rs against the real vendor
    CLIs, requires PIE_LIVE=1 (set for the child only), and may only use the
    approved models agy/gemini-3.8-flash and codex/gpt-6-luna. It must NEVER be
    run in CI, and is skippable only by explicit user decision - in which case
    the milestone is recorded as `offline-verified` rather than `done`.

    Windows-specific note: nothing in this script needs to hide a console
    window, because `cargo` is run in the foreground on purpose. Hidden child
    windows are a concern for pie itself when it spawns the vendor CLIs, and
    that is handled in src/process.rs via the CREATE_NO_WINDOW (0x0800_0000)
    creation flag. See planning/PLATFORMS.md.

    THIS FILE MUST STAY PURE ASCII. Windows PowerShell 5.1 (powershell.exe, the
    version that ships with Windows) reads a .ps1 file as Windows-1252 when it
    has no UTF-8 BOM, so a single non-ASCII character inside a string literal
    is re-read as mojibake and the script fails to PARSE - not at runtime, not
    with a useful message, just "Unexpected token". That is not hypothetical:
    a UTF-8 em-dash in this file's comments broke it exactly that way, and the
    Windows gate runner was broken on the primary platform while looking
    perfectly fine in an editor. Keep it ASCII-only; CI checks this.

.EXAMPLE
    powershell -NoProfile -ExecutionPolicy Bypass -File scripts/qc.ps1
    # G1 -> G2 -> G3 -> G4, stop at the first failure.

.EXAMPLE
    powershell -NoProfile -ExecutionPolicy Bypass -File scripts/qc.ps1 -Live
    # Also runs the live smoke tests. Spends real subscription quota.

.NOTES
    Invoke it with `powershell` (or `./scripts/qc.ps1` from PowerShell).
    PowerShell 7 (`pwsh`) runs this unchanged if you have it, but it is NOT
    required and is not installed on the development machine. If script
    execution is blocked:
        powershell -NoProfile -ExecutionPolicy Bypass -File scripts/qc.ps1
#>

[CmdletBinding()]
param(
    # Also run the live smoke tests. Spends real subscription quota.
    [switch]$Live
)

# Any unhandled error terminates the script; every gate additionally checks
# $LASTEXITCODE explicitly, because native command exit codes do not raise
# terminating errors on their own.
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

# Run from the repository root regardless of the current location.
$RepoRoot = Split-Path -Parent $PSScriptRoot
Set-Location -LiteralPath $RepoRoot

$script:GatesRun = 0
$script:GatesSkipped = 0

function Invoke-Gate {
    <#
    .SYNOPSIS
        Run one gate's cargo command, fail fast on a nonzero exit code.
    #>
    param(
        [Parameter(Mandatory)][string]   $Label,
        [Parameter(Mandatory)][string[]] $Argv
    )

    Write-Host ''
    Write-Host "==> ${Label}: cargo $($Argv -join ' ')"

    & cargo @Argv
    $exitCode = $LASTEXITCODE

    if ($exitCode -ne 0) {
        # Use the raw stderr stream rather than Write-Error: with
        # $ErrorActionPreference = 'Stop', Write-Error would throw a
        # terminating error before the exit code is propagated.
        [Console]::Error.WriteLine('')
        [Console]::Error.WriteLine("FAIL: $Label")
        [Console]::Error.WriteLine("     command: cargo $($Argv -join ' ')")
        [Console]::Error.WriteLine("     exit: $exitCode")
        exit $exitCode
    }

    $script:GatesRun++
    Write-Host "PASS: $Label"
}

Write-Host "pie QC gates - $RepoRoot"
if ($Live) {
    Write-Host 'live smoke: ENABLED (real subscription quota will be spent)'
} else {
    Write-Host 'live smoke: disabled (pass -Live to enable)'
}

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    [Console]::Error.WriteLine('FAIL: preflight (cargo not found on PATH - install the Rust toolchain)')
    exit 1
}

# --- G1 ---------------------------------------------------------------------
Invoke-Gate -Label 'G1 format' -Argv @('fmt', '--check')

# --- G2 ---------------------------------------------------------------------
Invoke-Gate -Label 'G2 lint' -Argv @('clippy', '--all-targets', '--', '-D', 'warnings')

# --- G3 ---------------------------------------------------------------------
# Live smoke tests are `#[ignore]`d and gated on PIE_LIVE=1, so a plain
# `cargo test` never touches the network or spends quota.
Invoke-Gate -Label 'G3 tests' -Argv @('test')

# --- G3b (every `#[ignore]` carries a reason) --------------------------------
# ACCEPTANCE section 1 makes "0 ignored-without-reason" part of G3's pass
# condition, but `cargo test` reports an unreasoned ignore as merely ignored, so
# nothing enforced it. A bare `#[ignore]` is a test that silently stopped running.
$ignoreHits = Select-String -Path (Get-ChildItem -Path src, tests -Recurse -Include *.rs).FullName `
    -Pattern '^\s*#\[ignore\]\s*$'
if ($ignoreHits) {
    [Console]::Error.WriteLine('FAIL: G3b ignore reasons')
    $ignoreHits | ForEach-Object { [Console]::Error.WriteLine("     $($_.Path):$($_.LineNumber)") }
    exit 1
}
$script:GatesRun++
Write-Host 'PASS: G3b ignore reasons'

# --- G3c (module layout: no `mod.rs`) ---------------------------------------
# ACCEPTANCE section 3 requires "no `mod.rs` files; `provider.rs` sits beside
# `provider/`" - one idiom for a parent module and its directory (DESIGN section 2).
#
# Nothing enforced it: no test, no script, no CI job. M6's own plan then named
# `src/tui/mod.rs`, the scaffold created exactly that file, and the whole suite
# stayed green - the same family as B20 (a rule with nothing able to contradict
# it) and the same shape as the G6 gap found in the same session. A convention
# with no check is a convention that lasts until the next agent does not read the
# section.
#
# Cheap to check and impossible to argue with: if the directory holds a `mod.rs`,
# the parent module file is in the wrong place.
$modFiles = Get-ChildItem -Path src -Recurse -Filter mod.rs -File
if ($modFiles) {
    [Console]::Error.WriteLine('')
    [Console]::Error.WriteLine('FAIL: G3c module layout (no mod.rs files - the parent module file belongs BESIDE its directory, e.g. provider.rs + provider/)')
    $modFiles | ForEach-Object { [Console]::Error.WriteLine("     $($_.FullName)") }
    exit 1
}
$script:GatesRun++
Write-Host 'PASS: G3c module layout'

# --- G3d (no test may depend on machine vendor state) -----------------------
# The whole suite is run again with `HOME` and `USERPROFILE` redirected to an empty
# directory, because **that is what a CI runner is**.
#
# This exists because four tests in `tests/parse_agy.rs` were passing only on a
# machine with agy logged in. `Agy::stream` refuses without a subscription
# credential, their helper pointed `PIE_HOME` at a scratch directory but left
# `HOME` alone, and the developer's real `~/.gemini` satisfied the gate. On every CI
# runner it did not, so `G4 acceptance (M1)` - M1's own named gate - was red on
# ubuntu, windows and macos, while the local gate run was green and the docs claimed
# CI was the real Linux evidence. Nothing could have caught it locally.
#
# A green suite whose greenness comes from the machine is not evidence, and it is
# invisible from the machine. This gate is the general form of that observation:
# run the tests somewhere the developer's state does not exist.
#
# `RUSTUP_HOME`/`CARGO_HOME` are carried over explicitly only because rustup cannot
# find a toolchain once `HOME` moves; they are toolchain plumbing, not vendor state.
$realHome = if ($env:HOME) { $env:HOME } else { $env:USERPROFILE }
$emptyHome = Join-Path ([System.IO.Path]::GetTempPath()) ("pie-g3d-" + [System.Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $emptyHome -Force | Out-Null
$savedHome = $env:HOME
$savedProfile = $env:USERPROFILE
$savedRustup = $env:RUSTUP_HOME
$savedCargo = $env:CARGO_HOME
$g3dExit = 0
try {
    $env:HOME = $emptyHome
    $env:USERPROFILE = $emptyHome
    if (-not $env:RUSTUP_HOME) { $env:RUSTUP_HOME = Join-Path $realHome '.rustup' }
    if (-not $env:CARGO_HOME) { $env:CARGO_HOME = Join-Path $realHome '.cargo' }

    Write-Host ''
    Write-Host '==> G3d no vendor state: cargo test (HOME redirected to an empty directory)'
    & cargo test
    $g3dExit = $LASTEXITCODE
}
finally {
    $env:HOME = $savedHome
    $env:USERPROFILE = $savedProfile
    $env:RUSTUP_HOME = $savedRustup
    $env:CARGO_HOME = $savedCargo
    Remove-Item -Recurse -Force $emptyHome -ErrorAction SilentlyContinue
}
if ($g3dExit -ne 0) {
    [Console]::Error.WriteLine('')
    [Console]::Error.WriteLine('FAIL: G3d no vendor state')
    [Console]::Error.WriteLine('     command: cargo test, with HOME/USERPROFILE pointed at an empty directory')
    [Console]::Error.WriteLine('     a test passed only because this machine has a vendor CLI logged in, or other')
    [Console]::Error.WriteLine('     state under HOME. Every test must pass where none of that exists, because')
    [Console]::Error.WriteLine('     that is what CI is. Have the test establish the state it asserts about.')
    exit $g3dExit
}
$script:GatesRun++
Write-Host 'PASS: G3d no vendor state'

# --- G4 (offline acceptance, one gate per milestone) ------------------------
# The per-milestone acceptance commands from planning/ACCEPTANCE.md sections 3-8.
# G3 already runs every test binary, so these are partly redundant on purpose:
# the point is that a milestone's own acceptance command is named and fails
# individually, so a red gate says *which* milestone regressed rather than just
# "tests failed". Add the next milestone's command here as it lands.
Invoke-Gate -Label 'G4 acceptance (M0)' -Argv @('run', '--', '--list-backends')
Invoke-Gate -Label 'G4 acceptance (M1)' -Argv @('test', '--test', 'parse_agy')
Invoke-Gate -Label 'G4 acceptance (M2)' -Argv @('test', '--test', 'parse_codex')
Invoke-Gate -Label 'G4 acceptance (M3 tests)' -Argv @('test', '--test', 'profile')
Invoke-Gate -Label 'G4 acceptance (M3 profile)' -Argv @('run', '--', 'profile')
Invoke-Gate -Label 'G4 acceptance (M4 auth)' -Argv @('run', '--', 'auth')
Invoke-Gate -Label 'G4 acceptance (M4 usage)' -Argv @('run', '--', 'usage')
# M5's stages share one test binary (cargo test --test sessions), so its gate is
# named for the store rather than for the stage that landed first: a label naming
# only stage A would lie once B (continue), C (compact) and F (switch) are
# covered by the same command.
Invoke-Gate -Label 'G4 acceptance (M5 sessions)' -Argv @('test', '--test', 'sessions')
Invoke-Gate -Label 'G4 acceptance (M5 compact)' -Argv @('test', '--test', 'compact')
Invoke-Gate -Label 'G4 acceptance (M5 ocr)' -Argv @('test', '--test', 'ocr')
# Stage E's server has its own binary, so its gate is its own command: the stdio
# handshake, driven through the built binary rather than through the library.
Invoke-Gate -Label 'G4 acceptance (M5 mcp)' -Argv @('test', '--test', 'mcp')

# M6's stages each have their OWN test binary, so a red gate names the stage that
# broke rather than just "the TUI failed" (ACCEPTANCE sections 8 and 9: M5 shipped
# two gates that could not fail their own stage). Stage D gets its own binary even
# though it refines stage A's subject matter, for exactly that attribution reason.
Invoke-Gate -Label 'G4 acceptance (M6 tui_app)' -Argv @('test', '--test', 'tui_app')
Invoke-Gate -Label 'G4 acceptance (M6 tui_render)' -Argv @('test', '--test', 'tui_render')
Invoke-Gate -Label 'G4 acceptance (M6 tui_loop)' -Argv @('test', '--test', 'tui_loop')
Invoke-Gate -Label 'G4 acceptance (M6 tui_keys)' -Argv @('test', '--test', 'tui_keys')

# M7's stages each have their own binary, for the same attribution reason as M6's.
Invoke-Gate -Label 'G4 acceptance (M7 learn)' -Argv @('test', '--test', 'learn')
Invoke-Gate -Label 'G4 acceptance (M7 consolidate)' -Argv @('test', '--test', 'consolidate')
Invoke-Gate -Label 'G4 acceptance (M7 schedule)' -Argv @('test', '--test', 'schedule')
Invoke-Gate -Label 'G4 acceptance (M7 commands)' -Argv @('test', '--test', 'memory_cmd')

# --- G4 (M6 tui non-TTY) ----------------------------------------------------
# ACCEPTANCE section 9: `pie tui` on a non-TTY stdin must fail with a clear message
# rather than emitting escape codes into a pipe.
#
# This is a property of the BUILT BINARY, so it cannot be a cargo test: `cargo test`
# has whatever stdio the harness gives it, and the check is precisely about what the
# binary does when that is not a terminal. `'' |` makes stdin a pipe deterministically
# instead of depending on how the runner was invoked.
#
# Both halves are asserted. Asserting only the exit code would pass for a `pie tui`
# that exited non-zero after printing escape codes; asserting only "no escapes in the
# output" would pass for one that started the TUI and then failed for another reason.
# Two traps here, and the first version fell into both.
#
#  1. Invoke-Gate cannot be used: this command is EXPECTED to fail, so a non-zero
#     exit is the pass condition.
#  2. `2>&1` on a native command's stderr raises NativeCommandError under
#     `$ErrorActionPreference = 'Stop'`, which ABORTED THE SCRIPT before any check
#     ran. The gate then exited 1 having printed no FAIL line at all - a gate that
#     fails without saying why, which is precisely the defect this whole file exists
#     to prevent, committed by the check written to catch it. The preference is
#     therefore relaxed for the invocation only, and every path reports a named FAIL.
Invoke-Gate -Label 'G4 acceptance (M6 tui build)' -Argv @('build')
$tuiExe = Join-Path $RepoRoot 'target/debug/pie.exe'
# PowerShell 5.1 has no `` `e `` escape sequence (that is a PowerShell 6 addition),
# and `powershell.exe` is exactly the version this runner targets - so the ESC byte
# is built from its code point rather than written as an escape that would silently
# become a literal backtick-e and make the escape check pass for any output at all.
$esc = [string][char]27
$previousPreference = $ErrorActionPreference
$tuiExit = -1
$tuiOutput = ''
$tuiThrew = $null
try {
    $ErrorActionPreference = 'Continue'
    $tuiOutput = ('' | & $tuiExe tui 2>&1 | Out-String)
    $tuiExit = $LASTEXITCODE
}
catch {
    # Reported rather than swallowed: an invocation that never ran must not be read
    # as "it refused", which a non-zero default would have meant.
    $tuiThrew = "$_"
}
finally {
    $ErrorActionPreference = $previousPreference
}

$tuiFailure = $null
if ($null -ne $tuiThrew) {
    $tuiFailure = "the invocation threw: $tuiThrew"
} elseif ($null -eq $tuiExit) {
    $tuiFailure = 'no exit code was captured, so the binary may never have run'
} elseif ($tuiExit -eq 0) {
    $tuiFailure = '`pie tui` must NOT succeed with stdin on a pipe: it would have taken over the terminal'
} elseif ($tuiOutput.Contains($esc + '[')) {
    $tuiFailure = 'escape codes reached the pipe before the refusal'
} elseif ($tuiOutput -notmatch 'terminal') {
    $tuiFailure = "the refusal must name the reason; got: $tuiOutput"
}

if ($null -ne $tuiFailure) {
    [Console]::Error.WriteLine('')
    [Console]::Error.WriteLine('FAIL: G4 acceptance (M6 tui non-TTY)')
    [Console]::Error.WriteLine('     command: pie tui, with stdin on a pipe')
    [Console]::Error.WriteLine("     exit: $tuiExit")
    [Console]::Error.WriteLine("     reason: $tuiFailure")
    [Console]::Error.WriteLine("     output: $tuiOutput")
    exit 1
}
$script:GatesRun++
Write-Host 'PASS: G4 acceptance (M6 tui non-TTY)'

# --- G6 (cross-target check) ------------------------------------------------
# ACCEPTANCE section 1. Linux cannot be *run* locally (WSL2 is installed but
# cannot boot - PLATFORMS.md F3), so this catches cfg-gated compile errors
# without linking. CI's ubuntu job is the real Linux evidence; this is the local
# pre-flight that catches those errors before a CI round trip.
#
# TWO things this has to get right, and until M6 it got both wrong:
#
#   1. It had NO RUNNER AT ALL. ACCEPTANCE section 1 lists G6 as a required gate
#      and every run-log row reports it green, but neither this script nor
#      scripts/qc.sh ever ran it - it was a hand-run command and a claim, which
#      is B20's shape ("a gate ran but could not fail") in its other direction:
#      a gate that could not run. The run-log G6 column rested on nothing that
#      could have contradicted it.
#
#   2. The documented command COULD NOT PASS AS WRITTEN.
#      `cargo check --target x86_64-unknown-linux-gnu --all-targets` fails
#      locally with "failed to find tool x86_64-linux-gnu-gcc", because the
#      default `http` feature pulls reqwest -> rustls -> ring, which needs a C
#      compiler for the target. The flag that makes it work is
#      `--no-default-features`. Same class as B12: a criterion derived from a
#      document rather than from running the command.
#
# The cost of that second point is real and stated rather than hidden:
# `--no-default-features` means this check does NOT cover cfg(feature = "http")
# code on Linux. The CI ubuntu job covers it natively, and is the stronger
# evidence.
#
# A missing target is reported as SKIP, loudly and counted separately, never as a
# pass: a silent skip is exactly how B20 happened.
$g6Target = 'x86_64-unknown-linux-gnu'
$g6Installed = $false
if (Get-Command rustup -ErrorAction SilentlyContinue) {
    $g6Installed = (rustup target list --installed 2>$null) -contains $g6Target
}
if ($g6Installed) {
    Invoke-Gate -Label 'G6 cross-target (linux, no-default-features)' `
        -Argv @('check', '--target', $g6Target, '--all-targets', '--no-default-features')
} else {
    [Console]::Error.WriteLine('')
    [Console]::Error.WriteLine("SKIP: G6 cross-target - $g6Target is not installed")
    [Console]::Error.WriteLine("      rustup target add $g6Target")
    $script:GatesSkipped++
}

# --- G5 (live acceptance) ---------------------------------------------------
if ($Live) {
    [Console]::Error.WriteLine('')
    [Console]::Error.WriteLine('!! -Live: the next gate spends REAL subscription quota on the')
    [Console]::Error.WriteLine('!! approved models only (agy/gemini-3.8-flash, codex/gpt-6-luna).')

    # Set for the child process only; cleared again so nothing later inherits it.
    $env:PIE_LIVE = '1'
    try {
        # `--test-threads=1` keeps real CLI turns serialised so quota accounting
        # and any rate limiting stay legible.
        Invoke-Gate -Label 'G5 live smoke' -Argv @('test', '--test', 'live_smoke', '--', '--ignored', '--test-threads=1')
    }
    finally {
        Remove-Item Env:PIE_LIVE -ErrorAction SilentlyContinue
    }
}

$total = if ($Live) { $script:GatesRun } else { $script:GatesRun + 1 }
$suffix = if ($Live) { '' } else { ' (G5 live smoke not run - milestone would be `offline-verified`)' }
$skipped = if ($script:GatesSkipped -gt 0) { ", $($script:GatesSkipped) SKIPPED" } else { '' }
Write-Host ''
Write-Host "==> verdict: PASS - $($script:GatesRun)/$total gates green$skipped$suffix"
exit 0
