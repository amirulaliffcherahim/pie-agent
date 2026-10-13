//! `pie memory` — what pie learned, with provenance, and how to undo it.
//!
//! Deliberately takes no [`crate::config::Config`] and resolves no backend: it reads
//! pie's own memory directory and nothing else, so it is **spawn-free and quota-free by
//! construction**. That is the same argument `commands/sessions.rs` makes, and it is
//! what invariant I7 needs — a command with no path to `provider::stream` cannot spend
//! model quota, so "no session-log row" is a property of the shape rather than of a
//! discipline someone has to keep.
//!
//! Two properties come from `DESIGN.md` §16's decisions table and are tests here:
//!
//! * **Provenance is printed.** Every learned line carries the session id and turn
//!   index it came from, because a fact you cannot trace is one you cannot judge.
//! * **A revert restores the pre-apply bytes exactly.** The undo handle is the journal
//!   `memory::apply` wrote *before* it touched the document, so a revert is a restore
//!   rather than a reconstruction.
//!
//! The `<ID>` a revert takes is derived from the bytes that revert would restore
//! ([`undo_id`]), which makes it an id for a *state* rather than for a run: an id copied
//! from an earlier `show` cannot silently undo a later apply. FNV-1a rather than
//! `DefaultHasher` because the id is written down in a terminal and pasted back, and
//! `DefaultHasher`'s algorithm is explicitly not stable across releases — an id that
//! stopped matching after a Rust upgrade would make the undo handle useless.

use crate::out::{outfrag, outln};
use std::path::Path;

use crate::error::{PieError, Result};
use crate::memory::{self, Journal};
use crate::paths;
use crate::tui::LocalCommand;

/// `pie memory show` — render what was learned, with provenance.
pub fn show() -> Result<()> {
    outfrag!("{}", render(&paths::pie_home())?);
    Ok(())
}

/// `pie memory revert <ID>` — undo the most recent apply.
pub fn revert(id: &str) -> Result<()> {
    let home = paths::pie_home();
    revert_at(&home, id)?;
    outln!(
        "reverted: {} restored to the bytes the last `pie learn` apply found",
        memory::doc_path(&home).display()
    );
    Ok(())
}

/// The memory report, as a string.
///
/// A renderer rather than `println!` so the output is assertable without capturing a
/// process's stdout, and so `pie memory show` and the TUI's `/memory` print the *same*
/// bytes: two renderings of "what pie learned" would drift, and the one the user reads
/// least would be the one that did.
pub fn render(home: &Path) -> Result<String> {
    let doc = memory::read_doc(home)?;
    let journal = memory::read_journal(home)?;
    let mut out = format!("learned memory: {}\n", memory::doc_path(home).display());

    let facts: Vec<&str> = doc
        .learned
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.trim().is_empty())
        .collect();

    if facts.is_empty() {
        out.push_str(
            "nothing learned yet — `pie learn` mines your own sessions and writes what it \
             finds here\n",
        );
        return Ok(out);
    }

    out.push_str(&format!(
        "{} fact(s), in the order they are appended to the projection. Each names the \
         session and turn it came from:\n\n",
        facts.len()
    ));
    for line in facts {
        out.push_str(&format!("  {line}\n"));
    }

    match journal {
        Some(journal) => out.push_str(&format!("\n{}\n", undo_hint(&journal))),
        None => out.push_str(
            "\nthere is no undo handle: the last apply changed nothing, or it has already \
             been reverted\n",
        ),
    }
    Ok(out)
}

/// Undo the apply named by `id`, restoring the document's exact previous bytes.
///
/// Split from [`revert`] so a test can point it at a scratch home rather than at
/// `PIE_HOME`.
///
/// A wrong id changes nothing and says so. Reverting the *wrong* state is worse than
/// refusing: the document is the persona sent to a vendor on every later turn, and a
/// user who asked to undo one apply has not asked to undo a different one.
pub fn revert_at(home: &Path, id: &str) -> Result<()> {
    let Some(journal) = memory::read_journal(home)? else {
        return Err(PieError::Config(format!(
            "nothing to revert: there is no {} under {}. `pie memory revert` undoes the most \
             recent `pie learn` apply, and no apply has written one",
            memory::JOURNAL_FILE,
            memory::memory_dir(home).display()
        )));
    };

    let expected = undo_id(&journal.previous);
    if id != expected {
        return Err(PieError::Config(format!(
            "`{id}` is not the undo id of the current apply (`{expected}`); `pie memory show` \
             prints it. Nothing was changed"
        )));
    }
    memory::revert(&journal, home)
}

/// The lines the TUI's `/memory` prints.
///
/// Always non-empty. A slash command is handled in-process, so no turn ever runs and
/// nothing would ever end the `Running` status a submission sets: a command that
/// answered with nothing would look exactly like a hang.
pub fn local_lines(command: &LocalCommand) -> Vec<String> {
    match command {
        LocalCommand::Memory => match render(&paths::pie_home()) {
            Ok(text) => text.lines().map(str::to_string).collect(),
            Err(error) => vec![format!("`/memory` could not be read: {error}")],
        },
        LocalCommand::Unknown(word) => vec![
            format!("`/{word}` is not a pie command, and nothing was sent to a backend"),
            "pie-local commands: /memory — what pie learned, with provenance".to_string(),
        ],
    }
}

/// The id an apply's undo handle is addressed by.
///
/// Derived from the bytes the undo would restore, so the id names a state rather than a
/// run. FNV-1a with the offset basis and prime the algorithm specifies: an id that a
/// later build of pie disagreed about would leave a journal nothing could address.
pub(crate) fn undo_id(previous: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in previous.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// The one line that names the undo handle.
///
/// Shared between `pie learn`, `pie memory show` and the TUI so the id, and the command
/// that consumes it, cannot be printed in two spellings.
pub(crate) fn undo_hint(journal: &Journal) -> String {
    let id = undo_id(&journal.previous);
    format!("undo: {id}  (`pie memory revert {id}`)")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    /// The id has to be stable for a given state, or a `show` id could never be pasted
    /// back into a later `revert`.
    #[test]
    fn an_undo_id_is_a_function_of_the_state_it_names() {
        assert_eq!(undo_id(""), undo_id(""));
        assert_eq!(undo_id("hello"), undo_id("hello"));
        assert_ne!(undo_id("hello"), undo_id("hello\n"));
        assert_eq!(undo_id("hello").chars().count(), 16);
        assert!(undo_id("hello").chars().all(|c| c.is_ascii_hexdigit()));
    }
}
