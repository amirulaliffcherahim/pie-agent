//! `pie usage` — backend quota plus pie's own session-log totals.
//!
//! Two independent sources, deliberately shown separately (`PLAN.md` §6):
//!
//! * **quota buckets** from the backend itself (a CLI slash expansion or an
//!   HTTP probe — no model tokens),
//! * **pie's session log totals**, read from `~/.pie/usage.jsonl`, so the
//!   numbers come from stream events already recorded rather than from
//!   re-reading session transcripts (`PLAN.md` §9).

use crate::config::Config;
use crate::error::{PieError, Result};
use crate::paths;
use crate::provider;
use crate::report;
use crate::session_log;

/// Milliseconds in a day, for the rolling windows.
const DAY_MS: u64 = 24 * 60 * 60 * 1_000;

/// `pie usage [backend]`
pub async fn run(cfg: &Config, backend: Option<&str>) -> Result<()> {
    if let Some(id) = backend {
        print_quota(cfg, id).await?;
        println!();
    }

    print_session_totals(backend)
}

/// The backend's own quota buckets.
async fn print_quota(cfg: &Config, id: &str) -> Result<()> {
    let provider = provider::by_id(id).ok_or_else(|| PieError::UnknownBackend {
        backend: id.to_string(),
        known: super::models::known_backends(),
    })?;

    match provider.usage(cfg).await {
        Ok(report) => println!("{}", report::render_report(&report)),
        // Not implemented yet is not an error for a pre-M1/M2 backend.
        Err(PieError::Unsupported { feature, .. }) => {
            println!(
                "{}: quota probe not implemented yet — {feature}",
                provider.id()
            );
        }
        Err(e) => return Err(e),
    }

    Ok(())
}

/// Rolling windows over the append-only session log.
fn print_session_totals(backend: Option<&str>) -> Result<()> {
    let rows = session_log::read_all(&paths::usage_log())?;
    let rows = report::rows_for_backend(&rows, backend);

    let now = session_log::now_millis();

    for (label, window) in [
        ("all time", rows.clone()),
        (
            "last 7 days",
            report::rows_since(&rows, now.saturating_sub(7 * DAY_MS)),
        ),
        (
            "last 30 days",
            report::rows_since(&rows, now.saturating_sub(30 * DAY_MS)),
        ),
    ] {
        println!("{}", report::render_totals(label, &report::totals(&window)));
        println!();
    }

    if backend.is_none() {
        println!("pass a backend (`pie usage agy`) for its quota buckets.");
    }

    Ok(())
}
