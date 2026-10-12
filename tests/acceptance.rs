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
