//! The interactive TUI (`DESIGN.md` §12).
//!
//! Three layers, split so that the one part which cannot be automated stays as
//! small as possible:
//!
//! | Layer | Testable how |
//! |---|---|
//! | [`app`] — reducer and state | unit tests: a neutral [`app::Input`] reaches no crossterm type, so `key → Action → state` is asserted without a terminal |
//! | [`render`] | ratatui's `TestBackend`: draw into an in-memory buffer and assert its contents |
//! | this module's [`run_session`] | **not automatable.** Kept thin on purpose: crossterm setup, a select loop, and teardown — nothing else |
//!
//! ## Two constraints that shape the behaviour
//!
//! * **Delegate mode means the UI shows the vendor's tool steps, not pie's.**
//!   `agy --print` and `codex exec` run their own tools (`RISKS.md` §1), so this
//!   is architecturally unlike a TUI that displays its own tool cards, and the UI
//!   says so ([`app::VENDOR_TOOL_NOTE`]) rather than implying a capability pie
//!   does not have.
//! * **The UI never blocks on a turn.** A turn runs as a task and pushes events
//!   down an `mpsc` channel; the loop selects over that channel and crossterm's
//!   event stream. A synchronous turn would freeze the interface until the vendor
//!   CLI returned — up to ~40 s on agy.
//!
//! ## Entry point
//!
//! `pie tui`, opt-in. A bare `pie` keeps printing help, so CI and every existing
//! test are unaffected and the TUI can never be entered by accident from a script.

pub mod app;
pub mod render;

pub use app::VENDOR_TOOL_NOTE;
pub use app::{apply_event, reduce, Action, App, Entry, Input, Picker, PickerItem, Status};
pub use render::render;

use crate::config::Config;
use crate::error::{PieError, Result};

/// Run the interactive session until the user quits.
///
/// Frozen signature (`DESIGN.md` §12). This is the only part of M6 that no test
/// can reach, so it must contain nothing worth testing:
///
/// 1. refuse to start unless stdout **and** stdin are terminals, so `pie tui` in
///    a pipe fails with a clear message instead of emitting escape codes into it
///    (`ACCEPTANCE.md` §9);
/// 2. put the terminal into raw mode and enter the alternate screen;
/// 3. loop: select over the crossterm event stream and the turn channel, map a
///    `KeyEvent` to an [`app::Input`] and call [`app::reduce`], act on the
///    returned [`app::Action`], and call [`render`] after each change;
/// 4. restore the terminal on the way out — including on an error, which is why
///    the teardown is not simply the last statement.
///
/// Every branch inside the loop must be a call into a tested function. Anything
/// that grows a decision here is a decision no gate can see.
pub async fn run_session(_app: &mut App, _cfg: &Config) -> Result<()> {
    // SCAFFOLD — stage C replaces this body with the loop above and asserts the
    // channel half in `tests/tui_loop.rs`.
    //
    // It returns an error rather than `Ok(())` on purpose: a stub that reported
    // success would let `pie tui` exit 0 having drawn nothing, which is exactly
    // the "gate that cannot fail" shape `ACCEPTANCE.md` §1 exists to prevent. A
    // named refusal cannot be mistaken for a working TUI.
    Err(PieError::Unsupported {
        backend: "tui".to_string(),
        feature: "the interactive terminal loop has not been wired up yet (M6 stage C)".to_string(),
    })
}
