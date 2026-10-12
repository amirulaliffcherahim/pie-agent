//! `pie watch` — the monthly watchtower's runner (`DESIGN.md` §17).
//!
//! Stages A ([`crate::watch::drift`]), B ([`crate::watch::news`]) and C
//! ([`crate::watch::render_report`]) each own a pure half. This module is the command
//! that ties them together and produces the artefact the milestone's Done condition
//! names: one file under `$PIE_HOME/reports`, called `watchtower-YYYY-MM.md`.
//!
//! Four properties are the shape of this file, and each is a test rather than a comment:
//!
//! * **The clock is a parameter.** `render_report` already takes `month`; this module
//!   takes `now_ms` and derives the file name from it, so `watchtower-YYYY-MM.md` is
//!   testable at any date instead of only in the current month. Nothing in the decision
//!   path reads the system time ([`crate::session_log::now_millis`] is passed in by
//!   [`run`] and by [`execute_scheduled`]'s caller, never by `execute`).
//! * **The default path spends nothing, and the scheduled path is opt-in.** With the
//!   shipped configuration nothing is fetched — no source is configured — and
//!   [`execute_scheduled`] reaches no CLI at all: the cadence is `never`, so `due()`
//!   answers before anything else, and the job's budget is `0`, so even an opted-in
//!   cadence is refused **before** the first spawn (`risks §13`, and `DESIGN.md` §16's
//!   ruling that a budget consulted after the call is a report, not a budget). The
//!   zero-spawn claim is asserted by **count**, through a counting stand-in.
//! * **An empty capture is a diagnostic, never a quiet snapshot.** `render_report` sees
//!   only findings, so a snapshot whose `help` is empty would read as `no drift` on every
//!   later run. A `--help` that produced nothing is therefore a stated error naming the
//!   backend (`DESIGN.md` §17, "The `--help` producer"), and no snapshot is written for
//!   it.
//! * **A lost source is stated, and the run is not a success.** `Http` is a trait so no
//!   test reaches the network; a fetch failure becomes a [`crate::watch::failed_source`]
//!   entry, which `render_report` turns into the digest-incomplete note, and
//!   [`incomplete_digest`] turns into a non-zero exit.
//!
//! # Two entry points, because "the user asked" and "the schedule ticked" are different
//!
//! [`execute`] is the manual path (`pie watch`). The user typed the command, so it does
//! what the command says: probe, fetch, write. Everything it does is free — a `--help` is
//! not a turn (`DESIGN.md` §17) — so there is no model spend to gate, and gating a
//! zero-token probe behind a *token* budget would make the command require a fake token
//! budget to do anything at all.
//!
//! [`execute_scheduled`] is the background path, and it is the one that must be opt-in:
//! it goes through [`schedule::run_job`], so the cadence decides first and
//! [`schedule::check_budget`] refuses before the spawn. With the shipped defaults it
//! performs **zero** probes and writes no file.
//!
//! # Contract notes this stage could not resolve on its own
//!
//! Written here rather than silently decided, and repeated in the stage report:
//!
//! 1. **The scheduled job's estimate counts spawns, not model tokens.** The only
//!    refuse-before-spawn gate pie has is `schedule::check_budget`, whose field is named
//!    `max_tokens`; a probe spends no model tokens, and an estimate of `0` would let a
//!    budget of `0` *allow* a spawn — a gate that cannot fail, the shape B9/B20 recorded.
//!    So the estimate is one unit per planned probe, and the scheduled run is genuinely
//!    refused by the shipped budget of `0`.
//! 2. **There is no `[jobs] watch_cadence` / `watch_budget_tokens` key.** `config.rs` is
//!    the maintainer's, so [`watch_job`] reuses `jobs.learn_cadence` and
//!    `jobs.learn_budget_tokens` — the same "one opt-in story, two consumers" the M7/M8
//!    brief names — while the ledger that records the spend is this job's own file, so a
//!    probe count can never be added to the learn job's token accounting.
//! 3. **The `strings` leg reads the vendor's own artefact, never its launcher.**
//!    `DESIGN.md` §17 documents the trap: codex is `node <codex.js>` here, so extracting
//!    from the *resolved program* would record node's strings as codex's
//!    (`ACCEPTANCE.md` §13 B34). [`crate::provider::Provider::artefact`] names the right
//!    file, [`crate::watch::strings`] extracts from it, and the read is bounded — a prefix
//!    read is stated in the snapshot and in the run's output rather than silently compared.

use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::error::{PieError, Result};
use crate::paths;
use crate::process;
use crate::provider;
use crate::schedule::{self, Job, Ledger, RunReport, Spend, Trigger};
use crate::session_log::now_millis;
use crate::watch::news::{self, Http, NewsItem, NewsSource};
use crate::watch::{self, CliSnapshot, Drift};

/// How long one `--version`/`--help` probe may take.
///
/// Short on purpose: a probe that hangs must not hold the monthly job open, and
/// `process::run_streaming` kills the whole child tree on expiry.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// The scheduled job's id, the one a refusal and the report both name.
pub const WATCH_JOB: &str = "watch";

/// The scheduled job's ledger, inside the snapshot directory.
///
/// Not `$PIE_HOME/schedule.json`: that file is the **learn** job's ledger
/// (`commands/schedule.rs`), and a drift probe recorded there would be added to a token
/// budget it never spent — the two jobs share one *mechanism*, not one set of numbers.
const LEDGER_FILE: &str = "schedule.json";

/// How many probes one backend costs: its version, and its help.
const PROBES_PER_BACKEND: u64 = 2;

/// The suffix a snapshot gets once it has been replaced, so both sides of a diff stay
/// readable after the run that reported them.
const PREVIOUS_SUFFIX: &str = ".previous.json";

/// The dot-suffix for the report's scratch file, inside the reports directory so the
/// final rename is same-volume.
const SCRATCH_SUFFIX: &str = ".tmp";

/// The window a percentage-style month must be, for [`checked_month`].
const MONTH_LEN: usize = 7;

/// Milliseconds in a day, for the civil-date walk.
const MS_PER_DAY: u64 = 86_400_000;

/// Which halves of the monthly run to perform (`DESIGN.md` §17's CLI surface).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Scope {
    /// `pie watch` — probe the CLIs, fetch the sources, and write the report.
    #[default]
    All,
    /// `pie watch drift` — probe and snapshot, and write no report.
    Drift,
    /// `pie watch news` — fetch and print the digest, and write no report.
    News,
}

impl Scope {
    /// Whether this scope spawns the CLIs.
    fn probes(self) -> bool {
        matches!(self, Scope::All | Scope::Drift)
    }
    /// Whether this scope fetches over the network.
    fn fetches(self) -> bool {
        matches!(self, Scope::All | Scope::News)
    }

    /// Whether this scope writes the monthly report.
    ///
    /// Only [`Scope::All`]. A report written from a run that fetched nothing would print
    /// "no headlines this month" for a month it never looked at, which is the silent-empty
    /// outcome `ACCEPTANCE.md` §16 rules out — so the two partial scopes deliberately
    /// produce no file at all.
    fn writes_report(self) -> bool {
        matches!(self, Scope::All)
    }

    /// The word the run's report prints.
    pub fn label(self) -> &'static str {
        match self {
            Scope::All => "all",
            Scope::Drift => "drift",
            Scope::News => "news",
        }
    }
}

/// What one `pie watch` run is asked to do.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Options {
    /// Which halves to run.
    pub scope: Scope,
    /// Decide and report, but write nothing, spawn nothing and fetch nothing.
    ///
    /// Free without exception, following M7's ruling (`DESIGN.md` §16): "run with
    /// `--dry-run` to see what it would do" is only honest if looking is free, and a
    /// `--help` probe is a process pie starts.
    pub dry_run: bool,
    /// `--month YYYY-MM`. `None` derives the month from the clock the caller passes,
    /// which is what keeps a date bug testable at any date.
    pub month: Option<String>,
    /// The feeds to fetch. **Empty by default, and that is the opt-in**: with no source
    /// configured the run fetches nothing at all.
    ///
    /// `DESIGN.md` §17 says "configured sources" but no configuration key exists for them,
    /// and `config.rs` is not this stage's to extend — so the list arrives here, from the
    /// wiring.
    pub sources: Vec<NewsSource>,
    /// The stack [`crate::watch::rank`] scores headlines against, as `profile.toml` would
    /// name it. Empty means nothing is ranked above zero, which is a stated rule of that
    /// function rather than a special case here.
    pub stack: Vec<String>,
}

/// The command-line scope this means.
///
/// An exhaustive match, the same shape `JobCadence::to_schedule` uses (`src/config.rs`):
/// the CLI's spelling and this module's decision are separate types — one is a parsing
/// surface, the other a behaviour — and mapping them explicitly means a scope added to one
/// cannot be forgotten in the other without failing to compile.
impl From<crate::cli::WatchScope> for Scope {
    fn from(value: crate::cli::WatchScope) -> Self {
        match value {
            crate::cli::WatchScope::All => Scope::All,
            crate::cli::WatchScope::Drift => Scope::Drift,
            crate::cli::WatchScope::News => Scope::News,
        }
    }
}

/// Where the report for `month` is written.
///
/// Derived from the passed `home` rather than from [`watch::reports_dir`], which reads
/// `$PIE_HOME` at call time — a runner that takes its home as a parameter and then reads
/// the environment for the same path is a runner whose tests pass for the wrong reason.
/// `tests/watch_cmd.rs` asserts the two agree under a pinned `PIE_HOME`, so this derivation
/// cannot drift from the frozen function.
pub fn report_path(home: &Path, month: &str) -> PathBuf {
    home.join("reports").join(format!("watchtower-{month}.md"))
}

/// Where one backend's snapshot lives, under [`watch::snapshot_dir`].
pub fn snapshot_path(home: &Path, backend: &str) -> PathBuf {
    watch::snapshot_dir(home).join(format!("{backend}.json"))
}

/// Turn configured URLs into news sources, naming each after its host.
///
/// A bare URL is enough in `config.toml` because requiring a name as well would be two
/// keys to express one source, and the report needs *something* readable — so the host is
/// used. A URL with no recognisable host keeps the whole string as its name rather than
/// becoming "unknown": a typo in the config should be visible in the report, not
/// anonymous.
pub fn sources_from(urls: &[String]) -> Vec<NewsSource> {
    urls.iter()
        .map(|url| NewsSource {
            name: host_of(url).unwrap_or_else(|| url.clone()),
            url: url.clone(),
        })
        .collect()
}

/// The host part of a URL, if there is one. Handles a missing scheme (a feed URL can be
/// written either way in a config file) and strips any path, query or fragment.
fn host_of(url: &str) -> Option<String> {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
    (!host.is_empty()).then(|| host.to_string())
}

/// Where the snapshot it replaced stays, so both sides of a reported diff remain
/// inspectable after the run.
fn previous_snapshot_path(home: &Path, backend: &str) -> PathBuf {
    watch::snapshot_dir(home).join(format!("{backend}{PREVIOUS_SUFFIX}"))
}

/// Everything one run did, in a form a test can assert on and [`Outcome::render`] can
/// print.
#[derive(Debug, Default)]
pub struct Outcome {
    /// The month the run was for.
    pub month: String,
    /// What it was asked to do.
    pub scope: Scope,
    /// Whether it was a dry run.
    pub dry_run: bool,
    /// Whether the scheduled path found nothing due.
    pub not_due: bool,
    /// Whether there was no previous snapshot to compare against — the difference between
    /// "nothing changed" and "nothing was compared".
    pub first_run: bool,
    /// The file written, when one was.
    pub written: Option<PathBuf>,
    /// Every finding, in `diff`'s order.
    pub drifts: Vec<Drift>,
    /// The backends probed and the version each reported.
    pub snapshots: Vec<(String, String)>,
    /// The backends that could not be probed, and why.
    pub skipped: Vec<(String, String)>,
    /// The ranked digest, failure records included.
    pub digest: Vec<NewsItem>,
    /// How many sources were configured.
    pub sources: usize,
    /// How many of them answered.
    pub sources_ok: usize,
    /// How many headlines arrived.
    pub headlines: usize,
    /// The sources that failed, by name.
    pub failed_sources: Vec<String>,
    /// Things the run did not do, or did only in part, stated rather than implied — an
    /// artefact read that stopped at the cap, a dry run, a scope that fetched nothing.
    /// A note is never decoration: each one exists because the omission it describes would
    /// otherwise read as a clean result.
    pub notes: Vec<String>,
}

impl Outcome {
    /// The notification the roadmap asks for: the file path, then what produced it.
    ///
    /// A renderer rather than `println!` so the output is assertable without capturing a
    /// process's stdout — and so the sentence that names the artefact is a value a test
    /// reads, not a side effect it cannot see.
    pub fn render(&self) -> String {
        let mut out = String::new();

        if self.not_due {
            out.push_str(&format!(
                "watch: {WATCH_JOB} is not due, so nothing was probed, fetched or written. The \
                 cadence is `never` in the shipped configuration (`[jobs] learn_cadence`), and \
                 raising it is still refused before the first spawn while the budget is `0`\n"
            ));
            return out;
        }

        out.push_str(&format!(
            "watch: {} · scope {} · {}\n",
            self.month,
            self.scope.label(),
            if self.dry_run {
                "DRY RUN — nothing was written, spawned or fetched"
            } else {
                "real run"
            }
        ));

        match &self.written {
            Some(path) => out.push_str(&format!("report: {}\n", path.display())),
            None if self.dry_run => out.push_str(&format!(
                "report: not written (dry run); it would be \
                 `$PIE_HOME/reports/watchtower-{}.md`\n",
                self.month
            )),
            None => out.push_str("report: not written (this scope writes no report)\n"),
        }

        if self.scope.writes_report() || self.scope.probes() {
            out.push_str(&format!(
                "snapshot: {}\n",
                if self.first_run {
                    watch::FIRST_RUN_NOTE
                } else if self.drifts.is_empty() {
                    watch::NO_DRIFT_NOTE
                } else {
                    "compared against the previous snapshot"
                }
            ));
        }

        if !self.snapshots.is_empty() {
            out.push_str(&format!(
                "probed: {}\n",
                self.snapshots
                    .iter()
                    .map(|(backend, version)| format!("{backend} {version}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        for (backend, why) in &self.skipped {
            out.push_str(&format!("skipped: {backend} — {why}\n"));
        }
        for drift in &self.drifts {
            out.push_str(&format!(
                "finding: [{}] {}: {}\n",
                drift.class.label(),
                drift.backend,
                drift.what
            ));
        }

        if self.scope.fetches() {
            out.push_str(&format!(
                "news: {} headline(s) from {}/{} source(s)\n",
                self.headlines, self.sources_ok, self.sources
            ));
            if !self.failed_sources.is_empty() {
                out.push_str(&format!(
                    "digest: {} — {}\n",
                    watch::DIGEST_INCOMPLETE_NOTE,
                    self.failed_sources.join(", ")
                ));
            }
        }

        for note in &self.notes {
            out.push_str(&format!("note: {note}\n"));
        }

        out
    }
}

/// The digest's own gate (`ACCEPTANCE.md` §16): a run that lost a source is not a success.
///
/// A report that says "incomplete" and exits `0` is a report a script treats as a clean
/// month, so the two halves are asserted together: the note in the file, and a non-zero
/// exit here.
pub fn incomplete_digest(outcome: &Outcome) -> Result<()> {
    if outcome.failed_sources.is_empty() {
        return Ok(());
    }

    let report = match &outcome.written {
        Some(path) => format!(" The report was still written, to {} — read it as a partial picture rather than as a quiet month", path.display()),
        None => String::new(),
    };

    Err(PieError::NotReachable(format!(
        "the digest is incomplete: {} could not be fetched.{report}",
        outcome.failed_sources.join(", ")
    )))
}

/// `pie watch [drift|news] [--dry-run] [--month YYYY-MM]`
///
/// The manual path: the user asked for this run, so it happens, and the clock and the
/// network are resolved here and passed down.
pub async fn run(cfg: &Config, options: &Options) -> Result<()> {
    let outcome = execute(cfg, &paths::pie_home(), options, &NetworkHttp, now_millis()).await?;
    print!("{}", outcome.render());
    incomplete_digest(&outcome)
}

/// The manual run, over injected inputs.
///
/// Everything that could read the world — the clock, the home directory, the transport —
/// is a parameter, so a test drives the whole command without a network, without a vendor
/// CLI and without depending on today's date.
pub async fn execute(
    cfg: &Config,
    home: &Path,
    options: &Options,
    http: &dyn Http,
    now_ms: u64,
) -> Result<Outcome> {
    let month = match &options.month {
        Some(month) => checked_month(month)?,
        None => month_of(now_ms),
    };

    // A dry run decides and reports, and does nothing else. It does not probe, because a
    // probe is a process pie starts and the whole point of the flag is that looking is
    // free; it does not fetch for the same reason.
    if options.dry_run {
        let mut outcome = Outcome {
            month,
            scope: options.scope,
            dry_run: true,
            first_run: !a_snapshot_is_already_on_file(home),
            ..Outcome::default()
        };
        outcome.sources = options.sources.len();
        outcome.notes.push(
            "nothing was probed or fetched: `--dry-run` is free without exception, so it \
             reports what it would do rather than doing it"
                .to_string(),
        );
        return Ok(outcome);
    }

    let half = if options.scope.probes() {
        drift_half(cfg, home, now_ms).await?
    } else {
        DriftHalf::default()
    };

    finish(home, &month, options, http, half)
}

/// The background path: what a scheduled tick calls.
///
/// Opt-in by construction, in three steps whose order **is** the guarantee
/// (`DESIGN.md` §16, stage C's ruling):
///
/// 1. the cadence decides — the shipped `never` answers before anything is probed,
///    fetched or written;
/// 2. [`schedule::check_budget`] refuses before the spawn, so the shipped budget of `0`
///    cannot be "exceeded by one probe";
/// 3. only then does the drift half run, and only then is the ledger written.
///
/// The probe is the spawn closure rather than a call inside `run_job`, which is the only
/// shape in which "zero probes" is a *count* a test can assert instead of a sentence.
pub async fn execute_scheduled(
    cfg: &Config,
    home: &Path,
    http: &dyn Http,
    now_ms: u64,
) -> Result<Outcome> {
    let month = month_of(now_ms);
    let job = watch_job(cfg);
    let mut ledger = read_ledger(cfg, home)?;

    // `run_job`'s spawn is synchronous and the probes are not, so the two are bridged here
    // the way M7's opt-in review bridges them (`commands/learn.rs`): `block_in_place` needs
    // the multi-threaded runtime `main.rs` starts, and panics on a current-thread one
    // rather than blocking the only worker.
    let mut half: Option<DriftHalf> = None;
    let report = schedule::run_job(&job, &mut ledger, Trigger::Cadence, now_ms, |_job| {
        let probed = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(drift_half(cfg, home, now_ms))
        })?;
        let spend = Spend {
            tokens: probed.spawns,
            output: format!(
                "{} probe(s) over {} backend(s)",
                probed.spawns,
                probed.probed.len()
            ),
        };
        half = Some(probed);
        Ok(spend)
    })?;

    if matches!(report, RunReport::NotDue) {
        return Ok(Outcome {
            month,
            scope: Scope::All,
            not_due: true,
            ..Outcome::default()
        });
    }

    // Written after the run, and it records what the run *reported* rather than what it
    // was estimated at: the ledger is what makes the budget a per-window ceiling instead
    // of a per-run one.
    write_ledger(&ledger, home)?;

    let Some(half) = half else {
        // Unreachable in practice: `Ran` means the closure returned `Ok`. An error rather
        // than an empty outcome, because a run reported as complete with no probe result
        // would be a claim pie cannot support.
        return Err(PieError::Config(format!(
            "job `{WATCH_JOB}` reported a run but produced no probe result"
        )));
    };

    let options = Options::default();
    finish(home, &month, &options, http, half)
}

/// Assemble an outcome: fetch and rank, then write the report `month` names.
///
/// Shared by both entry points so that "what a run writes" has exactly one definition —
/// a spawned run and a hand-run one cannot drift into producing different artefacts.
fn finish(
    home: &Path,
    month: &str,
    options: &Options,
    http: &dyn Http,
    half: DriftHalf,
) -> Result<Outcome> {
    let mut outcome = Outcome {
        month: month.to_string(),
        scope: options.scope,
        first_run: half.first_run,
        drifts: half.drifts,
        snapshots: half
            .probed
            .iter()
            .map(|p| (p.snapshot.backend.clone(), p.snapshot.version.clone()))
            .collect(),
        skipped: half.skipped,
        sources: options.sources.len(),
        ..Outcome::default()
    };
    // A snapshot whose strings came from a prefix of the artefact has to say so where the
    // reader is, not only inside the JSON: the report's REA legend otherwise claims a
    // comparison over the whole file, and the header is the part a reader skims
    // (`DESIGN.md` §17).
    if options.scope.probes() {
        for probed in &half.probed {
            if let Some(note) = &probed.snapshot.strings_note {
                outcome
                    .notes
                    .push(format!("{}: {note}", probed.snapshot.backend));
            }
        }
    }

    if options.scope.fetches() {
        let digest = news_half(&options.sources, &options.stack, http);
        outcome.digest = digest.items;
        outcome.headlines = digest.headlines;
        outcome.sources_ok = digest.sources_ok;
        outcome.failed_sources = digest.failed;
    }

    if options.scope.writes_report() {
        let body = watch::render_report(month, &outcome.drifts, &outcome.digest, outcome.first_run);
        outcome.written = Some(write_report(home, month, &body)?);
    }

    Ok(outcome)
}

// ------------------------------------------------------------------ the drift half --

/// One backend's probe result, with the snapshot it replaces.
#[derive(Debug)]
struct Probed {
    /// The snapshot just taken.
    snapshot: CliSnapshot,
    /// The snapshot it replaced, when one was on disk. `None` is the first run.
    previous: Option<CliSnapshot>,
}

/// What one drift pass produced.
#[derive(Debug, Default)]
struct DriftHalf {
    /// The backends that answered.
    probed: Vec<Probed>,
    /// The backends that could not be probed, and why.
    skipped: Vec<(String, String)>,
    /// Every finding, across every backend.
    drifts: Vec<Drift>,
    /// True when no backend it probed had a previous snapshot: nothing was compared.
    first_run: bool,
    /// How many probes were attempted. The budget's unit — see the module docs.
    spawns: u64,
}

/// Probe every registered backend, persist each snapshot, and diff it against the one it
/// replaces.
///
/// Each backend is persisted immediately after it answers, so a later backend's failure
/// cannot discard evidence already taken. The diff is computed **before** the rotation, so
/// the file being replaced is what the finding was made against.
async fn drift_half(cfg: &Config, home: &Path, now_ms: u64) -> Result<DriftHalf> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let scrub = cfg.scrub_keys(cfg.scrub_enabled());
    let mut half = DriftHalf::default();

    for provider in provider::registry() {
        let id = provider.id().to_string();

        // Both halves of the probe must come from the trait: a hardcoded `--help` against
        // codex's launcher would compare *node's* help between runs and report the
        // difference as a change to codex (`DESIGN.md` §17).
        let (Some((version_program, version_args)), Some((help_program, help_args))) =
            (provider.version_command(cfg), provider.help_command(cfg))
        else {
            half.skipped.push((
                id,
                "no binary is configured for it, or none resolves on this machine".to_string(),
            ));
            continue;
        };

        half.spawns += 1;
        let Some(version_lines) = capture(&id, &version_program, &version_args, &cwd, &scrub).await
        else {
            half.skipped.push((
                id,
                format!(
                    "`{}` could not be run (not installed, or it refused to start)",
                    argv_label(&version_program, &version_args)
                ),
            ));
            continue;
        };

        // A version is the baseline pie's parsers were validated against (`ACCEPTANCE.md`
        // §2 I8). Recording an empty one would report a version *change* on the next run
        // instead of reporting that this probe is broken, so the backend is skipped with
        // the reason rather than snapshotted with a hole in it.
        let Some(version) = first_non_empty(&version_lines) else {
            half.skipped.push((
                id,
                "its version probe printed nothing, so a snapshot would record an empty \
                 baseline"
                    .to_string(),
            ));
            continue;
        };

        half.spawns += 1;
        let Some(help_lines) = capture(&id, &help_program, &help_args, &cwd, &scrub).await else {
            half.skipped.push((
                id,
                format!(
                    "`{}` could not be run (not installed, or it refused to start)",
                    argv_label(&help_program, &help_args)
                ),
            ));
            continue;
        };

        // The rule `DESIGN.md` §17 states: an empty capture is a diagnostic, not a quiet
        // snapshot. `render_report` sees only findings, so a snapshot with no help would
        // read as "no drift" on every later run — the one answer this job must never
        // invent — and nothing is written for this backend, so no baseline is silently
        // reset either.
        let help = help_lines.join("\n");
        if help.trim().is_empty() {
            return Err(PieError::protocol(
                id.as_str(),
                format!(
                    "`{}` produced no output — an empty capture is not a snapshot: \
                     `render_report` sees only findings, so an empty `help` would read as \
                     `no drift` on every later run (`DESIGN.md` §17)",
                    argv_label(&help_program, &help_args)
                ),
            ));
        }

        // Read before writing: this is the snapshot the finding is made against.
        let path = snapshot_path(home, &id);
        let previous = read_snapshot(&path)?;
        // The third leg: read the vendor's **own** artefact and keep the strings a CLI
        // change could plausibly move. A file read is not a spawn, so the probe count the
        // budget is enforced against is unchanged by this.
        let (strings, strings_note) = artefact_strings(&id, provider.artefact(cfg));

        let snapshot = CliSnapshot {
            backend: id.clone(),
            version,
            help,
            strings,
            strings_note,
            taken_ms: now_ms,
        };

        if let Some(previous) = &previous {
            half.drifts.extend(watch::diff(previous, &snapshot));
        }

        write_snapshot(home, &id, &snapshot)?;
        half.probed.push(Probed { snapshot, previous });
    }

    // A run that probed nothing has nothing to say about drift, and `render_report` has no
    // branch that can say it: its two no-findings sentences both claim a comparison
    // happened. So it is a stated error naming what was missing, rather than a report that
    // reads as a clean bill of health.
    if half.probed.is_empty() {
        let reasons = half
            .skipped
            .iter()
            .map(|(backend, why)| format!("{backend}: {why}"))
            .collect::<Vec<_>>()
            .join("; ");
        return Err(PieError::Config(format!(
            "no backend could be probed, so there is nothing to compare: {reasons}"
        )));
    }

    // "No previous snapshot for any backend probed" is the only honest meaning of
    // `first_run`, whose signature is a single bool for the whole run.
    half.first_run = half.probed.iter().all(|p| p.previous.is_none());
    Ok(half)
}

/// Capture a probe's stdout.
///
/// `None` means the probe could not be run at all — the CLI is not installed, or it
/// refused to start — which is an ordinary skip. `Some(lines)` means it ran, *including*
/// when it printed nothing: only the caller knows whether emptiness is a skip (a version)
/// or a diagnostic (the help that drift diffs).
///
/// The spawn goes through [`process::run_streaming`] and nothing else, so every probe
/// inherits the hidden window, the env scrub, the bounded stderr and the kill-tree.
async fn capture(
    backend: &str,
    program: &Path,
    args: &[OsString],
    cwd: &Path,
    scrub: &[String],
) -> Option<Vec<String>> {
    let spec = process::RunSpec {
        backend: backend.to_string(),
        program: program.to_path_buf(),
        args: args.to_vec(),
        cwd: cwd.to_path_buf(),
        scrub: scrub.to_vec(),
        timeout: PROBE_TIMEOUT,
    };

    let mut lines: Vec<String> = Vec::new();
    // The closure borrows `lines` mutably for the duration of the spawn, so it is dropped
    // explicitly before the post-run inspection below reads `lines` back. Relying on
    // non-lexical lifetimes here would compile today and break the moment anyone reorders
    // the match arms.
    let outcome = {
        let mut push = |line: &str| lines.push(line.to_string());
        process::run_streaming(&spec, &mut push).await
    };

    match outcome {
        Ok(summary) => {
            // **A CLI may print its usage to stderr.** Measured on this machine:
            // `agy --help` writes 2905 bytes to **stderr** and nothing at all to stdout,
            // because Go's flag package prints usage there. Capturing stdout alone made
            // every real `pie watch` run fail with "produced no output" — and had the
            // empty capture been treated as a snapshot instead of an error, it would have
            // recorded an empty help text and reported "no drift" forever afterwards.
            //
            // Only when stdout was empty: a CLI that writes to both has already given us
            // the content, and appending the stderr copy would put every line in the
            // snapshot twice. Deterministic either way, but noise in a diffed artefact is
            // noise a reader has to filter.
            if lines.is_empty() && !summary.stderr_tail.trim().is_empty() {
                lines.extend(summary.stderr_tail.lines().map(str::to_string));
            }
            Some(lines)
        }
        Err(e) => {
            // Debug rather than warn: a backend that is not installed is the common case on
            // a machine that drives two of the three CLIs, and the skip list names it.
            tracing::debug!(backend, error = %e, "probe could not be run");
            None
        }
    }
}

/// Read one backend's artefact and extract the strings a CLI change could plausibly move.
///
/// The second value is the stated reason the strings are not a complete picture, when
/// there is one: a read that stopped at the cap, no artefact at all, or a read that failed.
/// It travels into the snapshot, because a read that reports nothing is a false "no drift"
/// — the same failure B35 recorded for an empty `--help`, one leg over.
///
/// **Nothing here is fatal**, and that is deliberate. Version and `--help` are valid
/// evidence on their own, so an artefact that cannot be read degrades this leg and says so
/// rather than discarding the whole probe; skipping the backend would be over-correcting,
/// which is the shape `ACCEPTANCE.md` §13 B23 recorded.
///
/// The path is taken as an argument rather than resolved from the provider here, so the
/// method under test is the one the trait names and not a second resolution order that
/// could drift from it.
fn artefact_strings(backend: &str, path: Option<PathBuf>) -> (Vec<String>, Option<String>) {
    let Some(path) = path else {
        return (
            Vec::new(),
            Some(format!(
                "no artefact resolves for `{backend}`, so the binary-strings leg compared \
                 nothing"
            )),
        );
    };

    match read_artefact(&path) {
        Ok((strings, note)) => (strings, note),
        Err(error) => (
            Vec::new(),
            Some(format!(
                "{} could not be read ({error}), so the binary-strings leg compared nothing; \
                 version and `--help` still ran",
                path.display()
            )),
        ),
    }
}

/// How much of the artefact is read per chunk.
///
/// The bound that matters is on **memory**, not on how much is read. A chunk buffer of this
/// size plus the scanner's constant-sized state is the whole footprint, whatever the file's
/// size — so the read can go to EOF and still be an allocation pie chose (`ACCEPTANCE.md`
/// §13 B22). 1 MiB is large enough to keep the syscall count low on a 200 MB binary and
/// small enough that the buffer is not the memory the scan is trying to avoid.
const STREAM_CHUNK_BYTES: usize = 1024 * 1024;

/// Stream `path` to EOF in bounded chunks and extract the strings, stating when a genuine
/// read bound stopped the scan short.
///
/// This is the driver half of the leg; the selection itself is
/// [`watch::strings::StringScanner`], which is pure so every rule is tested against byte
/// slices. The scanner holds the partial run across chunk boundaries, so a string that
/// straddles two reads is found whole — the failure a per-chunk scan would produce on every
/// file, silently, as a missing or split string.
///
/// The note is not an apology but evidence: without it, a diff taken over a **prefix** is
/// indistinguishable from one taken over the whole file. For every real artefact it is
/// `None`, because [`watch::strings::MAX_ARTEFACT_BYTES`] is far above the vendor sizes and
/// the read reaches EOF.
fn read_artefact(path: &Path) -> std::io::Result<(Vec<String>, Option<String>)> {
    let started = std::time::Instant::now(); // TIMING-TEMP
    let mut file = std::fs::File::open(path)?;
    let total = file.metadata()?.len();
    let bound = watch::strings::MAX_ARTEFACT_BYTES;

    let mut scanner = watch::strings::StringScanner::new(watch::strings::MAX_STRINGS);
    let mut buffer = vec![0u8; STREAM_CHUNK_BYTES];
    let mut read: u64 = 0;

    loop {
        if read >= bound {
            break;
        }
        // Never read past the bound, so the last chunk does not over-read into bytes the
        // scan would then have to discard.
        let want = ((bound - read).min(buffer.len() as u64)) as usize;
        let filled = file.read(&mut buffer[..want])?;
        if filled == 0 {
            break;
        }
        // The chunk is fed as it arrives; nothing accumulates between chunks but the
        // scanner's own bounded state.
        scanner.feed(&buffer[..filled]);
        read += filled as u64;
    }

    let strings = scanner.finish();
    let note = (total > bound).then(|| {
        format!(
            "read the first {read} of {total} bytes of {}, because pie stops a scan at \
             {bound} bytes (the streaming read bound) — so the strings below describe that \
             prefix and not the whole artefact",
            path.display()
        )
    });

    eprintln!("TIMING read {} in {:?}", path.display(), started.elapsed()); // TIMING-TEMP
    Ok((strings, note))
}

// ------------------------------------------------------------------- the news half --

/// What one digest pass produced.
#[derive(Debug, Default)]
struct NewsHalf {
    /// The ranked items, failure records included.
    items: Vec<NewsItem>,
    /// How many headlines arrived.
    headlines: usize,
    /// How many sources answered.
    sources_ok: usize,
    /// The sources that failed, by name.
    failed: Vec<String>,
}

/// Fetch, and rank what arrived by rule.
///
/// A failure becomes a [`watch::failed_source`] entry rather than an empty list: the
/// digest's incompleteness has to survive into the report, because a digest that is
/// silently one source short reads as "nothing happened" (`DESIGN.md` §17 rule 2).
fn news_half(sources: &[NewsSource], stack: &[String], http: &dyn Http) -> NewsHalf {
    let mut half = NewsHalf::default();
    let mut items: Vec<NewsItem> = Vec::new();

    for source in sources {
        match news::fetch(source, http) {
            Ok(found) => {
                half.sources_ok += 1;
                half.headlines += found.len();
                items.extend(found);
            }
            Err(error) => {
                half.failed.push(source.name.clone());
                items.push(watch::failed_source(&source.name, &error.to_string()));
            }
        }
    }

    half.items = news::rank(items, stack);
    half
}

// ------------------------------------------------------------------- the filesystem --

/// The on-disk form of a [`CliSnapshot`].
///
/// Deliberately not `CliSnapshot` itself: that type is stage A's frozen shape and carries
/// no serde derives, and the on-disk form is this module's business — a field added to
/// `CliSnapshot` must not silently become a key an older pie would reject, or a key this
/// pie would silently lose. (`commands/schedule.rs` makes the same argument for its
/// ledger.)
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct SnapshotFile {
    backend: String,
    version: String,
    help: String,
    strings: Vec<String>,
    /// The read's own caveat, when it has one. `#[serde(default)]` on the struct is what
    /// lets a snapshot written before this key existed still be read as a baseline: an
    /// older file simply had a whole-prefix-less read, which is what `None` means.
    strings_note: Option<String>,
    taken_ms: u64,
}

impl SnapshotFile {
    /// The snapshot as it is written.
    fn of(snapshot: &CliSnapshot) -> Self {
        Self {
            backend: snapshot.backend.clone(),
            version: snapshot.version.clone(),
            help: snapshot.help.clone(),
            strings: snapshot.strings.clone(),
            strings_note: snapshot.strings_note.clone(),
            taken_ms: snapshot.taken_ms,
        }
    }

    /// The snapshot as it is read back.
    fn into_snapshot(self) -> CliSnapshot {
        CliSnapshot {
            backend: self.backend,
            version: self.version,
            help: self.help,
            strings: self.strings,
            strings_note: self.strings_note,
            taken_ms: self.taken_ms,
        }
    }
}

/// Read the snapshot a previous run left, if any.
///
/// A snapshot that is *unreadable* is an error, not "no previous snapshot". Treating it as
/// absent would report `first snapshot taken` for a file that exists and silently reset the
/// baseline on every run — the one outcome the first-run wording exists to prevent.
fn read_snapshot(path: &Path) -> Result<Option<CliSnapshot>> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };

    let file: SnapshotFile = serde_json::from_slice(&bytes).map_err(|e| {
        PieError::Config(format!(
            "{}: {e} — refusing to treat an unreadable snapshot as `there was none`, which \
             would report a first run for a file that exists and reset the drift baseline",
            path.display()
        ))
    })?;

    Ok(Some(file.into_snapshot()))
}

/// Persist one backend's snapshot, rotating the one it replaces.
fn write_snapshot(home: &Path, backend: &str, snapshot: &CliSnapshot) -> Result<()> {
    std::fs::create_dir_all(watch::snapshot_dir(home))?;

    let path = snapshot_path(home, backend);
    // The replaced snapshot stays readable, so both sides of a reported diff can be
    // inspected after the fact — a finding whose evidence has been overwritten is a finding
    // nobody can check.
    if path.is_file() {
        std::fs::rename(&path, previous_snapshot_path(home, backend))?;
    }

    let bytes = serde_json::to_vec_pretty(&SnapshotFile::of(snapshot))?;
    scratch_write(&path, &bytes)
}

/// Write the monthly report.
fn write_report(home: &Path, month: &str, body: &str) -> Result<PathBuf> {
    let path = report_path(home, month);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    scratch_write(&path, body.as_bytes())?;
    Ok(path)
}

/// Write `bytes` to `path` through a scratch file and a rename.
///
/// The rename is the point (`memory.rs` and `commands/schedule.rs` make the same
/// argument): a plain overwrite that dies half way leaves a file that parses as a
/// *different* state — a half-written report reads as a month with no findings, and a
/// half-written snapshot becomes the baseline the next run diffs against.
fn scratch_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let scratch = scratch_path(path);

    if let Err(e) = std::fs::write(&scratch, bytes) {
        let _ = std::fs::remove_file(&scratch);
        return Err(e.into());
    }
    if let Err(e) = std::fs::rename(&scratch, path) {
        let _ = std::fs::remove_file(&scratch);
        return Err(e.into());
    }
    Ok(())
}

/// The scratch path beside `path`.
fn scratch_path(path: &Path) -> PathBuf {
    match path.file_name() {
        Some(name) => {
            let mut scratch = name.to_os_string();
            scratch.push(SCRATCH_SUFFIX);
            path.with_file_name(scratch)
        }
        // A path with no file name cannot be written to at all; returning it unchanged
        // makes the write fail loudly rather than inventing a name somewhere else.
        None => path.to_path_buf(),
    }
}

/// Whether a snapshot from an earlier run already exists for any registered backend.
///
/// The dry run's prediction of `first_run`: a real run decides it over the backends it
/// actually probed, which is why this is a separate, cheaper question rather than a shared
/// answer.
fn a_snapshot_is_already_on_file(home: &Path) -> bool {
    provider::registry()
        .iter()
        .any(|provider| snapshot_path(home, provider.id()).is_file())
}

// ---------------------------------------------------------------- the scheduled job --

/// The scheduled job as `config.toml` describes it.
///
/// The cadence comes from the configuration rather than from this module, so the shipped
/// `never` is what makes the job not due — a job cannot become scheduled by being installed
/// (`DESIGN.md` §16). There is no `watch_cadence` key yet; the learn job's keys are the
/// only scheduled-job switches pie has, and M7/M8 share one opt-in story.
fn watch_job(cfg: &Config) -> Job {
    Job {
        id: WATCH_JOB.to_string(),
        cadence: cfg.jobs.learn_cadence.to_schedule(),
        estimate_tokens: probe_estimate(cfg),
    }
}

/// What the scheduled run is expected to cost, in the budget's own unit.
///
/// One unit per planned probe — **spawns, not model tokens**. A `--help` spends no model
/// tokens at all (`DESIGN.md` §17, which is why drift is safe to run on a schedule), but it
/// is a process pie starts, and the one gate pie has that refuses *before* a spawn is
/// `schedule::check_budget`. An estimate of `0` would make that gate decorative: the
/// shipped budget of `0` would allow the spawn, which is exactly the "gate that cannot
/// fail" shape B9 and B20 recorded. This is a contract note, not a silent decision.
fn probe_estimate(cfg: &Config) -> u64 {
    let probed = provider::registry()
        .iter()
        .filter(|provider| provider.help_command(cfg).is_some())
        .count() as u64;

    (probed * PROBES_PER_BACKEND).max(1)
}

/// The ledger as it is serialised. Not [`Ledger`] itself, for the reason
/// `SnapshotFile` is not `CliSnapshot`.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct LedgerFile {
    last_run_ms: Option<u64>,
    spent: u64,
}

/// `$PIE_HOME/watch/schedule.json`
fn ledger_path(home: &Path) -> PathBuf {
    watch::snapshot_dir(home).join(LEDGER_FILE)
}

/// Read the scheduled job's ledger, overlaid on the configured budget.
///
/// A missing file is a fresh install: nothing has run and nothing has been spent. A
/// *damaged* file is an error, for the reason `commands/schedule.rs` gives — treating a
/// corrupt ledger as "nothing spent" hands the whole window's budget back, which is the
/// silent spend `RISKS.md` §13 names.
fn read_ledger(cfg: &Config, home: &Path) -> Result<Ledger> {
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

/// Persist the scheduled job's ledger, through the same scratch-and-rename.
fn write_ledger(ledger: &Ledger, home: &Path) -> Result<()> {
    std::fs::create_dir_all(watch::snapshot_dir(home))?;

    let file = LedgerFile {
        last_run_ms: ledger.last_run_ms,
        spent: ledger.budget.spent,
    };
    let bytes = serde_json::to_vec_pretty(&file)?;
    scratch_write(&ledger_path(home), &bytes)
}

// ---------------------------------------------------------------- the clock and argv --

/// `YYYY-MM` from an epoch-millisecond instant.
///
/// Integer arithmetic, no formatter and no timezone database — the same rule
/// `memory.rs::date_of` follows, and for the same reason: a locale-dependent rendering
/// would name a different file on different machines. Duplicated from `memory.rs` (where
/// it is private) rather than shared, which is the cost of `render_report` taking a
/// `&str`.
fn month_of(now_ms: u64) -> String {
    let (year, month, _day) = civil_from_days((now_ms / MS_PER_DAY) as i64);
    format!("{year:04}-{month:02}")
}

/// Validate an injected `--month`.
///
/// A typo (`2026-1`, `2026-13`, `next-month`) must be a config error rather than a file
/// name, because `watchtower-2026-1.md` is a report nobody will ever look for and a cron
/// entry that names it would look like it worked.
fn checked_month(month: &str) -> Result<String> {
    // `is_ascii` first, so every slice below is at a character boundary: a multi-byte
    // string of the right *byte* length would otherwise panic in `checked_month` (I6).
    let shape_ok = month.is_ascii()
        && month.len() == MONTH_LEN
        && month.as_bytes()[4] == b'-'
        && month[..4].bytes().all(|b| b.is_ascii_digit())
        && month[5..].bytes().all(|b| b.is_ascii_digit())
        && month[5..]
            .parse::<u32>()
            .is_ok_and(|m| (1..=12).contains(&m));

    if !shape_ok {
        return Err(PieError::Config(format!(
            "`--month {month}` is not a month: pass `YYYY-MM` (for example `2026-11`), \
             because the report's file name is built from it"
        )));
    }

    Ok(month.to_string())
}

/// Days since the Unix epoch → civil date (Howard Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month: u32 = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = if month <= 2 { year + 1 } else { year };
    (year, month, day)
}

/// The first line that is not blank.
fn first_non_empty(lines: &[String]) -> Option<String> {
    lines
        .iter()
        .map(|line| line.trim())
        .find(|line| !line.is_empty())
        .map(str::to_string)
}

/// `program --arg`, for a diagnostic that names the argv a probe would have used.
///
/// Nothing here is a secret: a probe argv carries no prompt and no credential.
fn argv_label(program: &Path, args: &[OsString]) -> String {
    let mut out = program.display().to_string();
    for arg in args {
        out.push(' ');
        out.push_str(&arg.to_string_lossy());
    }
    out
}

// ----------------------------------------------------------------------------- HTTP --

/// The real transport: one GET.
///
/// `DESIGN.md` §17 freezes `Http::get` as a synchronous method while the client pie has is
/// async, so the call is bridged the way the opt-in model review is (`commands/learn.rs`):
/// `block_in_place` needs the multi-threaded runtime `main.rs` starts. An unavailable
/// transport is a **stated** error — never an empty body, which `news::parse` would report
/// as a source with no news.
#[cfg(feature = "http")]
#[derive(Debug)]
pub struct NetworkHttp;

#[cfg(feature = "http")]
impl Http for NetworkHttp {
    fn get(&self, url: &str) -> Result<String> {
        /// How long one feed may take. A monthly job must not hang on a feed that never
        /// answers.
        const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

        let request = async {
            let client = reqwest::Client::builder()
                .timeout(REQUEST_TIMEOUT)
                .build()?;
            let response = client.get(url).send().await?;
            let status = response.status();
            let body = response.text().await?;

            if !status.is_success() {
                return Err(PieError::NotReachable(format!(
                    "{url} answered HTTP {}",
                    status.as_u16()
                )));
            }
            Ok(body)
        };

        tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(request))
    }
}

/// The same type without a TLS transport.
///
/// `--no-default-features` drops `reqwest` (gate G6), so the command must still compile and
/// must still answer *honestly*: a config error naming the missing transport, never an
/// empty body.
#[cfg(not(feature = "http"))]
#[derive(Debug)]
pub struct NetworkHttp;

#[cfg(not(feature = "http"))]
impl Http for NetworkHttp {
    fn get(&self, url: &str) -> Result<String> {
        Err(PieError::Config(format!(
            "this build has no TLS transport (`http` was disabled), so {url} cannot be \
             fetched — the digest is incomplete rather than empty"
        )))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    /// The month is derived from the instant the caller passes, never from the clock.
    #[test]
    fn the_month_comes_from_the_injected_instant() {
        // 1_700_000_000_000 ms is 2023-11-14T22:13:20Z.
        assert_eq!(month_of(1_700_000_000_000), "2023-11");
        // 1_735_689_600_000 ms is 2025-01-01T00:00:00Z — a month boundary, where an
        // off-by-one in the day arithmetic would show.
        assert_eq!(month_of(1_735_689_600_000), "2025-01");
        assert_eq!(month_of(0), "1970-01");
    }

    #[test]
    fn a_month_flag_is_validated_rather_than_used_as_a_file_name() {
        assert_eq!(checked_month("2026-11").unwrap(), "2026-11");
        assert_eq!(checked_month("2026-01").unwrap(), "2026-01");

        for bad in [
            "2026-1",
            "2026-13",
            "2026-00",
            "202611",
            "next-month",
            "",
            "2026/11",
        ] {
            let error = checked_month(bad).expect_err(bad);
            assert!(matches!(error, PieError::Config(_)), "{bad}: {error}");
        }

        // A multi-byte string of the right *byte* length must be refused rather than
        // sliced through (I6).
        assert!(checked_month("é026-11").is_err());
        assert!(checked_month("2026-1é").is_err());
    }

    #[test]
    fn the_scratch_file_sits_beside_its_target() {
        let path = Path::new("/state/reports/watchtower-2026-11.md");
        assert_eq!(
            scratch_path(path),
            Path::new("/state/reports/watchtower-2026-11.md.tmp"),
            "the rename has to be same-volume, so the scratch file cannot be elsewhere"
        );
    }

    #[test]
    fn only_the_full_scope_writes_a_report() {
        assert!(Scope::All.writes_report());
        assert!(!Scope::Drift.writes_report());
        assert!(!Scope::News.writes_report());

        // The default is the scope the roadmap's Done condition describes.
        assert_eq!(Scope::default(), Scope::All);
    }
}
