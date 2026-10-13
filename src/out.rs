//! Stdout and stderr that do not panic when the reader goes away.
//!
//! `println!` and `print!` panic on **any** write failure. Rust ignores `SIGPIPE`, so a write
//! to a pipe whose reader has gone returns `EPIPE` (Windows: `ERROR_BROKEN_PIPE`) instead of
//! killing the process the way a C program dies, and the macro turns that into a panic:
//! `pie -p … | head -1` exited **101** with a broken-pipe message on stderr.
//!
//! That is a panic on external input, which invariant **I6** says cannot happen — and the lint
//! that is supposed to enforce I6, `deny(clippy::panic)` in `lib.rs`, is structurally unable to
//! see it, because the panic is generated inside the standard library's macro rather than
//! written in this crate. A rule whose checker cannot observe the violation is the exact shape
//! this repository kept finding across M6–M11, so the fix is not a better lint: it is to make
//! the violating construct unavailable on every path that can write to a stream a user closes.
//!
//! # What this does instead, and what it gives up
//!
//! A failed write is **ignored**, and the process exits 0. That is the conventional behaviour
//! for the head of a pipeline: `head` closed the pipe because it already had what it wanted, and
//! reporting a failure there would be reporting an error for success.
//!
//! The cost is stated rather than glossed: a *genuine* write failure — a full disk, a reader
//! that closed without reading what mattered — is also silent. That trade is worth taking here
//! and only here, because the sole consumer of this output is a terminal or a pipe, and neither
//! channel can carry a report of its own failure back to anyone. A program with a log file or a
//! protocol client on the other end of its stdout would need the opposite choice, which is why
//! `mcp.rs` writes its frames with `writeln!` on a handle it owns and checks: a dropped JSON-RPC
//! frame is not the same kind of loss as a dropped table row.

use std::io::Write;

/// Write `text` to `sink`, ignoring a failure.
///
/// The `let _ =` is the whole point of this module, so it is a separate function with a name
/// rather than an inline expression: it is the single place the decision is made, and it is the
/// place `a_failed_write_is_ignored_rather_than_panicking` can actually reach with a failing
/// writer.
fn emit<W: Write>(sink: &mut W, text: &str) {
    let _ = sink.write_all(text.as_bytes());
}

/// Write `text` to stdout, ignoring a failure.
///
/// Public rather than `pub(crate)` because the **binary** is a separate crate: `main.rs` can no
/// more reach a crate-private item than any other consumer can, and its `PIE_STRIP_ENV` warning
/// needs exactly this. The alternative was a second `let _ = writeln!(stderr.lock(), …)` written
/// by hand in `main.rs`, which is the drift this repository keeps finding - one decision, two
/// copies, free to disagree. A library that already owns "how pie writes to a stream" should
/// just expose it.
pub fn write(text: &str) {
    emit(&mut std::io::stdout().lock(), text);
}

/// Write `text` to stderr, ignoring a failure.
pub fn write_err(text: &str) {
    emit(&mut std::io::stderr().lock(), text);
}

/// Append the terminator `outln!` promises.
///
/// Split out from the write so it can be asserted on directly: it is the entire difference
/// between `outln!` and `outfrag!`, and the one part of either macro a test can reach without
/// capturing a global stream.
pub(crate) fn terminated(text: &str) -> String {
    let mut line = String::with_capacity(text.len() + 1);
    line.push_str(text);
    line.push('\n');
    line
}

/// [`println!`], but a failed write loses a line instead of the process.
///
/// A drop-in for `println!`: the argument list is handed straight to `format!`, so every form
/// the macro accepts - `"{}"` and `"{x}"` captures, width parameters - keeps working. That is
/// deliberate, because a fix that forced 80 call sites to be rewritten by hand would be a much
/// larger change than the defect, and larger changes to working code are where new defects
/// come from.
///
/// The zero-argument arm is separate because `format!()` is **not** valid - it requires at
/// least a format string - even though `println!()` is. Three call sites are a bare `outln!()`
/// printing a blank line, so the arm is load-bearing rather than tidiness.
macro_rules! outln {
    () => {
        $crate::out::write(&$crate::out::terminated(""))
    };
    ($($arg:tt)*) => {
        $crate::out::write(&$crate::out::terminated(&format!($($arg)*)))
    };
}

/// [`print!`], but a failed write loses text instead of the process.
macro_rules! outfrag {
    () => {
        $crate::out::write("")
    };
    ($($arg:tt)*) => {
        $crate::out::write(&format!($($arg)*))
    };
}

/// [`eprintln!`], but a failed write loses a line instead of the process.
///
/// stderr is included on purpose. `pie -p … 2>&1 | head -1` closes stderr just as readily as
/// stdout, and a diagnostic that panics the process is worse than a diagnostic that is lost -
/// particularly for the `PIE_STRIP_ENV` warning, which exists precisely to be noticed in an
/// unusual setup and would otherwise kill the run it is warning about.
macro_rules! errln {
    () => {
        $crate::out::write_err(&$crate::out::terminated(""))
    };
    ($($arg:tt)*) => {
        $crate::out::write_err(&$crate::out::terminated(&format!($($arg)*)))
    };
}

// `pub(crate)` and not `pub`: a `macro_rules!` macro cannot be re-exported outside the crate
// without `#[macro_export]`, which would relocate all three to the crate root. The binary is a
// separate crate, so it calls `write`/`write_err` directly instead - the same two functions these
// macros expand to, so there is still exactly one place the error is swallowed.
pub(crate) use {errln, outfrag, outln};

#[cfg(test)]
mod tests {
    use super::*;

    /// A sink that fails every write the way a closed pipe does.
    struct ClosedPipe;

    impl Write for ClosedPipe {
        fn write(&mut self, _buffer: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "the reader went away",
            ))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// The regression test for **S2**, at the layer that can be driven directly.
    ///
    /// It would fail if `emit` stopped swallowing the error — a `?`, an `unwrap`, or a
    /// `println!` swapped back in — which is the only way the panic can return. The companion
    /// process-level test (`tests/acceptance.rs`) proves the *wiring*: that the binary at the
    /// head of a pipeline actually exits 0. Neither half is sufficient alone, because this one
    /// cannot see a call site that bypasses the helper, and that one cannot make the failure
    /// happen on demand.
    #[test]
    fn a_failed_write_is_ignored_rather_than_panicking() {
        let mut sink = ClosedPipe;
        emit(&mut sink, "a line that cannot be written\n");
        emit(&mut sink, "");
    }

    /// The macros are the surface the call sites actually use, so exercise every argument form
    /// that appears in the tree. This is a compile-time assertion first - a macro that stopped
    /// accepting `"{x}"` captures or a no-argument call would fail to build here rather than at
    /// the call site - and a runtime one second, because each call goes through the same
    /// ignore-the-error path the real output uses.
    ///
    /// Deliberately **not** an assertion about the text: capturing this process's own stdout
    /// from inside libtest is not possible with `std`, and a test that asserted `format!`
    /// behaviour while claiming to test `outln!` would be a claim with nothing behind it.
    /// `terminated` is where the text can be checked, and it is checked above.
    #[test]
    fn every_argument_form_in_the_tree_compiles_and_does_not_panic() {
        outln!();
        outln!("literal");
        outln!("{}", "positional");
        outln!("{value}", value = 12);
        outln!("{:<5}|", "ab");
        outln!("{:>7}  {}", "KIND", "BYTES");
        outfrag!("{}", "no terminator");
        errln!("{}", "to stderr");
    }

    /// The entire difference between `outln!` and `outfrag!`, asserted where it can be reached.
    #[test]
    fn outln_adds_exactly_the_terminator_and_nothing_else() {
        assert_eq!(terminated("x"), "x\n");
        assert_eq!(
            terminated(""),
            "\n",
            "a bare `outln!()` is a blank line; three call sites rely on that"
        );
        assert_eq!(
            terminated("already\n"),
            "already\n\n",
            "the macro never inspects its text, so it cannot double-guess one"
        );
    }
}
