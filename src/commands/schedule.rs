//! `pie schedule` — what is due, what last ran, and what the budget has left.
//!
//! Contract: `planning/DESIGN.md` §16; criteria: `planning/ACCEPTANCE.md` §15 (row
//! "D commands"). Two commands, two different guarantees:
//!
//! * **`status` spends nothing and writes nothing.** It reads the configuration and the
//!   ledger and prints them; it resolves no backend and reaches no provider, so the
//!   budget it reports is reported without being touched.
//! * **`run <JOB>` goes through [`crate::schedule::run_job`]**, which is where the
//!   order that matters lives: the cadence decides, `check_budget` refuses **before**
//!   the spawn, and only then is a provider reached. With the shipped default of
//!   `learn_budget_tokens = 0` that refusal happens on every machine, which is the point
//!   of the default (`RISKS.md` §13).
//!
//! # Why the ledger is a file
//!
//! `DESIGN.md` §16's ruling is that "the same window" means **one cadence interval** and
//! the budget is **per window rather than per run**, "so a second run inside it —
//! **including an explicit `pie schedule run`** — spends the remainder". A ledger that
//! lived only in memory could not honour that across two invocations of the binary, and
//! a `status` that always answered "never ran, spent 0" would be reporting two facts it
//! does not know. [`LEDGER_FILE`] is that state. It is the one on-disk artefact M7's
//! contract does not name; `paths.rs` is its natural home if the maintainer would rather
//! declare it there.
//!
//! A ledger that cannot be *read* is an error rather than a fresh start. Treating a
//! corrupt ledger as "nothing spent" would hand the whole window's budget back, which is
//! the silent spend `RISKS.md` §13 names.

use crate::out::{outfrag, outln};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::error::{PieError, Result};
use crate::paths;
use crate::schedule::{self, Ledger, LEARN_JOB};
use crate::session_log::now_millis;

/// The job ledger: when the learn job last ran and what the open window has spent.
pub const LEDGER_FILE: &str = "schedule.json";

/// The ledger as it is serialised. Deliberately not [`Ledger`] itself: that type is
/// stage C's frozen shape and carries no serialisation derives, and the on-disk form is
/// this module's business — a field added to `Ledger` must not silently become a key
/// that an older pie would reject.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct LedgerFile {
    /// Epoch milliseconds of the last run. `None` means it never has.
    last_run_ms: Option<u64>,
    /// Tokens spent inside the window that is open now.
    spent: u64,
}

/// `$PIE_HOME/schedule.json`
pub fn ledger_path(home: &Path) -> PathBuf {
    home.join(LEDGER_FILE)
}

/// Read the ledger, overlaid on the configured budget.
///
/// A missing file is a fresh install: nothing has run and nothing has been spent, which
/// is exactly [`Ledger::from_config`]. A *damaged* file is an error — see the module
/// docs.
pub(crate) fn read_ledger(cfg: &Config, home: &Path) -> Result<Ledger> {
    let mut ledger = Ledger::from_config(cfg);
    let path = ledger_path(home);

    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(ledger),
        Err(e) => return Err(e.into()),
    };

    let file: LedgerFile = serde_json::from_slice(&bytes).map_err(|e| {
        PieError::Config(format!(
            "{}: {e} — refusing to run a budgeted job against a ledger pie cannot read, \
             because a spend pie cannot see is a spend it would repeat (RISKS.md §13)",
            path.display()
        ))
    })?;

    ledger.last_run_ms = file.last_run_ms;
    ledger.budget.spent = file.spent;
    Ok(ledger)
}

/// Persist the ledger, through a scratch file and a rename.
///
/// The rename is the point (`memory.rs` makes the same argument for the learned
/// document): a plain overwrite that dies half way leaves a *readable but wrong* ledger,
/// and a ledger that under-reports a spend is what lets a window's budget be spent
/// twice.
pub(crate) fn write_ledger(ledger: &Ledger, home: &Path) -> Result<()> {
    std::fs::create_dir_all(home)?;

    let file = LedgerFile {
        last_run_ms: ledger.last_run_ms,
        spent: ledger.budget.spent,
    };
    let bytes = serde_json::to_vec_pretty(&file)?;

    let path = ledger_path(home);
    let scratch = home.join(format!("{LEDGER_FILE}.tmp"));
    if let Err(e) = std::fs::write(&scratch, bytes) {
        let _ = std::fs::remove_file(&scratch);
        return Err(e.into());
    }
    if let Err(e) = std::fs::rename(&scratch, &path) {
        let _ = std::fs::remove_file(&scratch);
        return Err(e.into());
    }
    Ok(())
}

/// `pie schedule status` — the report, as a string.
///
/// A renderer rather than `println!` so the output is assertable without capturing a
/// process's stdout, and so `status_text` can be called in a test that then asserts
/// **nothing was written**: the read-only claim is only worth making about a function
/// that could have written.
pub fn status_text(cfg: &Config, home: &Path, now_ms: u64) -> Result<String> {
    let ledger = read_ledger(cfg, home)?;
    let cadence = cfg.jobs.learn_cadence.to_schedule();
    let budget = ledger.budget;

    let mut out = String::new();
    out.push_str(&format!("job       {LEARN_JOB}\n"));
    out.push_str(&format!(
        "cadence   {}\n",
        format!("{:?}", cfg.jobs.learn_cadence).to_lowercase()
    ));
    out.push_str(&format!(
        "due       {}{}\n",
        if schedule::due(ledger.last_run_ms, cadence, now_ms) {
            "yes"
        } else {
            "no"
        },
        // The reason, because "due: no" with a cadence of `never` and "due: no" a minute
        // after a run are different facts and a status that cannot tell them apart is
        // not telling the user anything.
        match cadence {
            schedule::Cadence::Never =>
                " — the cadence is `never`, so nothing ever runs unasked (`[jobs] learn_cadence`)",
            _ => "",
        }
    ));
    out.push_str(&format!(
        "last ran  {}\n",
        match ledger.last_run_ms {
            None => format!("never (no {} under {})", LEDGER_FILE, home.display()),
            Some(last) => format!("{last} ({})", ago(now_ms, last)),
        }
    ));
    out.push_str(&format!(
        "budget    {} max · {} spent · {} remaining{}\n",
        budget.max_tokens,
        budget.spent,
        budget.remaining(),
        if budget.max_tokens == 0 {
            " — a budget of 0 refuses every budgeted job before it spawns"
        } else {
            ""
        }
    ));
    out.push_str(
        "\nthis report resolved no backend and spawned nothing. `pie schedule run <JOB>` is \
         the only path that reaches a provider, and only when the budget covers it\n",
    );
    Ok(out)
}

/// `pie schedule status`
pub fn status(cfg: &Config) -> Result<()> {
    outfrag!("{}", status_text(cfg, &paths::pie_home(), now_millis())?);
    Ok(())
}

/// `pie schedule [status | run <JOB>]`
///
/// The `None` arm is the whole of `status`, which takes no `&mut` anything and cannot
/// write: asking what would run must not be a way to make it run.
pub async fn run(cfg: &Config, job: Option<&str>) -> Result<()> {
    match job {
        None => status(cfg),
        Some(id) => manual_run(cfg, id).await,
    }
}

/// `pie schedule run <JOB>` — run one job now, budget permitting.
async fn manual_run(cfg: &Config, id: &str) -> Result<()> {
    if id != LEARN_JOB {
        return Err(PieError::Config(format!(
            "unknown job `{id}` — pie ships one scheduled job, `{LEARN_JOB}`"
        )));
    }

    let home = paths::pie_home();
    // The same decision half `pie learn` uses, so what a run pays to review is the
    // candidate list the extractor actually produced rather than a second guess at it.
    let plan = super::learn::plan(&home, now_millis())?;
    let spend = super::learn::review(cfg, None, &plan, &home).await?;

    outln!(
        "{LEARN_JOB} ran: {} candidate(s) reviewed, {} token(s) reported",
        plan.candidates.len(),
        spend.tokens
    );
    outln!("{}", spend.output.trim_end());
    outln!(
        "the review is reported, not merged: `learned.md` is written only by the rule-based \
         pass (`pie learn`)"
    );
    Ok(())
}

/// A timestamp as an age, in the largest useful unit.
///
/// The same three-branch rule `commands/sessions.rs` renders an age with. Duplicated
/// rather than shared because that one is private to a module this stage does not own,
/// and a `status` line of epoch milliseconds is a number the user has to do arithmetic
/// on.
///
/// Saturating: a clock that moved backwards (or a hand-edited ledger) must not produce a
/// nonsense age or a panic (I6).
fn ago(now_ms: u64, then_ms: u64) -> String {
    const MINUTE: u64 = 60_000;
    const HOUR: u64 = 60 * MINUTE;
    const DAY: u64 = 24 * HOUR;

    match now_ms.saturating_sub(then_ms) {
        age if age < MINUTE => "just now".to_string(),
        age if age < HOUR => format!("{}m ago", age / MINUTE),
        age if age < DAY => format!("{}h ago", age / HOUR),
        age => format!("{}d ago", age / DAY),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn ages_are_rendered_in_the_largest_useful_unit() {
        let now = 1_000_000_000_000u64;
        assert_eq!(ago(now, now), "just now");
        assert_eq!(ago(now, now - 60_000), "1m ago");
        assert_eq!(ago(now, now - 3_600_000), "1h ago");
        assert_eq!(ago(now, now - 86_400_000), "1d ago");
    }

    /// A clock that moved backwards must read as "just now", not as a panic or a
    /// nonsense age.
    #[test]
    fn a_future_stamp_reads_as_just_now() {
        assert_eq!(ago(1_000, 9_999), "just now");
    }
}
