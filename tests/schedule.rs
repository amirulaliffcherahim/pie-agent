//! Gate G4 for M7 stage C: the budgeted, opt-in job runner.
//!
//! `planning/ACCEPTANCE.md` §15 lists the checks; this file is where they are enforced.
//! The requirement itself is cross-cutting (`planning/ROADMAP.md`'s list, `RISKS.md`
//! §13): **background jobs must be budgeted and opt-in**. Prose cannot enforce that, so
//! three claims carry it here, and every one is asserted by **count**:
//!
//! * with the default configuration a scheduled run reaches no provider at all;
//! * a job the budget cannot cover is refused **before** anything is spawned, so a
//!   budget of `0` cannot be "exceeded by one call";
//! * the ledger records what was spent, so a second run inside the same window is
//!   measured against the remainder rather than the maximum again.
//!
//! The count comes from a **counting stand-in**: a script that appends a line to
//! `calls.txt` before printing its fixture — the pattern `tests/cache.rs` established.
//! Asserting "no error came back" is the decorative gate this project has shipped
//! twice; a counter file that exists is not something an error-free return can fake.
//! `a_spawned_job_leaves_a_mark_in_the_counter` is the control that proves the counter
//! can see a spawn at all, so the absence assertions are not vacuous.
//!
//! Nothing here needs a vendor CLI, a network, or a single token of quota: the injected
//! spawner runs the stand-in, and `Config::default()` names no model. Every test builds
//! its own temp directory and never reads the real `~/.pie` or a vendor store, so none
//! of them depends on this machine having any vendor state at all (invariant I12).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::process::Command;

use pie::config::Config;
use pie::error::{PieError, Result};
#[cfg(windows)]
use pie::process::CREATE_NO_WINDOW;
use pie::provider;
use pie::schedule::{
    due, run_job, Budget, Cadence, Job, Ledger, RunReport, Spend, Trigger, LEARN_JOB,
};

/// What the stand-in claims to have spent.
///
/// Deliberately neither `0` nor a divisor of the budgets below: a runner that forgot to
/// record the spend must produce a *different* remainder, not a coincidentally equal one.
const STAND_IN_SPEND: u64 = 7;

/// The counting stand-in: it appends a line to `calls.txt` every time it runs, then
/// prints the payload.
///
/// The counter is the whole point. It distinguishes "the runner decided not to spawn"
/// from "the runner spawned and reported afterwards", which no assertion on the run's
/// own return value can do — the second runner returns `Ok` too.
fn counting_stand_in(dir: &Path) -> PathBuf {
    let counter = dir.join("calls.txt");

    #[cfg(windows)]
    let script = dir.join("counting-agy.bat");
    #[cfg(not(windows))]
    let script = dir.join("counting-agy.sh");

    #[cfg(windows)]
    let body = format!(
        "@echo off\r\necho call>> \"{}\"\r\necho SPENT={STAND_IN_SPEND}\r\n",
        counter.display()
    );
    #[cfg(not(windows))]
    let body = format!(
        "#!/bin/sh\necho call >> '{}'\necho SPENT={STAND_IN_SPEND}\n",
        counter.display()
    );

    std::fs::write(&script, body).expect("write the counting stand-in");

    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
            .expect("chmod the counting stand-in");
    }

    script
}

/// How many times the stand-in ran.
///
/// A missing counter file reports `0` rather than an error, because "never created" is
/// the assertion being made: a file that does not exist is exactly zero spawns.
fn spawns(dir: &Path) -> usize {
    std::fs::read_to_string(dir.join("calls.txt"))
        .map(|text| text.lines().count())
        .unwrap_or(0)
}

/// The **default** configuration with one override: agy's binary is the stand-in.
///
/// Nothing else changes, so the cadence and the budget under test are still the defaults
/// — which is what makes the "zero spawns" claim a claim about the shipped default
/// rather than about a config this test invented. The override is the point: a run that
/// reaches a provider at all reaches *this* process, and the counter says so.
fn cfg_with_stand_in(script: &Path) -> Config {
    let mut cfg = Config::default();
    cfg.agy.bin = script.display().to_string();
    cfg
}

/// The spawner the runner is handed.
///
/// It resolves the program the way the real one does — `provider::by_id("agy")` then
/// `binary(cfg)`, i.e. pie's own resolution of `cfg.agy.bin` — so the count below is a
/// count of a run reaching *the configured backend binary*, not of a helper the test
/// happens to call. Stage D's spawner drives the vendor CLI behind this same seam; a
/// test drives a script.
fn spawner(cfg: &Config) -> impl FnOnce(&Job) -> Result<Spend> {
    let program = provider::by_id("agy")
        .expect("agy is in the registry")
        .binary(cfg)
        .expect("the stand-in resolves as a path");

    move |job: &Job| {
        let mut cmd = Command::new(&program);
        cmd.args(["--print", &job.id]);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // I1's reason, applied to this test's own children: a batch file must not
            // paint a console over the gate run.
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        let output = cmd.output().map_err(|source| PieError::Spawn {
            backend: "agy".to_string(),
            source,
        })?;

        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        Ok(Spend {
            tokens: spent_tokens(&stdout),
            output: stdout,
        })
    }
}

/// The tokens the stand-in reported.
///
/// Everything the ledger records comes from the child's own output, so "the ledger
/// records `spent`" stays a fact about a run rather than about a constant that happens to
/// match it.
fn spent_tokens(stdout: &str) -> u64 {
    stdout
        .split_whitespace()
        .find_map(|word| word.strip_prefix("SPENT="))
        .and_then(|n| n.parse().ok())
        .expect("the stand-in prints SPENT=<n>")
}

/// The control, and the reason every absence assertion below is evidence.
///
/// Without this, "the counter file was never created" would also pass for a stand-in that
/// could never have been created. Same construction, one run: this one must leave a mark.
#[test]
fn a_spawned_job_leaves_a_mark_in_the_counter() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = cfg_with_stand_in(&counting_stand_in(dir.path()));
    let job = Job {
        id: LEARN_JOB.to_string(),
        cadence: Cadence::Daily,
        estimate_tokens: 1,
    };
    let mut ledger = Ledger {
        last_run_ms: None,
        budget: Budget::new(1_000),
    };

    let RunReport::Ran(spend) =
        run_job(&job, &mut ledger, Trigger::Cadence, 0, spawner(&cfg)).unwrap()
    else {
        panic!("a due, affordable job must run");
    };

    assert_eq!(
        spawns(dir.path()),
        1,
        "a run that ran must have spawned once"
    );
    assert_eq!(spend.tokens, STAND_IN_SPEND);
    assert_eq!(
        ledger.budget.spent, STAND_IN_SPEND,
        "the ledger must record what the run spent, not what it was estimated at"
    );
    assert_eq!(ledger.last_run_ms, Some(0));
}

/// The milestone's central claim: **the default path reaches nothing.**
///
/// `Config::default()` means the learn job is `Cadence::Never` with a budget of `0`, so
/// there is no instant at which anything is due. The assertion that carries it is the
/// counter, not the return value: a runner that spawned first and reported afterwards
/// returns `Ok` here too.
#[test]
fn the_default_config_reaches_no_provider_at_all() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = cfg_with_stand_in(&counting_stand_in(dir.path()));

    let job = Job::learn(&cfg, 500);
    assert_eq!(
        job.cadence,
        Cadence::Never,
        "a job must not run because it was installed"
    );
    let mut ledger = Ledger::from_config(&cfg);
    assert_eq!(
        ledger.budget.max_tokens, 0,
        "the default path must make no model call: no backend pie drives is cheap"
    );

    // Every instant worth asking about: never run, a first tick a day later, and the end
    // of time. None of them may reach a provider.
    for (last_run, now) in [
        (None, 0u64),
        (None, Cadence::Daily.interval_ms().unwrap()),
        (None, u64::MAX),
        (Some(0), u64::MAX),
    ] {
        ledger.last_run_ms = last_run;
        let report = run_job(&job, &mut ledger, Trigger::Cadence, now, spawner(&cfg)).unwrap();
        assert_eq!(
            report,
            RunReport::NotDue,
            "`never` means never, at {last_run:?}..{now}"
        );
    }

    assert_eq!(
        spawns(dir.path()),
        0,
        "the default config spawned a provider; the counter is the evidence, not the exit code"
    );
    assert!(
        !dir.path().join("calls.txt").exists(),
        "nothing spawned, so the counter file must not exist at all"
    );
    assert_eq!(ledger.budget.spent, 0, "nothing ran, so nothing was spent");
}

/// A budget the job cannot cover refuses **before** the spawn.
///
/// The order is the claim: a budget consulted after the call is a report, not a budget.
/// `0` is also the shipped default, so this is the second lock on the default path — even
/// when the cadence says "go now", a zero budget stops at zero rather than at one call.
#[test]
fn a_zero_budget_refuses_before_anything_is_spawned() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = cfg_with_stand_in(&counting_stand_in(dir.path()));
    let job = Job {
        id: LEARN_JOB.to_string(),
        cadence: Cadence::Daily,
        estimate_tokens: 1,
    };
    let mut ledger = Ledger::from_config(&cfg);

    // Two ways in, and both must refuse: a scheduled tick that *is* due (the user set a
    // cadence but no budget), and a manual `pie schedule run learn`.
    for trigger in [Trigger::Cadence, Trigger::Manual] {
        let err = run_job(&job, &mut ledger, trigger, 0, spawner(&cfg))
            .expect_err("a zero budget cannot cover a one-token job");
        assert!(
            !dir.path().join("calls.txt").exists(),
            "the refusal happened after the spawn ({trigger:?}): {err}"
        );
    }

    assert_eq!(spawns(dir.path()), 0, "a refused job must cost nothing");
    assert_eq!(ledger.budget.spent, 0);
    assert_eq!(
        ledger.last_run_ms, None,
        "a run that never happened is not recorded as one"
    );
}

/// A budget error that does not say how much was left is unusable.
///
/// So the refusal names the job, what it estimated, what actually remains, and the way
/// out — the four things a user needs to act on it rather than to guess.
#[test]
fn the_refusal_names_the_job_the_estimate_the_remainder_and_the_remedy() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = cfg_with_stand_in(&counting_stand_in(dir.path()));
    let job = Job {
        id: LEARN_JOB.to_string(),
        cadence: Cadence::Daily,
        estimate_tokens: 1_200,
    };
    let mut ledger = Ledger {
        last_run_ms: None,
        budget: Budget {
            max_tokens: 900,
            spent: 0,
        },
    };

    let err = run_job(&job, &mut ledger, Trigger::Manual, 0, spawner(&cfg))
        .expect_err("1200 tokens do not fit in 900");

    let message = err.to_string();
    assert!(
        message.contains("learn"),
        "the job must be named: {message}"
    );
    assert!(
        message.contains("estimates 1200 tokens"),
        "the estimate must be named: {message}"
    );
    assert!(
        message.contains("only 900 of 900 remain"),
        "the remainder must be named, not just the maximum: {message}"
    );
    assert!(
        message.contains("Nothing was spawned"),
        "the refusal must say that nothing happened: {message}"
    );
    assert!(
        message.contains("Raise the budget"),
        "the remedy must be named: {message}"
    );
    assert_eq!(spawns(dir.path()), 0);
}

/// The ledger records `spent`, so a second run **inside the same window** is measured
/// against the remainder — not handed the maximum again.
///
/// A runner that forgot `spent` between calls would allow the second run the full `10`
/// and spend it, which is exactly how a daily job drains a premium subscription without
/// ever looking unbudgeted.
#[test]
fn a_second_run_in_the_same_window_spends_the_remainder_not_the_maximum_again() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = cfg_with_stand_in(&counting_stand_in(dir.path()));
    let job = Job {
        id: LEARN_JOB.to_string(),
        cadence: Cadence::Daily,
        estimate_tokens: 3,
    };
    let mut ledger = Ledger {
        last_run_ms: None,
        budget: Budget::new(10),
    };

    let RunReport::Ran(spend) =
        run_job(&job, &mut ledger, Trigger::Manual, 1_000, spawner(&cfg)).unwrap()
    else {
        panic!("the first run is affordable");
    };
    assert_eq!(spend.tokens, STAND_IN_SPEND);
    assert_eq!(ledger.budget.spent, 7, "10 - 7 = 3 is what is left");
    assert_eq!(ledger.budget.remaining(), 3);

    // The scheduled path is not due again inside the window, so asking twice is a
    // deliberate act rather than something a tick can do.
    assert_eq!(
        run_job(&job, &mut ledger, Trigger::Cadence, 1_000, spawner(&cfg)).unwrap(),
        RunReport::NotDue
    );

    // 4 tokens is under the 10-token maximum but over the 3-token remainder. This is the
    // assertion a runner that re-read only `max_tokens` cannot satisfy.
    let greedy = Job {
        estimate_tokens: 4,
        ..job.clone()
    };
    let refused = run_job(&greedy, &mut ledger, Trigger::Manual, 1_000, spawner(&cfg))
        .expect_err("the ceiling is the remainder, not the maximum again");
    assert!(
        refused.to_string().contains("only 3 of 10 remain"),
        "{refused}"
    );
    assert_eq!(
        spawns(dir.path()),
        1,
        "a refusal must not have spawned anything"
    );

    // Exactly the remainder is still allowed, so this is a budget rather than a permanent
    // lock-out after the first run.
    let exact = Job {
        estimate_tokens: 3,
        ..job.clone()
    };
    assert!(matches!(
        run_job(&exact, &mut ledger, Trigger::Manual, 1_000, spawner(&cfg)).unwrap(),
        RunReport::Ran(_)
    ));
    assert_eq!(spawns(dir.path()), 2);
    // The stand-in spent 7 against a 3-token estimate, and the ledger records the fact
    // rather than the estimate: it saturates at zero usable budget from here.
    assert_eq!(ledger.budget.spent, 14);
    assert_eq!(ledger.budget.remaining(), 0);

    // And now nothing fits at all — one token is over a zero remainder.
    let one = Job {
        estimate_tokens: 1,
        ..job.clone()
    };
    assert!(
        run_job(&one, &mut ledger, Trigger::Manual, 1_000, spawner(&cfg)).is_err(),
        "a window whose budget is used up must refuse every further job"
    );
    assert_eq!(
        spawns(dir.path()),
        2,
        "the exhausted window spawned nothing more"
    );
}

/// A new cadence window opens the budget window with it — otherwise a job that spent its
/// budget once would be permanently dead — but a clock that moved backwards must not hand
/// the whole budget back, or a timezone slip becomes a spend (`RISKS.md` §13).
#[test]
fn a_new_window_opens_the_budget_and_a_backwards_clock_does_not() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = cfg_with_stand_in(&counting_stand_in(dir.path()));
    let day = Cadence::Daily.interval_ms().unwrap();
    let job = Job {
        id: LEARN_JOB.to_string(),
        cadence: Cadence::Daily,
        estimate_tokens: 3,
    };

    // A window with 8 of 20 already spent, and a clock that reports a time *before* the
    // last run.
    let mut ledger = Ledger {
        last_run_ms: Some(1_000),
        budget: Budget {
            max_tokens: 20,
            spent: 8,
        },
    };
    let RunReport::Ran(_) = run_job(&job, &mut ledger, Trigger::Cadence, 0, spawner(&cfg)).unwrap()
    else {
        panic!("a stamp in the future must not wedge the job: it is due");
    };
    assert_eq!(
        ledger.budget.spent, 15,
        "a backwards clock must not hand the budget back: 8 spent + 7 actually spent"
    );
    assert_eq!(ledger.budget.remaining(), 5);
    assert_eq!(
        ledger.last_run_ms,
        Some(0),
        "the run rewrites the stamp with the clock it saw, so the next tick is not wedged"
    );

    // Now the ordinary case: a full interval later the window has rolled, the job is due,
    // and the budget is whole again.
    let mut ledger = Ledger {
        last_run_ms: Some(1_000),
        budget: Budget {
            max_tokens: 20,
            spent: 8,
        },
    };
    assert!(matches!(
        run_job(
            &job,
            &mut ledger,
            Trigger::Cadence,
            1_000 + day,
            spawner(&cfg)
        )
        .unwrap(),
        RunReport::Ran(_)
    ));
    assert_eq!(
        ledger.budget.spent, 7,
        "a new window starts from zero spent, not from the last window's total"
    );
    assert_eq!(ledger.last_run_ms, Some(1_000 + day));
}

/// `due` is asserted **at and around its boundary**, not only in the middle.
///
/// The middle of a window is where every implementation is right; the boundary is where a
/// scheduling bug lives, and a runner that reads the clock inside its own decision can
/// only ever be tested at one instant.
#[test]
fn due_is_asserted_at_and_around_its_boundary() {
    let day = Cadence::Daily.interval_ms().unwrap();
    let month = Cadence::Monthly.interval_ms().unwrap();
    let now = 40 * day;

    assert_eq!(Cadence::default(), Cadence::Never);

    // Never run before: there is no window to be inside, so the first instant is due.
    assert!(due(None, Cadence::Daily, now));
    assert!(due(None, Cadence::Monthly, now));

    // Unchanged since the last run, and one millisecond short of the interval.
    assert!(!due(Some(now), Cadence::Daily, now));
    assert!(
        !due(Some(now - day + 1), Cadence::Daily, now),
        "one millisecond early must not run"
    );
    // Exactly at the interval, and one millisecond past it.
    assert!(
        due(Some(now - day), Cadence::Daily, now),
        "exactly at the interval is due"
    );
    assert!(due(Some(now - day - 1), Cadence::Daily, now));

    // The interval belongs to the cadence: a day is not a month.
    assert!(!due(Some(now - day), Cadence::Monthly, now));
    assert!(due(Some(now - month), Cadence::Monthly, now));

    // A clock that moved backwards must not wedge the job until the wall clock catches up
    // with a timestamp it will never reach.
    assert!(due(Some(now + 1), Cadence::Daily, now));
    assert!(due(Some(u64::MAX), Cadence::Daily, now));

    // And `Never` is never due, whatever the clock says — the default is the opt-out.
    assert!(!due(None, Cadence::Never, 0));
    assert!(!due(None, Cadence::Never, u64::MAX));
    assert!(!due(Some(0), Cadence::Never, u64::MAX));
    assert!(!due(Some(u64::MAX), Cadence::Never, 0));
}
