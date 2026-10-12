//! Opt-in, budgeted background jobs (`DESIGN.md` §16, `RISKS.md` §13).
//!
//! `ROADMAP.md`'s cross-cutting list requires background jobs to be **budgeted** and
//! **opt-in**. Prose cannot enforce that, so this module is built so the two can be
//! asserted: [`due`] is a pure function of `(last_run, cadence, now)` so scheduling is
//! testable at its boundary, and [`check_budget`] refuses **before** anything is
//! spawned, so a budget of `0` cannot be "exceeded by one call".
//!
//! The M7/M8 gate that matters: with the **default** config a scheduled run performs
//! **zero** provider spawns, asserted by *count* using the counting-stand-in pattern
//! `tests/cache.rs` established. A job that spawns and then reports is not opt-in.
//!
//! [`run_job`] is the runner those two properties exist for, and the thing it spawns
//! through is a **parameter**: stage D passes a closure that drives a real backend,
//! a test passes one that runs a counting stand-in. That seam is deliberate — a runner
//! that spawns a vendor CLI itself can only be tested by spawning one, and "no error
//! came back" is exactly the decorative gate this project has shipped twice.

use crate::config::Config;
use crate::error::{PieError, Result};

/// How often a job should run. Opt-in: the default is [`Cadence::Never`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Cadence {
    /// The default. Nothing is ever due, so nothing runs and nothing is fetched.
    #[default]
    Never,
    /// Once per day.
    Daily,
    /// Every 30 days.
    ///
    /// A fixed interval, not a calendar month: a 31-day month drifts by a day. That is
    /// acceptable for a monthly digest and it is what `interval_ms` promises, so the name
    /// says 30 days rather than claiming a precision the function does not have.
    Monthly,
}

impl Cadence {
    /// The interval in milliseconds, or `None` for [`Cadence::Never`].
    pub fn interval_ms(self) -> Option<u64> {
        match self {
            Cadence::Never => None,
            Cadence::Daily => Some(24 * 60 * 60 * 1_000),
            Cadence::Monthly => Some(30 * 24 * 60 * 60 * 1_000),
        }
    }
}

/// A token budget for one job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Budget {
    /// The most a job may spend. `0` means it may not spend at all.
    pub max_tokens: u64,
    /// What has already been spent inside the current window.
    pub spent: u64,
}

impl Budget {
    /// Create a budget with nothing spent.
    pub fn new(max_tokens: u64) -> Self {
        Self {
            max_tokens,
            spent: 0,
        }
    }

    /// What is left, saturating rather than wrapping.
    pub fn remaining(&self) -> u64 {
        self.max_tokens.saturating_sub(self.spent)
    }
}

/// Whether a job is due. **Pure** — the clock is a parameter, never read.
///
/// A runner that reads the clock inside its own decision can only be tested today, at
/// one instant, which is exactly where a boundary bug lives.
pub fn due(last_run_ms: Option<u64>, cadence: Cadence, now_ms: u64) -> bool {
    // `Never` is the default and is answered before any arithmetic, so no clock value —
    // not even `u64::MAX` — can make an opted-out job run.
    let Some(interval_ms) = cadence.interval_ms() else {
        return false;
    };

    // Never run before: there is no window to be inside, so the first opportunity is
    // this one. (The scaffold's `None ⇒ false` would have made an opted-*in* job wait
    // an interval for a run that never happened.)
    let Some(last_run_ms) = last_run_ms else {
        return true;
    };

    // A clock that moved backwards (an NTP correction, a restored snapshot, a timezone
    // change) leaves `last_run_ms` in the future. Subtracting would either wrap and
    // report a colossal interval already elapsed, or saturate to zero and wedge the job
    // until the wall clock caught up with a timestamp it will never reach. Neither is
    // acceptable, so a future stamp is treated as "never ran": it is due, and the run
    // rewrites the stamp with the clock it actually saw.
    if last_run_ms > now_ms {
        return true;
    }

    // `>=`, not `>`: exactly at the interval is due. The boundary is where a scheduling
    // bug lives, and a one-millisecond-early skip is invisible from the middle of the
    // window. This subtraction cannot wrap because of the guard above.
    now_ms - last_run_ms >= interval_ms
}

/// Refuse a job whose estimate the budget cannot cover, **before** anything spawns.
///
/// Checked before the spawn, not after: a budget that is only consulted once the call
/// has been made is a report, not a budget. A `max_tokens` of `0` therefore refuses
/// every job rather than allowing one.
pub fn check_budget(budget: &Budget, estimate: u64) -> Result<()> {
    if estimate > budget.remaining() {
        return Err(PieError::Config(format!(
            "background jobs are budgeted: this job estimates {estimate} tokens but only {} of \
             {} remain. Nothing was spawned. Raise the budget in config, or run with \
             `--dry-run` to see what it would do (RISKS.md §13)",
            budget.remaining(),
            budget.max_tokens
        )));
    }
    Ok(())
}

/// The id of the learn job (`pie schedule run learn`), and the one `Config` names today.
pub const LEARN_JOB: &str = "learn";

/// Why a job is being considered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    /// A scheduled tick: only what [`due`] calls due runs.
    Cadence,
    /// The user asked for this job now (`pie schedule run <JOB>`, `DESIGN.md` §16).
    ///
    /// The cadence is not consulted — that is what "run it now" means — but the budget
    /// still is: a manual run is a request to do the work, never a licence to spend.
    Manual,
}

/// A background job, as the runner sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    /// Stable id, used by `pie schedule run <JOB>` and by the dated report.
    pub id: String,
    /// How often it may run. Opt-in: [`Cadence::Never`] by default.
    pub cadence: Cadence,
    /// What the job is expected to spend. Checked against the budget **before** the
    /// spawn, so it is the estimate that has to fit, not the invoice.
    pub estimate_tokens: u64,
}

impl Job {
    /// The learn job as `config.toml` describes it.
    ///
    /// The cadence comes from the configuration rather than from this module, so
    /// `[jobs] learn_cadence = "never"` — the default — is what makes the job not due,
    /// and a job cannot become scheduled by being installed.
    pub fn learn(cfg: &Config, estimate_tokens: u64) -> Self {
        Self {
            id: LEARN_JOB.to_string(),
            cadence: cfg.jobs.learn_cadence.to_schedule(),
            estimate_tokens,
        }
    }
}

/// What a completed run reports back to the ledger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spend {
    /// Tokens the run **actually** consumed, as the run reported them.
    pub tokens: u64,
    /// The run's output, for the dated report and `pie schedule status`.
    pub output: String,
}

/// What a job recorded: when it last ran, and what it has spent in the current window.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Ledger {
    /// When the job last ran, in epoch ms. `None` means it never has.
    pub last_run_ms: Option<u64>,
    /// The budget and the spend against it, for the window currently open.
    pub budget: Budget,
}

impl Ledger {
    /// A ledger for the configured budget, with nothing spent and nothing run.
    ///
    /// Inherits the configuration's `0` default, so a ledger nothing has opted into
    /// refuses every job rather than merely being unconfigured.
    pub fn from_config(cfg: &Config) -> Self {
        Self {
            last_run_ms: None,
            budget: Budget::new(cfg.jobs.learn_budget_tokens),
        }
    }
}

/// What one call to [`run_job`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunReport {
    /// Nothing was due, so nothing was checked further and nothing was spawned.
    NotDue,
    /// The job ran; the ledger now records its spend.
    Ran(Spend),
}

/// Decide, budget, and run exactly one job.
///
/// The order of the three steps **is** the guarantee, and each is a claim in
/// `ACCEPTANCE.md` §15:
///
/// 1. the cadence decides ([`Trigger::Cadence`]) — with the default config that is
///    [`Cadence::Never`], so nothing is due and no provider is ever reached;
/// 2. [`check_budget`] refuses **before** the spawn, so a job that cannot be afforded
///    costs nothing at all and a budget of `0` cannot be exceeded by one call;
/// 3. only then is `spawn` called, and only then is the ledger written.
///
/// `spawn` is a parameter rather than a call to [`crate::provider::by_id`] because the
/// thing being spawned must be replaceable by a counting stand-in: that is what turns
/// "background jobs are budgeted and opt-in" from a paragraph into an assertion about
/// *how many times* a run reached a provider.
///
/// A failed spawn records nothing: pie cannot tell a binary that never started from one
/// that started and burned quota before dying, and inventing a figure it cannot measure
/// is worse than an honest error. The cadence bounds how often the retry happens, and
/// the failure is the caller's to report.
///
/// # Errors
///
/// [`PieError::Config`] when the estimate does not fit the remaining budget, and
/// whatever `spawn` returns when the job itself fails.
pub fn run_job<S>(
    job: &Job,
    ledger: &mut Ledger,
    trigger: Trigger,
    now_ms: u64,
    spawn: S,
) -> Result<RunReport>
where
    S: FnOnce(&Job) -> Result<Spend>,
{
    match trigger {
        Trigger::Cadence if !due(ledger.last_run_ms, job.cadence, now_ms) => {
            return Ok(RunReport::NotDue);
        }
        // A new cadence window opens the budget window with it. Without this, a job that
        // spent its budget once would be permanently dead; with it, `spent` means "spent
        // in the window that is open now", which is what the claim needs it to mean.
        //
        // Not when the stamp is in the future: a clock that moved backwards must not also
        // hand the whole budget back, or a timezone slip becomes a spend (`RISKS.md` §13).
        // Only the scheduled trigger rolls a window — asking twice is not time passing.
        Trigger::Cadence => {
            if matches!(ledger.last_run_ms, Some(last) if last <= now_ms) {
                ledger.budget.spent = 0;
            }
        }
        Trigger::Manual => {}
    }

    // Before the spawn, never after. A budget consulted once the call has been made is a
    // report, not a budget.
    //
    // The job is named here because `check_budget` cannot name it — its signature is
    // frozen to `(budget, estimate)` — and with a second job arriving in M8 a refusal
    // that says only "this job" cannot be attributed to anything.
    check_budget(&ledger.budget, job.estimate_tokens)
        .map_err(|e| PieError::Config(format!("job `{}`: {e}", job.id)))?;

    let spend = spawn(job)?;

    // The actual spend is recorded, not the estimate: an estimate that under-quoted the
    // run must not become the ledger's memory of it. `saturating_add` keeps a spend past
    // the budget from wrapping into "plenty left", and `remaining` already saturates at
    // zero, so an overrun refuses everything afterwards instead of going negative.
    ledger.budget.spent = ledger.budget.spent.saturating_add(spend.tokens);
    ledger.last_run_ms = Some(now_ms);
    Ok(RunReport::Ran(spend))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    /// The default cadence is `Never`, which is what makes M7 opt-in by construction.
    #[test]
    fn the_default_cadence_is_never_so_nothing_runs_unasked() {
        assert_eq!(Cadence::default(), Cadence::Never);
        assert_eq!(Cadence::Never.interval_ms(), None);
        assert!(!due(None, Cadence::Never, u64::MAX));
        assert!(!due(Some(0), Cadence::Never, u64::MAX));
    }

    /// A budget of zero must refuse everything: "budgeted" means the check happens
    /// before the spawn, so zero cannot be exceeded by one call.
    #[test]
    fn a_zero_budget_refuses_even_the_smallest_job() {
        let budget = Budget::new(0);
        assert!(
            check_budget(&budget, 0).is_ok(),
            "a zero-token job costs nothing"
        );
        assert!(
            check_budget(&budget, 1).is_err(),
            "one token is over a zero budget"
        );
    }

    /// The ceiling is the remainder, not the maximum: a second run in the same window
    /// cannot re-spend what the first one spent.
    #[test]
    fn the_ceiling_is_the_remainder_not_the_maximum() {
        let mut budget = Budget::new(100);
        assert!(check_budget(&budget, 100).is_ok());
        budget.spent = 60;
        assert!(
            check_budget(&budget, 40).is_ok(),
            "exactly the remainder fits"
        );
        assert!(
            check_budget(&budget, 41).is_err(),
            "one over the remainder must refuse, so a second run cannot re-spend"
        );
    }

    #[test]
    fn remaining_saturates_instead_of_wrapping() {
        let budget = Budget {
            max_tokens: 10,
            spent: u64::MAX,
        };
        assert_eq!(budget.remaining(), 0);
    }
}
