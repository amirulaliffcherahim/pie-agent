//! `pie tui` — the interactive session (`DESIGN.md` §12).
//!
//! Opt-in, and the only command that takes over the terminal: a bare `pie` keeps
//! printing help, so CI and every existing test are unaffected, and nothing that
//! runs today can enter the TUI by accident.
//!
//! What this file does is small on purpose. It resolves the backend and the model
//! with exactly the two functions `pie -p` uses (`provider::resolve_target`,
//! `provider::by_id`), so a misspelt `--model` or a bad `general.default_backend`
//! fails with the same message and the same exit code as the one-shot path — and
//! fails *before* the terminal is put into raw mode, so the error is readable on a
//! normal screen. It then mints the session, builds [`App`] and hands both to
//! `tui::run_session`.
//!
//! The persona is **not** loaded here. `tui::run_session` reads
//! `Profile::from_home()` per turn and builds the request through
//! `tui::build_request`, which takes the same projection, through the same
//! accessor, that `commands/print.rs` hands to every backend — M3's Done condition
//! is that agy and codex receive byte-identical personas, and a TUI that assembled
//! its own would falsify it silently.

use crate::config::Config;
use crate::error::{PieError, Result};
use crate::provider;
use crate::tui::{self, App};

/// `pie tui`
pub async fn run(cfg: &Config) -> Result<()> {
    // Which backend and model, resolved identically to `pie -p` so the TUI opens on
    // the conversation the user's configuration already selects. `resolve_target`
    // does not consult `--continue`/`--session`: a TUI run is its own conversation,
    // deliberately, because auto-resuming would silently answer in the wrong
    // context (`DESIGN.md` §11).
    let (backend, model) = provider::resolve_target(cfg, None)?;

    // The same check `pie -p` makes, for the same reason and before the same
    // irreversible step: an unknown backend must be reported on a normal terminal.
    // The provider returned here is dropped because `tui::run_turn` resolves one per
    // turn from `App::backend` — the picker can change the backend mid-session — so
    // the honest way to read this call is "is this backend one we can drive?".
    if provider::by_id(&backend).is_none() {
        return Err(PieError::UnknownBackend {
            backend: backend.clone(),
            known: super::models::known_backends(),
        });
    }

    // One session per TUI run, minted before the loop starts so the id the status
    // line shows exists from the first frame. Minting writes nothing: the session
    // appears in `pie sessions` only once a turn has been recorded against it
    // (`DESIGN.md` §11).
    let session = tui::begin_session(&backend);
    let mut app = App::new(backend, model, session.id());

    tui::run_session(&mut app, cfg).await
}
