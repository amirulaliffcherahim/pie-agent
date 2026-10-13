//! `pie auth` — per-backend auth state.
//!
//! Runs **no subprocess and no model call**: it inspects each vendor CLI's own
//! credential store on disk (`auth.rs`), so it is free and instant.
//!
//! pie never stores, logs, or prints a token. This command reports only *which
//! kind* of credential exists — subscription, API-key-only, absent.

use crate::config::Config;
use crate::error::Result;
use crate::out::outln;
use crate::provider;

const ID_WIDTH: usize = 9;
const AUTH_WIDTH: usize = 15;

/// `pie auth`
pub async fn run(cfg: &Config) -> Result<()> {
    outln!("{:<ID_WIDTH$} {:<AUTH_WIDTH$} DETAIL", "BACKEND", "AUTH");

    for p in provider::registry() {
        let auth = p.auth_status(cfg).await;
        outln!(
            "{:<ID_WIDTH$} {:<AUTH_WIDTH$} {}",
            p.id(),
            auth.label(),
            auth.detail().unwrap_or_default()
        );
    }

    outln!();
    outln!("pie never stores tokens; auth comes from each CLI's own login.");
    outln!("Run `codex login`, or launch `agy` / `claude`, to authenticate.");
    Ok(())
}
