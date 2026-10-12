//! Live smoke tests — gate **G5**.
//!
//! # This spends real subscription quota
//!
//! These tests drive the real `agy` and `codex` CLIs. They are `#[ignore]`d, so
//! a plain `cargo test` never runs them, and they additionally require
//! `PIE_LIVE=1` so that `--ignored` cannot be used by accident. Both belts are
//! deliberate: quota is real money on a subscription plan.
//!
//! Run them with the gate runner:
//!
//! ```text
//! bash scripts/qc.sh --live        # Linux / WSL / macOS
//! pwsh scripts/qc.ps1 -Live        # Windows
//! ```
//!
//! # Approved models only
//!
//! `planning/ACCEPTANCE.md` §1 fixes the models: **`agy/gemini-3.8-flash`** and
//! **`codex/gpt-6-luna`**. [`APPROVED_MODELS`] is asserted rather than trusted,
//! so a careless edit to a prompt in this file cannot silently start billing a
//! more expensive model.
//!
//! # Why an end-to-end test and not a unit test
//!
//! Everything else in the suite tests parsers against recorded fixtures. Only
//! this file proves the three things fixtures cannot: that the CLI accepts the
//! argv pie builds, that the real stream matches the contract pie was written
//! against, and that a turn is actually recorded in the session log.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::process::Command;

/// The only models a live gate may use (`planning/ACCEPTANCE.md` §1).
const APPROVED_MODELS: [&str; 2] = ["agy/gemini-3.8-flash", "codex/gpt-6-luna"];

/// A trivial prompt: enough to produce a turn, cheap enough to be polite.
const PROMPT: &str = "Reply with the single word: ok";

/// A generous deadline so a slow cold start does not look like a failure, but
/// a hung CLI still fails instead of hanging the suite.
const TURN_TIMEOUT_SECS: &str = "300";

/// Whether live tests are allowed to run at all.
fn live_enabled() -> bool {
    std::env::var("PIE_LIVE").is_ok_and(|v| v == "1")
}

/// Guard that keeps a live test from running by accident.
///
/// Returns `false` when the tests must be skipped. Prints why, so an operator
/// who ran `--ignored` without `PIE_LIVE=1` is not left guessing.
fn ready(model: &str) -> bool {
    assert!(
        APPROVED_MODELS.contains(&model),
        "`{model}` is not an approved live model. Approved: {APPROVED_MODELS:?} (ACCEPTANCE.md §1)"
    );

    if !live_enabled() {
        eprintln!("skipping: set PIE_LIVE=1 to run live smoke tests (this spends subscription quota)");
        return false;
    }
    true
}

/// Run `pie` with an isolated state directory.
///
/// `PIE_HOME` points at a throwaway directory for two reasons: the developer's
/// real `~/.pie/usage.jsonl` is not polluted with test rows, and the presence of
/// a session-log row becomes an assertion we can make.
fn run_pie(home: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_pie"))
        .args(args)
        .env("PIE_HOME", home)
        // Live runs must not inherit a strip-env opt-out from the shell.
        .env_remove("PIE_STRIP_ENV")
        .output()
        .expect("failed to run the pie binary")
}

/// Rows in the session log written under `home`.
fn session_rows(home: &Path) -> Vec<serde_json::Value> {
    let path = home.join("usage.jsonl");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

/// Assert one turn succeeded and was recorded.
fn assert_turn_recorded(home: &Path, provider: &str, output: &std::process::Output) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "`{provider}` turn failed with {:?}\nstdout: {stdout}\nstderr: {stderr}",
        output.status.code()
    );
    assert!(
        !stdout.trim().is_empty(),
        "`{provider}` produced no answer on stdout"
    );

    let rows = session_rows(home);
    assert_eq!(rows.len(), 1, "expected exactly one session-log row, got {rows:#?}");

    let row = &rows[0];
    assert_eq!(row["provider"], provider);
    assert_eq!(row["ok"], true, "row should record success: {row}");
    assert!(
        row["input_tokens"].as_u64().unwrap_or(0) > 0,
        "a real turn must report input tokens: {row}"
    );
    assert!(
        row["duration_ms"].as_u64().unwrap_or(0) > 0,
        "a real turn must report a duration: {row}"
    );
    // The frozen schema (DESIGN §7) must be honoured by every backend.
    for key in [
        "ts",
        "provider",
        "model",
        "input_tokens",
        "cached_input_tokens",
        "output_tokens",
        "reasoning_tokens",
        "cost_usd_micros",
        "credits_consumed",
        "credits_remaining",
        "duration_ms",
        "ok",
        "error_kind",
        "stop_reason",
    ] {
        assert!(row.get(key).is_some(), "session row is missing `{key}`: {row}");
    }
}

#[test]
#[ignore = "spends real subscription quota; requires PIE_LIVE=1 and a logged-in agy"]
fn agy_turn_succeeds_and_is_recorded() {
    if !ready("agy/gemini-3.8-flash") {
        return;
    }
    let home = tempfile::tempdir().unwrap();

    let output = run_pie(
        home.path(),
        &[
            "-p",
            PROMPT,
            "--model",
            "agy/gemini-3.8-flash",
            "--timeout",
            TURN_TIMEOUT_SECS,
        ],
    );

    assert_turn_recorded(home.path(), "agy", &output);

    let row = &session_rows(home.path())[0];
    assert_eq!(row["model"], "gemini-3.8-flash");
}

/// The agy quota probe is a slash expansion, so it must take the
/// `--disable-slash-commands`-free path (`knowledge/agy.md`).
#[test]
#[ignore = "spends real subscription quota; requires PIE_LIVE=1 and a logged-in agy"]
fn agy_usage_probe_reports_buckets() {
    if !live_enabled() {
        eprintln!("skipping: set PIE_LIVE=1 to run live smoke tests");
        return;
    }
    let home = tempfile::tempdir().unwrap();

    let output = run_pie(home.path(), &["usage", "agy"]);
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        output.status.success(),
        "`pie usage agy` failed: {stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("agy quota"),
        "expected a quota table, got:\n{stdout}"
    );

    // A slash expansion must not be a model turn, so it must not be logged.
    assert!(
        session_rows(home.path()).is_empty(),
        "a quota probe must not spend a turn"
    );
}

#[test]
#[ignore = "spends real subscription quota; requires PIE_LIVE=1 and a logged-in codex"]
fn codex_turn_succeeds_and_is_recorded() {
    if !ready("codex/gpt-6-luna") {
        return;
    }
    let home = tempfile::tempdir().unwrap();

    let output = run_pie(
        home.path(),
        &[
            "-p",
            PROMPT,
            "--model",
            "codex/gpt-6-luna",
            "--timeout",
            TURN_TIMEOUT_SECS,
        ],
    );

    assert_turn_recorded(home.path(), "codex", &output);

    let row = &session_rows(home.path())[0];
    assert_eq!(row["model"], "gpt-6-luna");
}

#[test]
#[ignore = "spends real subscription quota; requires PIE_LIVE=1 and a logged-in codex"]
fn codex_usage_probe_reports_buckets() {
    if !live_enabled() {
        eprintln!("skipping: set PIE_LIVE=1 to run live smoke tests");
        return;
    }
    let home = tempfile::tempdir().unwrap();

    let output = run_pie(home.path(), &["usage", "codex"]);
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        output.status.success(),
        "`pie usage codex` failed: {stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("codex quota"),
        "expected a quota table, got:\n{stdout}"
    );
    assert!(
        session_rows(home.path()).is_empty(),
        "a quota probe must not spend a turn"
    );
}

/// A live turn must never leak a credential into its output, even if the vendor
/// CLI prints one in an error path. `I3` / `I10`.
#[test]
#[ignore = "spends real subscription quota; requires PIE_LIVE=1 and a logged-in agy"]
fn a_live_turn_does_not_leak_key_material() {
    if !live_enabled() {
        eprintln!("skipping: set PIE_LIVE=1 to run live smoke tests");
        return;
    }
    let home = tempfile::tempdir().unwrap();

    let output = run_pie(
        home.path(),
        &[
            "-p",
            PROMPT,
            "--model",
            "agy/gemini-3.8-flash",
            "--timeout",
            TURN_TIMEOUT_SECS,
        ],
    );

    for stream in [&output.stdout, &output.stderr] {
        let text = String::from_utf8_lossy(stream);
        for needle in ["sk-", "Bearer ", "eyJ", "access_token"] {
            assert!(
                !text.contains(needle),
                "live output contained `{needle}`:\n{text}"
            );
        }
    }
}
