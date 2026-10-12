//! `pie profile` — show the assembled persona projection and its provenance.
//!
//! This is the command that makes the M3 contract inspectable: what the user
//! actually wrote, how it was rendered, and which mechanism each backend uses to
//! receive it. It answers "why did the model behave like that?" without spending
//! a turn.
//!
//! Zero-token and zero-side-effect **by construction**: this module has no path
//! to a spawn (it never names [`crate::process`] or a provider's `stream`), writes
//! no session row, and the only work it does is reading pie's own files — plus
//! each backend's `Capabilities`, which is a compile-time fact, not a probe.
//! `tests/profile.rs` asserts both halves: a counting stand-in CLI that must
//! never be invoked, and a static check that this file cannot spawn anything.

use crate::error::Result;
use crate::profile::Profile;
use crate::provider;
use crate::provider::agy;

/// Width of the left-hand column in the two tables.
const LABEL_WIDTH: usize = 12;

/// `pie profile`
pub fn run() -> Result<()> {
    let profile = Profile::from_home()?;

    println!("{:<LABEL_WIDTH$} {}", "home", profile.root().display());

    if profile.is_empty() {
        println!(
            "{:<LABEL_WIDTH$} empty (no {}, {} or {}/ is configured)",
            "projection",
            crate::paths::PROFILE_FILE,
            crate::paths::AGENTS_FILE,
            crate::paths::MEMORY_DIR
        );
    } else {
        println!(
            "{:<LABEL_WIDTH$} {} bytes from {} source(s)",
            "projection",
            profile.bytes(),
            profile.contributions().len()
        );
    }

    println!();
    println!("{:<LABEL_WIDTH$} {:>7}  SOURCE", "KIND", "BYTES");
    if profile.contributions().is_empty() {
        println!("{:<LABEL_WIDTH$} {:>7}  -", "none", 0);
    }
    for contribution in profile.contributions() {
        println!(
            "{:<LABEL_WIDTH$} {:>7}  {}",
            contribution.kind.label(),
            contribution.bytes,
            contribution.source.display()
        );
    }

    // The mechanism is the other half of M3: the same bytes, three ways in.
    println!();
    println!("MECHANISM (DESIGN §10)");
    for backend in provider::registry() {
        let how = if backend.capabilities().system_prompt {
            "--append-system-prompt <projection>"
        } else {
            "folded before the prompt text"
        };
        println!("{:<LABEL_WIDTH$} {how}", backend.id());
    }

    println!();
    println!("--- projection ({} bytes) ---", profile.bytes());
    if let Some(persona) = profile.persona() {
        println!("{persona}");
    }

    // Reported here because this is the moment the user can act on it: on agy
    // the projection competes with the task for the same argv budget, so a
    // projection larger than the whole budget makes every agy turn a
    // `PromptTooLong` (DESIGN §10). pie will not truncate the persona to fit, so
    // saying so is the only useful thing it can do. stderr, because stdout is
    // the report.
    if profile.bytes() > agy::PROMPT_ARGV_CAP {
        eprintln!(
            "pie: WARNING: the projection is {} bytes and agy's prompt argv cap is {} chars, so \
             no agy turn can fit it. Shorten `{}`/`{}`/`{}/`; pie never truncates a persona.",
            profile.bytes(),
            agy::PROMPT_ARGV_CAP,
            crate::paths::PROFILE_FILE,
            crate::paths::AGENTS_FILE,
            crate::paths::MEMORY_DIR
        );
    }

    Ok(())
}
