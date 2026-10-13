//! Output safety: **no production path may use a macro that panics on a failed write**.
//!
//! This is the regression guard for **S2**. `println!`, `print!` and `eprintln!` panic on *any*
//! write failure, and Rust ignores `SIGPIPE`, so a reader that goes away — `pie -p … | head -1`,
//! the most ordinary thing a user can do — turned into `EPIPE` and a panic: **exit 101**.
//!
//! That is a panic on external input, which invariant **I6** says cannot happen. The lint that
//! enforces I6 (`deny(clippy::panic)` in `lib.rs`) is structurally unable to see it, because the
//! panic is generated inside the standard library's macro rather than written in this crate. So
//! I6's enforcement had a hole with nothing able to observe it, which is the shape this
//! repository kept finding across M6–M11. `src/out.rs` closes the hole and this file makes sure
//! it stays closed.
//!
//! Two halves, and neither is sufficient alone:
//!
//! * the **static** scan would fail if a call site bypassed the helper, but it cannot see the
//!   helper misbehaving;
//! * the **process** test proves the binary at the head of a pipeline exits 0, but it cannot make
//!   the failure happen on demand — and a binary that wrote nothing would pass it for the wrong
//!   reason, which is what the paired control below exists to prevent.
//!
//! `src/out.rs` carries the third half: a unit test that drives the swallow-the-error function
//! with a writer that fails.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The macros that panic on a failed write. `eprintln!` is included because
/// `pie -p … 2>&1 | head -1` closes stderr just as readily, and a diagnostic that kills the
/// process it is diagnosing is worse than one that is lost.
const FORBIDDEN: [&str; 3] = ["println!(", "print!(", "eprintln!("];

fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_sources(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

#[test]
fn no_production_path_uses_a_macro_that_panics_on_a_failed_write() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut sources = Vec::new();
    rust_sources(&root.join("src"), &mut sources);

    // Non-vacuity, in three parts. A scan that walked nothing, or that looked for a string which
    // does not exist, reports "no offenders" for the same reason a correct scan does.
    assert!(sources.len() > 10, "the scan found no sources: {sources:?}");
    assert!(
        sources
            .iter()
            .any(|path| relative(&root, path) == "src/commands/profile.rs"),
        "the scan never looked at a file that used to offend, so it cannot say anything about it"
    );

    let mut offenders: Vec<String> = Vec::new();
    for path in &sources {
        let file = relative(&root, path);
        let text = std::fs::read_to_string(path).unwrap();

        // Test code may print freely: it is not a user-facing output path, and the crate's
        // `deny` on panics does not apply there either. Everything from the last `#[cfg(test)]`
        // onward is test code.
        let split = text.rfind("#[cfg(test)]").unwrap_or(text.len());
        let production = &text[..split];

        for (index, line) in production.lines().enumerate() {
            // A comment that names the macro — including the rustdoc example in `process.rs`
            // and the prose note in `mcp.rs` — is documentation about the rule, not a use of it.
            let code = line.trim_start();
            if code.starts_with("//") {
                continue;
            }
            for macro_name in FORBIDDEN {
                if line.contains(macro_name) {
                    offenders.push(format!("{file}:{} {}", index + 1, code.trim_end()));
                }
            }
        }
    }

    // And the marker has to be findable *somewhere*, or "found none" would be a statement about
    // the search string rather than about the tree. The two production files that name it in
    // prose are skipped above on purpose, so they are the proof that the scan can see it.
    let all: String = sources
        .iter()
        .map(|path| std::fs::read_to_string(path).unwrap())
        .collect::<Vec<String>>()
        .join("\n");
    assert!(
        FORBIDDEN.iter().any(|macro_name| all.contains(macro_name)),
        "the scan cannot find any of {FORBIDDEN:?} anywhere in src/, so finding none in a \
         production path means the search is wrong rather than the tree being clean"
    );

    assert!(
        offenders.is_empty(),
        "these production paths use a macro that panics when the reader goes away, so \
         `pie … | head -1` exits 101 with a broken-pipe message instead of succeeding (I6, S2). \
         Route them through `crate::out` (`outln!`, `outfrag!`, `errln!`):\n{}",
        offenders.join("\n")
    );
}

/// The process-level half: a real `pie` whose stdout reader goes away must exit **0**.
///
/// **Why the read end is closed rather than the pipe filled.** Closing is immediate and
/// deterministic — an empty pipe with no reader fails on the *first* write, whereas a full pipe
/// would block and need a payload larger than the 64 KiB pipe buffer to provoke anything at all.
/// The one thing this depends on is that the child has not already written when the parent closes
/// the handle. `--list-backends` makes that all but certain: its first write sits behind three
/// vendor version probes, i.e. three spawned processes, while the parent closes the handle
/// immediately after `spawn` returns.
#[test]
fn a_reader_that_goes_away_does_not_panic_the_process() {
    let home = tempfile::tempdir().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_pie"))
        .args(["--list-backends"])
        .env("PIE_HOME", home.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to run the pie binary");

    // Dropping `ChildStdout` closes the read end of the child's stdout.
    drop(child.stdout.take());

    let output = child.wait_with_output().expect("failed to wait for pie");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !stderr.contains("failed printing to stdout"),
        "pie reported a failed stdout write instead of ignoring it, which is the panic this \
         guard exists to prevent: {stderr}"
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "a reader went away and pie exited {:?} rather than 0 - `pie … | head -1` calls this a \
         failure for what is success. stderr: {stderr}",
        output.status.code()
    );
}

/// The paired control that stops the test above from being vacuous.
///
/// A binary that printed nothing at all would also exit 0 with a closed pipe. This asserts that
/// the same command *does* produce its table when its reader stays, so "exit 0" above is a
/// statement about the closed pipe rather than about a run that did nothing.
#[test]
fn the_same_command_writes_its_table_when_the_reader_stays() {
    let home = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_pie"))
        .args(["--list-backends"])
        .env("PIE_HOME", home.path())
        .output()
        .expect("failed to run the pie binary");

    assert!(
        output.status.success(),
        "`pie --list-backends` failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("agy") && stdout.contains("codex"),
        "the control must show the command producing output, or the closed-pipe test above \
         proves nothing about a command that writes: {stdout:?}"
    );
}
