<#
.SYNOPSIS
    pie quality-control gate runner for Windows (PowerShell).

.DESCRIPTION
    The Windows equivalent of scripts/qc.sh. Both run the SAME gates in the
    SAME order so the two platforms cannot drift apart.

    Gates (planning/ACCEPTANCE.md §1):

      G1  cargo fmt --check                              no diff
      G2  cargo clippy --all-targets -- -D warnings       zero warnings
      G3  cargo test                                      all pass
      G4  offline acceptance — M0: cargo run -- --list-backends
      G5  live acceptance                                 ONLY with -Live

    G4 is the milestone's offline acceptance command. Only the M0 command is
    wired in here; as M1/M2 land, their acceptance commands (`cargo test --test
    parse_agy`, `cargo test --test parse_codex`) are added next to it. CI runs
    the same command (.github/workflows/ci.yml) so local and CI agree.

    FAIL-FAST: the first failing gate writes `FAIL: <gate>` to stderr and exits
    immediately with that gate's exit code. Later gates do not run — a red G2
    means the G3 result would be meaningless.

    -Live: THIS SPENDS REAL SUBSCRIPTION QUOTA. It additionally runs the
    `#[ignore]`d smoke tests in tests/live_smoke.rs against the real vendor
    CLIs, requires PIE_LIVE=1 (set for the child only), and may only use the
    approved models agy/gemini-3.8-flash and codex/gpt-6-luna. It must NEVER be
    run in CI, and is skippable only by explicit user decision — in which case
    the milestone is recorded as `offline-verified` rather than `done`.

    Windows-specific note: nothing in this script needs to hide a console
    window, because `cargo` is run in the foreground on purpose. Hidden child
    windows are a concern for pie itself when it spawns the vendor CLIs, and
    that is handled in src/proc.rs via the CREATE_NO_WINDOW (0x0800_0000)
    creation flag. See planning/PLATFORMS.md.

.EXAMPLE
    ./scripts/qc.ps1
    # G1 -> G2 -> G3 -> G4, stop at the first failure.

.EXAMPLE
    ./scripts/qc.ps1 -Live
    # Also runs the live smoke tests. Spends real subscription quota.

.NOTES
    If script execution is blocked, run:
        pwsh -NoProfile -ExecutionPolicy Bypass -File scripts/qc.ps1
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

Write-Host "pie QC gates — $RepoRoot"
if ($Live) {
    Write-Host 'live smoke: ENABLED (real subscription quota will be spent)'
} else {
    Write-Host 'live smoke: disabled (pass -Live to enable)'
}

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    [Console]::Error.WriteLine('FAIL: preflight (cargo not found on PATH — install the Rust toolchain)')
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

# --- G4 (M0 offline acceptance) ---------------------------------------------
Invoke-Gate -Label 'G4 acceptance (M0)' -Argv @('run', '--', '--list-backends')

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
$suffix = if ($Live) { '' } else { ' (G5 live smoke not run — milestone would be `offline-verified`)' }
Write-Host ''
Write-Host "==> verdict: PASS — $($script:GatesRun)/$total gates green$suffix"
exit 0
