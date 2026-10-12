//! Drawing: one screen, from one [`App`], with no state of its own.
//!
//! `DESIGN.md` §12. `Frame` is ratatui's and appears in exactly one signature in
//! this crate — here. Everything this module needs is already in [`App`], which
//! is what lets every assertion be made against an in-memory buffer through
//! ratatui's `TestBackend` instead of against a terminal.

use ratatui::Frame;

use super::app::App;

/// Draw the whole screen.
///
/// Frozen signature. The layout this must produce, and the reason for each part
/// (`ACCEPTANCE.md` §9, stage B):
///
/// * the transcript, scrolled by [`App::scroll`], where a long conversation
///   scrolls rather than overflowing;
/// * the prompt line, showing [`App::prompt`] and [`App::cursor`];
/// * a status line naming [`App::target`], [`App::session`], [`App::status`] and
///   the most recent [`App::last_usage`];
/// * the model picker overlay when [`App::picker`] is open, listing every row and
///   marking the highlighted one;
/// * [`VENDOR_TOOL_NOTE`] beside tool steps, and never the implication that pie
///   ran them.
///
/// `render` takes `&App`, not `&mut App`: it must be impossible for drawing to
/// change state, so that a screenshot cannot be the thing that moves the
/// conversation on.
pub fn render(_frame: &mut Frame, _app: &App) {
    // SCAFFOLD — stage B replaces this body with the layout above and asserts it
    // in `tests/tui_render.rs` through `TestBackend`. It draws nothing rather
    // than panicking, and the signature is the frozen part: A and C are written
    // against this call, not against its current body.
}
