//! Gate for M12 stage C: **the default reverses, and the safety property moves**
//! (`planning/ACCEPTANCE.md` §20, row C; `planning/DESIGN.md` §21.1, §21.7).
//!
//! Row C is: *"bare `pie` **on a pipe** prints help to stdout, exits 0, and its stdout
//! contains **no `ESC` byte**; and the decision to enter the TUI is a **pure function**
//! asserted in both directions, because the terminal branch itself cannot be exercised
//! without a pty"*.
//!
//! ## Two halves, and one passing does not imply the other
//!
//! M6's guarantee was *"the TUI is not the default, so no test and no CI job can enter
//! it by accident"*. M12's is *"it requires a terminal on both streams, so nothing
//! without one can enter it by accident"*. Same outcome, different mechanism — and the
//! new mechanism is **weaker unless it is asserted**, because the pipe half is now the
//! only thing standing between a CI job and a full-screen interface.
//!
//! | Claim | Test |
//! |---|---|
//! | bare `pie` on a pipe: help on stdout, exit 0, no `ESC` | [`bare_pie_on_a_pipe_prints_help_and_never_starts_a_screen`] |
//! | the decision, in all four directions | [`the_decision_to_enter_the_tui_is_a_pure_function_of_both_streams`] |
//! | the pipe in the test above really is a pipe | [`the_same_pipe_that_gets_help_refuses_an_explicit_tui`] |
//! | `dispatch` consults the decision rather than a copy of it | [`the_bare_arm_consults_the_predicate_rather_than_restating_it`] |
//!
//! The first two are separate on purpose, and `ACCEPTANCE.md` §20's Checks list says why:
//! a pipe run that printed help proves nothing about the predicate, and a predicate that
//! is right proves nothing about what the binary does with it.
//!
//! ## What this gate cannot prove, and does not claim
//!
//! **The terminal branch.** This machine has no pty to allocate, so no test here — and
//! none anywhere in the suite — has ever typed into a bare `pie` and watched the TUI
//! open. What is proven is (a) the binary on a pipe cannot reach it, and (b) the
//! decision that would let it is correct for every combination of the two facts. The
//! fourth test is the weakest of the four and is labelled as such in place: it is a
//! **source-text** assertion, the same shape `S20` recorded for
//! `process::configured_command`, and it catches a wiring mistake rather than a
//! behaviour.
//!
//! Nothing here spawns a vendor CLI, needs a network, or needs a terminal: each run gets
//! a `tempfile::tempdir()` `PIE_HOME`, so an isolated run cannot read or write this
//! machine's real state.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::process::{Command, Stdio};

use pie::commands::enters_tui;

/// The byte `ACCEPTANCE.md` §20 names by name. `ESC` is the first byte of every ANSI
/// sequence a full-screen interface would write into the stream; its presence in stdout
/// is the evidence that something other than text reached the pipe.
const ESC: char = '\u{1b}';

/// Run the real binary with **stdin and stdout both on a pipe** — the exact condition
/// `DESIGN.md` §21.7's second row names — and return `(exit code, stdout, stderr)`.
///
/// Stdin is closed rather than inherited, so the child sees EOF and the test does not
/// depend on how `cargo test` happened to be invoked: under a developer's terminal the
/// harness still hands the child something that is not a terminal, but that is an
/// accident of the harness and an assertion resting on it would be an assertion about
/// `cargo test`.
fn run_on_pipes(args: &[&str], home: &Path) -> (i32, String, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_pie"))
        .args(args)
        .env("PIE_HOME", home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the pie binary starts");
    drop(child.stdin.take());

    let output = child.wait_with_output().expect("pie runs to completion");
    (
        output
            .status
            .code()
            .expect("pie exits normally rather than on a signal"),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// Row C, first half: **the load-bearing one**.
///
/// The help assertion is not a nicety. "No `ESC` byte" alone would pass for a run that
/// printed nothing at all, and "exit 0" alone would pass for a run that printed an
/// escape-free error — so the three are asserted together, on the stream §21.7 names
/// (stdout, not stderr), which is also the form a script consumes.
#[test]
fn bare_pie_on_a_pipe_prints_help_and_never_starts_a_screen() {
    let home = tempfile::tempdir().unwrap();
    let (code, stdout, stderr) = run_on_pipes(&[], home.path());

    assert_eq!(
        code, 0,
        "a bare `pie` on a pipe must exit 0; stderr was: {stderr:?}"
    );
    assert!(
        stdout.contains("Usage:"),
        "the help must arrive on STDOUT, where a script reads it (not on stderr, and not \
         as an empty stream): {stdout:?}"
    );
    assert!(
        !stdout.contains(ESC),
        "no ESC byte may reach the pipe, or a CI log is a terminal dump: {stdout:?}"
    );
    assert!(
        !stderr.contains(ESC),
        "the refusal path emits no escapes either: {stderr:?}"
    );
}

/// Row C, second half: the decision is a pure function, asserted in **both**
/// directions — and in all four cases, because "both directions" for a two-input
/// predicate is four inputs, and the two mixed ones are the halves that matter most:
/// a CI runner has stdout on a pipe with stdin inherited from nowhere, and a pipe
/// into a terminal (`pie | less`) has the other shape.
#[test]
fn the_decision_to_enter_the_tui_is_a_pure_function_of_both_streams() {
    assert!(
        enters_tui(true, true),
        "two terminals is the one case that enters the TUI"
    );
    assert!(
        !enters_tui(true, false),
        "stdout on a pipe must NOT enter the TUI — this is a CI runner's shape"
    );
    assert!(
        !enters_tui(false, true),
        "stdin on a pipe must NOT enter the TUI — this is `pie | less`"
    );
    assert!(
        !enters_tui(false, false),
        "neither stream on a terminal must NOT enter the TUI"
    );

    // Pure, in the literal sense the word is doing here: no second call disagrees, so
    // the answer is a value of these two arguments and not of any state behind them.
    for (stdin, stdout) in [(true, true), (true, false), (false, true), (false, false)] {
        assert_eq!(
            enters_tui(stdin, stdout),
            enters_tui(stdin, stdout),
            "the decision must not vary between two calls with the same input"
        );
    }
}

/// **The control that makes the first test mean something.**
///
/// Two readings of "bare `pie` on a pipe printed help" would pass the test above for the
/// wrong reason: that the pipes were not really pipes, and that this binary refuses
/// everything on a pipe. Driving the same helper with `tui` — the explicit request, the
/// branch M6 already gated — shows the pipes are pipes and that the binary distinguishes
/// the two answers: here it refuses, non-zero, with the message that names the reason.
///
/// This is the paired-control shape `tests/output.rs` records for the same reason — a
/// test that cannot fail and a test that fails for the wrong reason are one defect.
#[test]
fn the_same_pipe_that_gets_help_refuses_an_explicit_tui() {
    let home = tempfile::tempdir().unwrap();
    let (code, stdout, stderr) = run_on_pipes(&["tui"], home.path());

    assert_ne!(
        code, 0,
        "`pie tui` must still refuse on a pipe — the default changed, this did not"
    );
    assert!(
        stderr.contains("terminal"),
        "the refusal must name the reason (M6's message, unchanged): {stderr:?}"
    );
    assert!(
        !stdout.contains(ESC),
        "the refusal emits no escapes into the pipe: {stdout:?}"
    );
    assert!(
        !stdout.contains("Usage:"),
        "the refusal is not help — the two answers must stay distinguishable on a pipe: \
         {stdout:?}"
    );
}

/// **The weakest test here, and labelled as such.** A source-text assertion, not a
/// behaviour.
///
/// The predicate can be perfect while `dispatch` never asks it — and on this machine
/// nothing behavioural would notice: a bare `pie` that *always* printed help passes the
/// pipe test, the predicate test, and the control, because the terminal branch it
/// skipped is the one branch no test can reach. So the wiring is asserted where it
/// exists, in text, exactly as `S20` recorded for `process::configured_command`. It
/// catches a `dispatch` that stopped consulting the decision (or that grew a second copy
/// of it); it cannot catch a `dispatch` that consults it wrongly.
#[test]
fn the_bare_arm_consults_the_predicate_rather_than_restating_it() {
    let source = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/commands.rs"))
        .expect("src/commands.rs is part of the crate");

    assert!(
        source.contains("None if enters_tui("),
        "the bare arm of `dispatch` must select on `enters_tui(...)`; a restated \
         `is_terminal()` inside the arm would be a second copy of the decision, and the \
         two can disagree without any behavioural test on this machine noticing"
    );
    assert_eq!(
        source.matches("fn enters_tui(").count(),
        1,
        "the decision is one function; two definitions would be the same defect as two \
         call sites"
    );
}
