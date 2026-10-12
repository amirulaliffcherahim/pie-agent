//! Gate G4 for M0: assert what `--list-backends` actually *prints*.
//!
//! The gate runners check only the exit status, so a table that printed nothing,
//! printed the wrong rows, or dropped a column would pass every gate. The QC
//! sweep flagged that gap (ACCEPTANCE section 9, B18). These tests close it by
//! running the real binary and asserting the output, which is what
//! `planning/ACCEPTANCE.md` section 1 means by "exact expected output".
//!
//! Runs no vendor CLI: version probes may spawn, but a missing CLI must still
//! produce a row, and no model call is made.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::process::Command;

/// The backend order ACCEPTANCE section 3 fixes.
const EXPECTED_ORDER: [&str; 3] = ["agy", "codex", "claude"];

fn run(home: &std::path::Path, args: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_pie"))
        .args(args)
        .env("PIE_HOME", home)
        .output()
        .expect("failed to run the pie binary");

    assert!(
        output.status.success(),
        "`pie {}` failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );

    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn list_backends_prints_exactly_three_rows_in_order() {
    let home = tempfile::tempdir().unwrap();
    let stdout = run(home.path(), &["--list-backends"]);

    let rows: Vec<&str> = stdout
        .lines()
        .skip(1) // header
        .filter(|line| !line.trim().is_empty())
        .collect();

    assert_eq!(
        rows.len(),
        EXPECTED_ORDER.len(),
        "expected one row per backend, got:\n{stdout}"
    );

    for (row, expected) in rows.iter().zip(EXPECTED_ORDER) {
        assert!(
            row.starts_with(expected),
            "rows must be {EXPECTED_ORDER:?} in order; got {row:?} where `{expected}` was due\n{stdout}"
        );
    }

    // The header names all five columns, so a dropped column is visible here.
    let header = stdout.lines().next().unwrap_or_default();
    for column in ["BACKEND", "BINARY", "VERSION", "AUTH", "DETAIL"] {
        assert!(
            header.contains(column),
            "header is missing `{column}`: {header}"
        );
    }
}

#[test]
fn every_backend_row_reports_a_binary_and_an_auth_state() {
    const AUTH_STATES: [&str; 5] = [
        "ready",
        "needs-login",
        "no-subscription",
        "binary-missing",
        "error",
    ];

    let home = tempfile::tempdir().unwrap();
    let stdout = run(home.path(), &["--list-backends"]);

    for row in stdout.lines().skip(1).filter(|l| !l.trim().is_empty()) {
        // Not split on whitespace: the columns are fixed-width, and a binary path
        // like `C:\Program Files\nodejs\node.exe` contains a space, which shifted
        // every field when this test first tried it.
        assert!(
            AUTH_STATES.iter().any(|state| row.contains(state)),
            "a row must report an auth state, one of {AUTH_STATES:?}: {row:?}"
        );
        assert!(
            row.trim().chars().count() > 20,
            "the binary column must never be blank: {row:?}"
        );
    }
}

#[test]
fn no_api_key_backend_leaks_into_the_table() {
    let home = tempfile::tempdir().unwrap();
    let stdout = run(home.path(), &["--list-backends"]);

    for absent in ["openrouter", "deepseek"] {
        assert!(
            !stdout.contains(absent),
            "`{absent}` is deferred to M10 and must not be registered:\n{stdout}"
        );
    }
}

/// The auth table is the same data without a subprocess, so it must agree.
#[test]
fn auth_reports_the_same_backends_as_list_backends() {
    let home = tempfile::tempdir().unwrap();
    let stdout = run(home.path(), &["auth"]);

    for id in EXPECTED_ORDER {
        assert!(stdout.contains(id), "`pie auth` omitted {id}:\n{stdout}");
    }
}

/// M4's Done condition, verbatim from `ROADMAP.md`: *both render without a selected
/// model*. Worth asserting because a command that silently required one would still
/// appear to work whenever a `--model` happened to be supplied.
#[test]
fn auth_and_usage_render_without_a_selected_model() {
    let home = tempfile::tempdir().unwrap();

    let auth = run(home.path(), &["auth"]);
    for id in EXPECTED_ORDER {
        assert!(auth.contains(id), "`pie auth` omitted {id}:\n{auth}");
    }
    for absent in ["openrouter", "deepseek"] {
        assert!(
            !auth.contains(absent),
            "`{absent}` is deferred to M10 and must not be registered:\n{auth}"
        );
    }
    assert!(
        auth.contains("no-subscription"),
        "claude has no subscription here and must say so:\n{auth}"
    );

    let usage = run(home.path(), &["usage"]);
    for window in ["all time", "last 7 days", "last 30 days"] {
        assert!(
            usage.contains(window),
            "`pie usage` is missing the {window} window:\n{usage}"
        );
    }
    for field in [
        "turns",
        "input tokens",
        "output tokens",
        "reasoning tokens",
        "total tokens",
        "cache hit rate",
    ] {
        assert!(
            usage.contains(field),
            "`pie usage` is missing `{field}`:\n{usage}"
        );
    }
}

/// I7: `auth` and `usage` are meant to be free, so neither may record a turn.
#[test]
fn auth_and_usage_write_no_session_row() {
    let home = tempfile::tempdir().unwrap();

    let _ = run(home.path(), &["auth"]);
    let _ = run(home.path(), &["usage"]);

    let log = home.path().join("usage.jsonl");
    let contents = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(
        contents.trim().is_empty(),
        "neither command may log a turn, but the log holds:\n{contents}"
    );
}

/// `pie usage codex` must **not** reach the private `wham/usage` endpoint unless the
/// user opts in.
///
/// User decision, 2026-10-12: the endpoint class is marked by its own vendor's
/// ecosystem as "Account may be restricted or banned", and an account risk nobody
/// opted into is not a reasonable thing for a default to spend. The capability is
/// kept — one config line or environment variable away — but the *default* is `off`.
///
/// Asserted here, offline and for free, rather than only in the live gate: a property
/// that needs no network and no model tokens should not be checked only by a gate that
/// costs real subscription quota. The live half — that opting in still returns real
/// buckets — stays in `tests/live_smoke.rs`, because only a live run can show that.
///
/// Deliberately does **not** need a codex credential: the disabled path returns before
/// the credential is read, which is the point of checking it before anything else.
#[test]
fn the_default_codex_usage_probe_is_disabled_and_says_so() {
    let home = tempfile::tempdir().unwrap();
    let stdout = run(home.path(), &["usage", "codex"]);

    assert!(
        stdout.contains("disabled by config"),
        "the default probe must be off and must say so, got:\n{stdout}"
    );
    assert!(
        stdout.contains("no buckets reported"),
        "with the probe off there are no buckets, and the table must not pretend otherwise:\n{stdout}"
    );
    assert!(
        stdout.contains("quota_probe"),
        "the note must name the setting that turns it back on, or the user cannot find it:\n{stdout}"
    );
    assert!(
        !stdout.contains('%'),
        "no remaining-fraction may be rendered when no bucket was fetched:\n{stdout}"
    );

    // The session-log totals half of the same command is unaffected by the switch: the
    // criterion in ACCEPTANCE section 5 requires totals either way, so a probe that is
    // off must not cost the user the half of `pie usage codex` that is always free.
    assert!(
        stdout.contains("all time"),
        "session-log totals must still render with the probe off:\n{stdout}"
    );
    assert!(
        stdout.contains("last 7 days") && stdout.contains("last 30 days"),
        "all three windows must still render:\n{stdout}"
    );

    // …and it is a probe, so it must not have recorded a turn.
    let log = home.path().join("usage.jsonl");
    let contents = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(
        contents.trim().is_empty(),
        "a probe may not log a turn, but the log holds:\n{contents}"
    );
}
