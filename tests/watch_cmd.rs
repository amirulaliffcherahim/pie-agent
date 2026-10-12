//! M8 stage D gate: `pie watch`, the command that produces the milestone's artefact.
//!
//! Contract: `planning/DESIGN.md` §17; criteria: `planning/ACCEPTANCE.md` §16 (row
//! "D command"). The gate is `cargo test --test watch_cmd`.
//!
//! **Nothing here reaches the network and nothing here spawns a vendor CLI.** The two
//! seams that make that true are the ones the earlier stages built for exactly this
//! reason: `watch::news::Http` is a trait, and a probe is a process whose *argv* comes from
//! `Provider::version_command`/`help_command` — so a backend's `bin` can be pointed at a
//! local stand-in, and the stand-in counts its own invocations.
//!
//! That count is the point of the first three tests. `DESIGN.md` §16's cross-cutting claim
//! is that with the shipped configuration a scheduled run performs **zero** spawns, and a
//! claim about *how many times* something happened can only be asserted by counting it —
//! "no error came back" is the decorative gate this project has already shipped twice.
//!
//! `HOME`/`USERPROFILE` are never inherited for anything asserted here: `execute` and
//! `execute_scheduled` take the home directory as a parameter, so no test in this file
//! depends on the machine it runs on (gate **G3d**, after B32 — a suite that is green
//! because the developer's machine has a vendor login is green for a reason nobody can
//! reproduce). The one exception is the test that compares the runner's report path
//! against `watch::reports_dir()`, which by definition reads `PIE_HOME`, and it pins that
//! variable itself.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};

use pie::commands::watch::{self, Options, Scope};
use pie::config::{Config, JobCadence};
use pie::error::{PieError, Result};
use pie::watch::news::{Http, NewsSource};
use pie::watch::DriftClass;

/// A fixed clock reading: **2023-11-14T22:13:20Z**.
///
/// Decades in the past on purpose. A timestamp near *now* would make a clock read
/// somewhere in the pipeline indistinguishable from the month being derived from the
/// instant the caller passed, which is the whole claim the month tests make.
const TS: u64 = 1_700_000_000_000;

/// The month `TS` falls in, written out rather than computed so the expectation is not a
/// second copy of the code under test.
const TS_MONTH: &str = "2023-11";

/// A second instant, in a different month and at a month boundary
/// (2025-01-01T00:00:00Z).
const NEW_YEAR: u64 = 1_735_689_600_000;

const RSS_URL: &str = "https://blog.rust-lang.org/feed.xml";
const ARXIV_URL: &str = "https://export.arxiv.org/rss/cs.SE";

/// An RSS 2.0 feed with one headline, kept minimal because stage B already owns feed
/// parsing: this file is about what the *command* does with a body, not with a shape.
const RSS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0">
  <channel>
    <title>Rust Blog</title>
    <link>https://blog.rust-lang.org/</link>
    <item>
      <title>Async closures land in Rust 1.87</title>
      <link>https://blog.rust-lang.org/2026/01/14/async-closures.html</link>
    </item>
  </channel>
</rss>"#;

/// The help text the stand-in prints when it is asked a question it can answer.
const HELP: &str = "usage: agy [--print <PROMPT>] [--help]\n\n  --print   run one turn\n  --help    show this help\n";

// ------------------------------------------------------------------------- the stand-in --

/// A stand-in `agy`: it records every invocation, then answers `--version` and `--help`
/// from files the test controls.
///
/// The recording is the whole point. It distinguishes "no probe happened" from "a probe
/// happened and returned something harmless", which no assertion on the output alone can
/// do — and it records the *argv*, which is how `ACCEPTANCE.md` §16's "the probe argv
/// contains no turn flag" is checked on the wire instead of by inspection.
#[derive(Debug)]
struct StandIn {
    _dir: tempfile::TempDir,
    script: PathBuf,
    calls: PathBuf,
    argv: PathBuf,
}

impl StandIn {
    fn new(version: &str, help: &str) -> Self {
        Self::with_trailing(version, help, "")
    }

    /// A stand-in whose script carries `trailing` bytes after an early `exit`.
    ///
    /// The shell never reads past the `exit`, so the bytes are dead to the probe and alive
    /// to the artefact reader — which is how a test makes the artefact larger than one read
    /// chunk without writing a script that executes megabytes.
    fn with_trailing(version: &str, help: &str, trailing: &str) -> Self {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let calls = dir.path().join("calls.txt");
        let argv_record = dir.path().join("argv.txt");
        let version_file = dir.path().join("version.txt");
        let help_file = dir.path().join("help.txt");

        std::fs::write(&version_file, format!("{version}\n")).expect("write the version");
        std::fs::write(&help_file, help).expect("write the help text");

        #[cfg(windows)]
        let script = dir.path().join("counting-agy.bat");
        #[cfg(not(windows))]
        let script = dir.path().join("counting-agy.sh");

        #[cfg(windows)]
        let body = format!(
            "@echo off\r\n\
             echo %* >> \"{argv}\"\r\n\
             echo call >> \"{calls}\"\r\n\
             if \"%1\"==\"--version\" type \"{version}\"\r\n\
             if not \"%1\"==\"--version\" type \"{help}\"\r\n\
             exit /b 0\r\n",
            argv = argv_record.display(),
            calls = calls.display(),
            version = version_file.display(),
            help = help_file.display(),
        );
        #[cfg(not(windows))]
        let body = format!(
            "#!/bin/sh\necho \"$@\" >> '{argv}'\necho call >> '{calls}'\n\
             if [ \"$1\" = \"--version\" ]; then cat '{version}'; else cat '{help}'; fi\nexit 0\n",
            argv = argv_record.display(),
            calls = calls.display(),
            version = version_file.display(),
            help = help_file.display(),
        );

        std::fs::write(&script, format!("{body}{trailing}")).expect("write the counting stand-in");

        #[cfg(not(windows))]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
                .expect("chmod the counting stand-in");
        }

        Self {
            _dir: dir,
            script,
            calls,
            argv: argv_record,
        }
    }

    /// How many times it ran. A missing counter is zero runs, not an error.
    fn calls(&self) -> usize {
        std::fs::read_to_string(&self.calls)
            .map(|text| text.lines().count())
            .unwrap_or(0)
    }

    /// Every invocation's argv, one line each.
    fn argv(&self) -> String {
        std::fs::read_to_string(&self.argv).unwrap_or_default()
    }
}

/// A config whose **only** probeable backend is `stand_in`.
///
/// codex and claude are pointed at paths that do not exist **and** the node shim is turned
/// off. `codex::program_and_prefix` falls back to `node <codex.js>` when the configured
/// binary is missing, so a test that moved only `codex.bin` would spawn the developer's
/// real codex on a machine that has one — the machine-dependence defect B32 recorded, and
/// the reason gate G3d exists.
fn config_with(stand_in: &Path) -> Config {
    let mut cfg = Config::default();
    cfg.agy.bin = stand_in.display().to_string();
    cfg.codex.bin = missing(stand_in, "codex").display().to_string();
    cfg.codex.use_node_shim = false;
    cfg.claude.bin = missing(stand_in, "claude").display().to_string();
    cfg
}

/// A sibling path that does not exist, so a probe of that backend is a skip rather than a
/// spawn of whatever the machine happens to have installed.
fn missing(beside: &Path, name: &str) -> PathBuf {
    beside.with_file_name(format!("pie-test-missing-{name}"))
}

/// A config in which **no** backend can be probed.
fn config_with_no_backend() -> Config {
    let mut cfg = Config::default();
    cfg.agy.bin = "pie-test-missing-agy".to_string();
    cfg.codex.bin = "pie-test-missing-codex".to_string();
    cfg.codex.use_node_shim = false;
    cfg.claude.bin = "pie-test-missing-claude".to_string();
    cfg
}

// ----------------------------------------------------------------------- the transport --

/// A transport over recorded bodies. Nothing in this file reaches the network.
#[derive(Debug)]
struct FakeHttp {
    bodies: Vec<(String, String)>,
    calls: Mutex<Vec<String>>,
}

impl FakeHttp {
    /// Answer `url` with `body`; refuse anything else.
    fn answering(url: &str, body: &str) -> Self {
        Self {
            bodies: vec![(url.to_string(), body.to_string())],
            calls: Mutex::new(Vec::new()),
        }
    }

    /// Refuse every URL.
    fn refusing() -> Self {
        Self {
            bodies: Vec::new(),
            calls: Mutex::new(Vec::new()),
        }
    }

    /// Every URL asked for.
    fn calls(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

impl Http for FakeHttp {
    fn get(&self, url: &str) -> Result<String> {
        self.calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(url.to_string());

        self.bodies
            .iter()
            .find(|(known, _)| known == url)
            .map(|(_, body)| body.clone())
            .ok_or_else(|| PieError::NotReachable(format!("{url}: refused by the test transport")))
    }
}

// ------------------------------------------------------------------------- the helpers --

fn source(name: &str, url: &str) -> NewsSource {
    NewsSource {
        name: name.to_string(),
        url: url.to_string(),
    }
}

fn options_asking_for(sources: Vec<NewsSource>) -> Options {
    Options {
        sources,
        // A stack, so the ranking rule has something to score against and a digest that
        // arrived is a digest in which something *matched*. Empty would rank everything at
        // zero, which is a rule stage B already owns.
        stack: vec!["rust".to_string()],
        ..Options::default()
    }
}

/// Every file name in `dir`, sorted. A missing directory is an empty directory.
fn entries(dir: &Path) -> Vec<String> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };

    let mut names: Vec<String> = read
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// The written report, read back.
fn read_report(outcome: &watch::Outcome) -> String {
    let path = outcome
        .written
        .as_ref()
        .expect("a real `all` run writes the artefact the Done condition names");
    std::fs::read_to_string(path).expect("read the written report")
}

/// Points `PIE_HOME` at a scratch directory for a block.
///
/// `src/testing.rs`'s `EnvGuard` is `pub(crate)`, so an integration test cannot use it;
/// this is the shape `tests/memory_cmd.rs` uses, with its own lock so this binary never
/// contends with another. Held for the shortest possible scope: a guard taken twice on one
/// thread would deadlock the binary (B24). Only one test needs it — everything else passes
/// its home as a parameter.
#[derive(Debug)]
struct EnvPin {
    saved: Vec<(&'static str, Option<String>)>,
    _guard: MutexGuard<'static, ()>,
}

impl EnvPin {
    fn pie_home(home: &Path) -> Self {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        let guard = LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        let saved = vec![("PIE_HOME", std::env::var("PIE_HOME").ok())];
        std::env::set_var("PIE_HOME", home);

        Self {
            saved,
            _guard: guard,
        }
    }
}

impl Drop for EnvPin {
    fn drop(&mut self) {
        for (key, value) in &self.saved {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

// ------------------------------------------------- the default path spends nothing -----

/// `ACCEPTANCE.md` §16's cross-cutting claim, asserted by **count**.
///
/// With the shipped configuration a scheduled run performs zero spawns, writes no file and
/// fetches nothing — and, because it never wrote a ledger or a snapshot, it leaves no trace
/// at all. The stand-in would have recorded every call; the test after this one proves it
/// can, by making calls happen.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_default_config_makes_a_scheduled_run_probe_nothing_and_write_nothing() {
    let home = tempfile::tempdir().expect("a scratch home");
    let stand_in = StandIn::new("agy version 9.9.9", HELP);
    let cfg = config_with(&stand_in.script);

    let outcome = watch::execute_scheduled(&cfg, home.path(), &FakeHttp::refusing(), TS)
        .await
        .expect("a job that is not due is not a failure");

    assert!(
        outcome.not_due,
        "the shipped cadence is `never`, so nothing is due"
    );
    assert_eq!(
        stand_in.calls(),
        0,
        "`pie watch` with the default config must reach no CLI at all"
    );
    assert!(
        !home.path().join("watch").exists(),
        "nothing was probed, so there is no snapshot, no ledger and no directory"
    );
    assert!(
        !home.path().join("reports").exists(),
        "and nothing was reported"
    );
    assert!(outcome.written.is_none(), "{:?}", outcome.written);

    // I7: a probe is not a turn, so it logs no session row. The file is not merely
    // unchanged — it does not exist, because nothing in this path writes one.
    assert!(
        !home.path().join("usage.jsonl").exists(),
        "no M8 command path may write a session-log row"
    );

    // And the run said so, rather than exiting silently: a job that does nothing silently
    // is indistinguishable from one that is broken.
    let rendered = outcome.render();
    assert!(rendered.contains("not due"), "{rendered}");
    assert!(rendered.contains("nothing was probed"), "{rendered}");
}

/// The budget gate on the path where it matters: an opted-in cadence with the shipped
/// budget of `0` is refused **before** the first spawn.
///
/// `DESIGN.md` §16: "a job whose estimate exceeds the budget is refused before any spawn,
/// so a budget of 0 cannot be exceeded by one call". The count is what makes that a fact
/// about the order rather than a hope about the intent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_scheduled_run_the_zero_budget_cannot_cover_is_refused_before_the_first_probe() {
    let home = tempfile::tempdir().expect("a scratch home");
    let stand_in = StandIn::new("agy version 9.9.9", HELP);
    let mut cfg = config_with(&stand_in.script);
    // Opted in to a cadence, and nothing else: the budget keeps its shipped `0`.
    cfg.jobs.learn_cadence = JobCadence::Daily;

    let error = watch::execute_scheduled(&cfg, home.path(), &FakeHttp::refusing(), TS)
        .await
        .expect_err("a run nothing can afford must be refused, not silently trimmed");

    assert!(matches!(error, PieError::Config(_)), "{error}");
    assert!(
        error.to_string().contains(watch::WATCH_JOB),
        "the refusal has to name the job, because M8 is the second consumer of one budget: {error}"
    );
    assert!(error.to_string().contains("budgeted"), "{error}");
    assert_eq!(
        stand_in.calls(),
        0,
        "the refusal must happen before the spawn, or the budget is a report rather than a gate"
    );
    assert!(
        !home.path().join("reports").exists(),
        "a refused run writes nothing"
    );
}

/// The control for the two tests above: with an opted-in cadence **and** a budget that
/// covers the job, the same sentinel records two probes and the report is written.
///
/// Without this, "zero spawns" would be indistinguishable from "this stand-in does not
/// count" — the shape of evidence `HANDOFF.md` §9 asks for.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_scheduled_run_with_an_opted_in_cadence_and_budget_probes_and_writes_the_report() {
    let home = tempfile::tempdir().expect("a scratch home");
    let stand_in = StandIn::new("agy version 9.9.9", HELP);
    let mut cfg = config_with(&stand_in.script);
    cfg.jobs.learn_cadence = JobCadence::Daily;
    cfg.jobs.learn_budget_tokens = 64;

    let outcome = watch::execute_scheduled(&cfg, home.path(), &FakeHttp::refusing(), TS)
        .await
        .expect("an opted-in run with a budget that covers it must run");

    assert!(!outcome.not_due);
    assert_eq!(
        stand_in.calls(),
        2,
        "one version probe and one help probe — the two the snapshot is built from"
    );
    assert_eq!(outcome.snapshots.len(), 1, "{:?}", outcome.snapshots);
    assert_eq!(outcome.snapshots[0].0, "agy");
    assert!(
        outcome.snapshots[0].1.contains("9.9.9"),
        "the snapshot records the CLI's own version: {:?}",
        outcome.snapshots
    );

    let reports = entries(&home.path().join("reports"));
    assert_eq!(
        reports,
        vec![format!("watchtower-{TS_MONTH}.md")],
        "a scheduled run writes the same single artefact a manual one does"
    );

    // The window's spend is recorded, so a second run inside it spends the remainder
    // rather than the whole budget again (`DESIGN.md` §16, stage C's ruling). The ledger is
    // the watch job's own file: a probe count must never be added to the learn job's token
    // accounting.
    let ledger = std::fs::read_to_string(home.path().join("watch").join("schedule.json"))
        .expect("write the watch job's own ledger");
    let ledger: serde_json::Value = serde_json::from_str(&ledger).expect("a readable ledger");
    assert_eq!(ledger["spent"], 2, "{ledger}");
    assert!(ledger["last_run_ms"].as_u64().is_some(), "{ledger}");
}

// ----------------------------------------------------------- what a run writes --------

/// `ACCEPTANCE.md` §16: "`--dry-run` writes no file". Asserted by comparing the reports
/// directory before and after, and strengthened: a dry run probes nothing either, because
/// "run with `--dry-run` to see what it would do" is only honest if looking is free
/// (`DESIGN.md` §16, M7's rule).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dry_run_writes_no_file_and_probes_nothing() {
    let home = tempfile::tempdir().expect("a scratch home");
    let stand_in = StandIn::new("agy version 9.9.9", HELP);
    let cfg = config_with(&stand_in.script);

    let before = entries(&home.path().join("reports"));
    let options = Options {
        dry_run: true,
        ..options_asking_for(vec![source("Rust Blog", RSS_URL)])
    };

    let outcome = watch::execute(
        &cfg,
        home.path(),
        &options,
        &FakeHttp::answering(RSS_URL, RSS),
        TS,
    )
    .await
    .expect("a dry run decides and reports");

    assert_eq!(
        before,
        entries(&home.path().join("reports")),
        "a dry run must not create or modify a single file"
    );
    assert!(
        !home.path().join("reports").exists(),
        "a dry run must not even create the reports directory"
    );
    assert_eq!(stand_in.calls(), 0, "a dry run spawns nothing");
    assert!(outcome.written.is_none());

    // …and it has to have said what it would do, or "writes nothing" would hold just as
    // well for a command that did nothing at all.
    let rendered = outcome.render();
    assert!(rendered.contains("DRY RUN"), "{rendered}");
    assert!(
        rendered.contains(&format!("watchtower-{TS_MONTH}.md")),
        "the dry run must name the file it would write:\n{rendered}"
    );
    assert!(rendered.contains("source(s)"), "{rendered}");
}

/// A real run writes **exactly one** file under `$PIE_HOME/reports`, and its name comes
/// from the month it was told, not from the clock.
///
/// Two months from one call site is the assertion that matters: a file name derived from
/// the system date can only be tested in the current month, which is exactly where a date
/// bug hides (`watch::render_report`'s own rule, carried into the runner).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_real_run_writes_exactly_one_file_named_from_the_injected_month() {
    let home = tempfile::tempdir().expect("a scratch home");
    let stand_in = StandIn::new("agy version 9.9.9", HELP);
    let cfg = config_with(&stand_in.script);

    for (month, expected) in [
        ("2026-11", "watchtower-2026-11.md"),
        ("2026-12", "watchtower-2026-12.md"),
    ] {
        let options = Options {
            month: Some(month.to_string()),
            ..Options::default()
        };
        let outcome = watch::execute(&cfg, home.path(), &options, &FakeHttp::refusing(), TS)
            .await
            .expect("a real run");

        let written = outcome.written.expect("a real run writes the artefact");
        assert_eq!(
            written.file_name().unwrap().to_string_lossy(),
            expected,
            "the file must be named from the injected month"
        );
    }

    // One file per run, no scratch file left behind, and nothing named for the month the
    // clock happens to be in.
    assert_eq!(
        entries(&home.path().join("reports")),
        vec![
            "watchtower-2026-11.md".to_string(),
            "watchtower-2026-12.md".to_string()
        ],
        "each run writes exactly one file, and the scratch file is renamed away"
    );
}

/// With no `--month`, the month comes from the instant the caller passes — never from the
/// system clock. `execute` has no other source for it, which is what this asserts.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_month_comes_from_the_injected_clock_when_no_flag_is_given() {
    let home = tempfile::tempdir().expect("a scratch home");
    let stand_in = StandIn::new("agy version 9.9.9", HELP);
    let cfg = config_with(&stand_in.script);

    let outcome = watch::execute(
        &cfg,
        home.path(),
        &Options::default(),
        &FakeHttp::refusing(),
        NEW_YEAR,
    )
    .await
    .expect("a real run");

    assert_eq!(
        outcome.month, "2025-01",
        "a month boundary is where this breaks"
    );
    assert_eq!(
        entries(&home.path().join("reports")),
        vec!["watchtower-2025-01.md".to_string()]
    );
}

/// An injected month that is not a month is a **config error**, not a file name.
///
/// A typo that becomes a file name produces `watchtower-2026-1.md`, which nothing will ever
/// look for, and a scheduled entry that writes it looks like it worked.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_injected_month_that_is_not_a_month_is_refused_rather_than_written() {
    let home = tempfile::tempdir().expect("a scratch home");
    let stand_in = StandIn::new("agy version 9.9.9", HELP);
    let cfg = config_with(&stand_in.script);

    for bad in ["2026-1", "2026-13", "next-month"] {
        let options = Options {
            month: Some(bad.to_string()),
            ..Options::default()
        };
        let error = watch::execute(&cfg, home.path(), &options, &FakeHttp::refusing(), TS)
            .await
            .expect_err("a month flag is validated");

        assert!(matches!(error, PieError::Config(_)), "{bad}: {error}");
        assert!(error.to_string().contains("YYYY-MM"), "{bad}: {error}");
    }

    assert!(
        !home.path().join("reports").exists(),
        "nothing may be written from an unusable month"
    );
    assert_eq!(stand_in.calls(), 0, "the refusal costs nothing");
}

/// The runner's report path is the one the frozen `watch` module names, and the report
/// body names the same file it was written as.
///
/// This is the seam a copy of a name lives in: `report_path` builds the name here because
/// `render_report` renders it rather than exposing it. Comparing the two is what stops the
/// copy from drifting — the same class of defect as a label written twice.
#[test]
fn the_report_path_is_the_one_the_frozen_module_names() {
    let home = tempfile::tempdir().expect("a scratch home");

    {
        // `watch::reports_dir()` reads `PIE_HOME` at call time, so the comparison pins it.
        // Scoped in its own block, and no other test in this binary depends on `PIE_HOME`.
        let _env = EnvPin::pie_home(home.path());

        assert_eq!(
            watch::report_path(home.path(), "2026-11"),
            pie::watch::reports_dir().join("watchtower-2026-11.md"),
            "the runner must write where the roadmap says it writes"
        );
        assert_eq!(
            pie::watch::reports_dir(),
            pie::paths::reports_dir(),
            "and the two frozen spellings of that directory must agree"
        );
    }

    let body = pie::watch::render_report("2026-11", &[], &[], false);
    let name = watch::report_path(home.path(), "2026-11")
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert!(
        body.contains(&name),
        "the report must name the file it is written as:\n{body}"
    );
}

/// `ACCEPTANCE.md` §16: "`--help` probing costs **no** model tokens: assert the probe argv
/// contains no turn flag, and that a probe run logs no session row (I7)".
///
/// The argv is read off the wire — the stand-in records what it was invoked with — because
/// that is the only way to assert what pie actually passed rather than what it meant to.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_probe_argv_carries_no_turn_flag_and_a_probe_run_writes_no_session_row() {
    let home = tempfile::tempdir().expect("a scratch home");
    let stand_in = StandIn::new("agy version 9.9.9", HELP);
    let cfg = config_with(&stand_in.script);

    watch::execute(
        &cfg,
        home.path(),
        &Options::default(),
        &FakeHttp::refusing(),
        TS,
    )
    .await
    .expect("a real run");

    let argv = stand_in.argv();
    assert!(argv.contains("--version"), "{argv}");
    assert!(argv.contains("--help"), "{argv}");
    assert_eq!(
        argv.lines().count(),
        2,
        "exactly the two probes, and no third spawn: {argv}"
    );

    // A turn flag would mean pie had asked for an answer, which is the thing the whole
    // schedule is built to avoid.
    for turn_flag in ["--print", "exec", "--prompt"] {
        assert!(
            !argv.contains(turn_flag),
            "a `--help` is not a turn, so the probe argv must carry no `{turn_flag}`: {argv}"
        );
    }

    assert!(
        !home.path().join("usage.jsonl").exists(),
        "I7: a probe writes no session-log row"
    );
}

/// The shipped configuration fetches nothing, and it says so in the report instead of
/// leaving the digest section out.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_default_config_fetches_no_source() {
    let home = tempfile::tempdir().expect("a scratch home");
    let stand_in = StandIn::new("agy version 9.9.9", HELP);
    let cfg = config_with(&stand_in.script);
    // A transport that *would* answer, so "nothing was fetched" is a fact about the
    // configuration rather than about the transport.
    let http = FakeHttp::answering(RSS_URL, RSS);

    let outcome = watch::execute(&cfg, home.path(), &Options::default(), &http, TS)
        .await
        .expect("a real run");

    assert!(
        http.calls().is_empty(),
        "no source is configured, so nothing may be fetched: {:?}",
        http.calls()
    );
    assert_eq!(outcome.sources, 0);
    assert_eq!(outcome.headlines, 0);

    let body = read_report(&outcome);
    assert!(
        body.contains("no headlines this month"),
        "the report must account for the digest rather than omitting the section:\n{body}"
    );
    assert!(!body.contains("INCOMPLETE"), "{body}");
}

/// A version that moved between two runs is reported, with the class that says pie has to
/// follow it.
///
/// This is the drift half reaching the artefact end to end: probe, diff against the
/// previous snapshot, render a finding of the right class.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_version_change_between_two_runs_is_reported_as_pie_must_change() {
    let home = tempfile::tempdir().expect("a scratch home");

    let first = StandIn::new("agy version 9.9.9", HELP);
    watch::execute(
        &config_with(&first.script),
        home.path(),
        &Options::default(),
        &FakeHttp::refusing(),
        TS,
    )
    .await
    .expect("the first run takes the first snapshot");

    let bumped = StandIn::new("agy version 9.9.10", HELP);
    let outcome = watch::execute(
        &config_with(&bumped.script),
        home.path(),
        &Options::default(),
        &FakeHttp::refusing(),
        TS,
    )
    .await
    .expect("the second run compares against it");

    assert_eq!(outcome.drifts.len(), 1, "{:?}", outcome.drifts);
    assert_eq!(
        outcome.drifts[0].class,
        DriftClass::PieMustChange,
        "a moved version is the baseline pie's parsers were validated against (RISKS §3)"
    );
    assert!(
        outcome.drifts[0].what.contains("version changed"),
        "{:?}",
        outcome.drifts[0]
    );

    let body = read_report(&outcome);
    assert!(body.contains("pie-must-change"), "{body}");
    assert!(body.contains("version changed"), "{body}");
    assert!(
        !body.contains(pie::watch::NO_DRIFT_NOTE),
        "a run with a finding must not read as a clean month:\n{body}"
    );
}

/// The first run and the second say **different** things, and the difference is the one
/// `ACCEPTANCE.md` §16 names: "first snapshot taken" versus a real comparison.
///
/// The two are indistinguishable in a report that only prints findings, and only one of
/// them is true — so the report, not just the constant, is asserted in both directions.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_first_run_says_a_snapshot_was_taken_and_the_second_says_no_drift() {
    let home = tempfile::tempdir().expect("a scratch home");
    let stand_in = StandIn::new("agy version 9.9.9", HELP);
    let cfg = config_with(&stand_in.script);

    let first = watch::execute(
        &cfg,
        home.path(),
        &Options::default(),
        &FakeHttp::refusing(),
        TS,
    )
    .await
    .expect("a real run");
    assert!(first.first_run, "there was no previous snapshot");
    let first_body = read_report(&first);
    assert!(
        first_body.contains(pie::watch::FIRST_RUN_NOTE),
        "{first_body}"
    );
    assert!(
        !first_body.contains("no drift found"),
        "a first run must never read as a comparison that happened:\n{first_body}"
    );

    let second = watch::execute(
        &cfg,
        home.path(),
        &Options::default(),
        &FakeHttp::refusing(),
        TS,
    )
    .await
    .expect("a real run");
    assert!(!second.first_run, "the first run left a baseline");
    assert!(second.drifts.is_empty(), "{:?}", second.drifts);
    let second_body = read_report(&second);
    assert!(
        second_body.contains(pie::watch::NO_DRIFT_NOTE),
        "{second_body}"
    );
    assert!(
        !second_body.contains(pie::watch::FIRST_RUN_NOTE),
        "a compared run must not claim to be a first run:\n{second_body}"
    );
}

/// A probe that answers with nothing is a **stated error naming the backend**, and no
/// snapshot is written for it.
///
/// `DESIGN.md` §17: "An empty capture is unrepresentable in `render_report` (which sees
/// only findings), so it would read as 'no drift'. The probe must treat empty output as a
/// stated error naming the backend." The control below is what proves the error is about
/// emptiness rather than about the stand-in being unusable.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_probe_that_returns_empty_help_is_a_stated_error_naming_the_backend() {
    let home = tempfile::tempdir().expect("a scratch home");
    let silent = StandIn::new("agy version 9.9.9", "");
    let cfg = config_with(&silent.script);

    let error = watch::execute(
        &cfg,
        home.path(),
        &Options::default(),
        &FakeHttp::refusing(),
        TS,
    )
    .await
    .expect_err("an empty `--help` is a diagnostic, not a snapshot");

    assert!(matches!(error, PieError::Protocol { .. }), "{error}");
    assert!(
        error.to_string().contains("agy"),
        "the error must name the backend it could not read: {error}"
    );
    assert!(
        error.to_string().contains("--help"),
        "and the probe that failed: {error}"
    );
    assert!(
        !watch::snapshot_path(home.path(), "agy").exists(),
        "nothing may be recorded from an empty capture: a snapshot with no help would read \
         as `no drift` on every later run"
    );
    assert!(
        !home.path().join("reports").exists(),
        "and no report may be written from it"
    );

    // Control: the same stand-in, answering, is a snapshot rather than an error.
    let home = tempfile::tempdir().expect("a second scratch home");
    let talking = StandIn::new("agy version 9.9.9", HELP);
    let outcome = watch::execute(
        &config_with(&talking.script),
        home.path(),
        &Options::default(),
        &FakeHttp::refusing(),
        TS,
    )
    .await
    .expect("a probe that answers is a snapshot");

    assert!(watch::snapshot_path(home.path(), "agy").is_file());
    assert_eq!(outcome.snapshots.len(), 1);
    let recorded =
        std::fs::read_to_string(watch::snapshot_path(home.path(), "agy")).expect("read it back");
    assert!(recorded.contains("--help"), "{recorded}");
}

/// A run that could probe **no** backend is a stated error, not a report.
///
/// `render_report`'s two no-findings sentences both claim a comparison happened ("no drift
/// found — every probe matched", "first snapshot taken"), and neither is true when nothing
/// was probed. The honest third option is to say so.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_run_that_can_probe_no_backend_is_a_stated_error_rather_than_a_clean_report() {
    let home = tempfile::tempdir().expect("a scratch home");
    let cfg = config_with_no_backend();

    let error = watch::execute(
        &cfg,
        home.path(),
        &Options::default(),
        &FakeHttp::refusing(),
        TS,
    )
    .await
    .expect_err("nothing was probed, so nothing may be claimed");

    assert!(matches!(error, PieError::Config(_)), "{error}");
    let text = error.to_string();
    assert!(text.contains("no backend could be probed"), "{text}");
    for backend in ["agy", "codex", "claude"] {
        assert!(
            text.contains(backend),
            "the error must name every backend it could not probe, missing `{backend}`: {text}"
        );
    }
    assert!(!home.path().join("reports").exists());
}

/// A lost source is stated **and** the run is not a success.
///
/// `ACCEPTANCE.md` §16: "a digest that lost a source says so in the report, and the run
/// exits non-zero rather than writing an empty digest as success." The control is the
/// half that makes the negative meaningful: with every source answering, neither fires.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lost_source_is_stated_in_the_report_and_is_not_a_successful_run() {
    let home = tempfile::tempdir().expect("a scratch home");
    let stand_in = StandIn::new("agy version 9.9.9", HELP);
    let cfg = config_with(&stand_in.script);
    // One source answers, one does not.
    let http = FakeHttp::answering(RSS_URL, RSS);

    let options = options_asking_for(vec![
        source("Rust Blog", RSS_URL),
        source("arXiv cs.SE", ARXIV_URL),
    ]);
    let outcome = watch::execute(&cfg, home.path(), &options, &http, TS)
        .await
        .expect("a failed source is reported, not fatal to the artefact");

    assert_eq!(http.calls().len(), 2, "both sources were attempted");
    assert_eq!(outcome.sources_ok, 1);
    assert_eq!(outcome.failed_sources, vec!["arXiv cs.SE".to_string()]);
    assert_eq!(outcome.headlines, 1);

    let body = read_report(&outcome);
    assert!(
        body.contains(pie::watch::DIGEST_INCOMPLETE_NOTE),
        "the digest must say it is incomplete:\n{body}"
    );
    assert!(body.contains("arXiv cs.SE"), "{body}");
    assert!(
        body.contains("Async closures land in Rust 1.87"),
        "a failed source must not blank the source that answered:\n{body}"
    );

    let error = watch::incomplete_digest(&outcome).expect_err("a lost source is not a clean month");
    assert!(error.to_string().contains("arXiv cs.SE"), "{error}");
    assert!(
        error.to_string().contains("watchtower"),
        "the error tells the reader the report exists and where: {error}"
    );

    // Control: the same run with only the source that answers.
    let home = tempfile::tempdir().expect("a second scratch home");
    let clean_options = options_asking_for(vec![source("Rust Blog", RSS_URL)]);
    let clean = watch::execute(&cfg, home.path(), &clean_options, &http, TS)
        .await
        .expect("a run whose sources all answered");

    assert!(clean.failed_sources.is_empty());
    assert!(
        watch::incomplete_digest(&clean).is_ok(),
        "a whole digest is a successful run"
    );
    let clean_body = read_report(&clean);
    assert!(!clean_body.contains("INCOMPLETE"), "{clean_body}");
    assert!(
        clean_body.contains("Async closures land in Rust 1.87"),
        "{clean_body}"
    );
}

/// The two partial scopes write **no** report at all.
///
/// `render_report` takes the digest from its caller and nothing stores one, so a report
/// re-rendered from a run that fetched nothing would print "no headlines this month" for a
/// month it never looked at — the silent-empty outcome `ACCEPTANCE.md` §16 rules out. Not
/// writing a file is the honest alternative, and this asserts it rather than leaving it to
/// be discovered.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_partial_scopes_write_no_report_file() {
    for scope in [Scope::Drift, Scope::News] {
        let home = tempfile::tempdir().expect("a scratch home");
        let stand_in = StandIn::new("agy version 9.9.9", HELP);
        let cfg = config_with(&stand_in.script);

        let options = Options {
            scope,
            ..Options::default()
        };
        let outcome = watch::execute(&cfg, home.path(), &options, &FakeHttp::refusing(), TS)
            .await
            .expect("a partial scope runs");

        assert!(outcome.written.is_none(), "{scope:?} wrote a report");
        assert!(
            !home.path().join("reports").exists(),
            "{scope:?} must write no report file"
        );
        assert_eq!(
            stand_in.calls(),
            if scope == Scope::Drift { 2 } else { 0 },
            "only the drift scope probes"
        );
    }
}

/// **A CLI may print its usage to stderr.** Measured on this machine: `agy --help` writes
/// its 2905 bytes of usage to **stderr** and nothing at all to stdout, because Go's flag
/// package prints usage there.
///
/// The probe captured stdout alone, so every real `pie watch` run failed with "produced no
/// output". That error is the *good* outcome: had the empty capture been written as a
/// snapshot instead of an error, it would have recorded an empty help text and reported
/// "no drift" on every later run, forever, with nothing to notice. This test is the
/// regression guard, and the stand-in mimics agy's own split rather than a convenient one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_help_capture_that_arrives_on_stderr_is_still_a_capture() {
    let home = tempfile::tempdir().expect("a scratch home");
    let dir = tempfile::tempdir().expect("a scratch dir");
    let script = dir.path().join(if cfg!(windows) {
        "stderr-help.bat"
    } else {
        "stderr-help.sh"
    });

    #[cfg(windows)]
    let body = "@echo off\r\n\
                if \"%1\"==\"--version\" (echo agy version 9.9.9) else (echo USAGE-ON-STDERR 1>&2)\r\n\
                exit /b 0\r\n";
    #[cfg(not(windows))]
    let body = "#!/bin/sh\n\
                if [ \"$1\" = \"--version\" ]; then echo 'agy version 9.9.9'; \
                else echo 'USAGE-ON-STDERR' 1>&2; fi\n\
                exit 0\n";
    std::fs::write(&script, body).expect("write the stderr-help stand-in");
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
            .expect("chmod the stand-in");
    }

    let cfg = config_with(&script);
    let options = watch::Options {
        scope: watch::Scope::Drift,
        dry_run: false,
        month: Some(TS_MONTH.to_string()),
        sources: Vec::new(),
        stack: Vec::new(),
    };

    let outcome = watch::execute(&cfg, home.path(), &options, &FakeHttp::refusing(), TS)
        .await
        .expect("help arriving on stderr is a capture, not an empty one");

    // The assertion is on what was *recorded*, not on the absence of an error: a probe
    // that swallowed the empty stdout and wrote an empty help would also have returned
    // `Ok`, and that is the failure this test exists to prevent.
    let snapshot = std::fs::read_to_string(watch::snapshot_path(home.path(), "agy"))
        .expect("the agy snapshot was written");
    assert!(
        snapshot.contains("USAGE-ON-STDERR"),
        "the stderr usage must reach the snapshot, or drift diffs nothing: {snapshot}"
    );
    assert!(
        !outcome.render().contains("produced no output"),
        "a stderr capture is not an empty capture: {}",
        outcome.render()
    );
}

/// `ACCEPTANCE.md` §13 B34, closed: the REA-lite label's third leg has a producer, so the
/// run no longer describes it as missing and the snapshot carries the strings it found.
///
/// The assertion on the *absence* and the assertion on the *content* are deliberately in
/// one test: either half alone is satisfiable by doing nothing — a run that never mentions
/// strings, and a snapshot that records an empty list, would each pass one of them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_strings_leg_runs_and_the_snapshot_carries_the_strings_it_found() {
    let home = tempfile::tempdir().expect("a scratch home");
    let stand_in = StandIn::new("agy version 9.9.9", HELP);
    let cfg = config_with(&stand_in.script);

    let outcome = watch::execute(
        &cfg,
        home.path(),
        &Options::default(),
        &FakeHttp::refusing(),
        TS,
    )
    .await
    .expect("a real run");

    // The note that announced the leg never runs is gone from the run's own output…
    let rendered = outcome.render();
    for gone in ["NOT compared", "inert", "no stage produces"] {
        assert!(
            !rendered.contains(gone),
            "B34: the run must no longer describe its own third leg as missing (`{gone}`):\n{rendered}"
        );
    }

    // …and from the report's legend, which is the part a reader skims.
    let body = read_report(&outcome);
    assert!(!body.contains("NOT compared"), "{body}");
    assert!(
        body.contains("all three diffed"),
        "the legend must claim the comparison that now happens:\n{body}"
    );

    // And the snapshot holds what the leg found, read from *this* backend's artefact. The
    // stand-in is a script whose own bytes contain `--version`, so a flag-like token from
    // it can only have come from reading that file.
    let recorded = std::fs::read_to_string(watch::snapshot_path(home.path(), "agy"))
        .expect("the agy snapshot was written");
    let file: serde_json::Value = serde_json::from_str(&recorded).expect("a readable snapshot");
    let strings: Vec<&str> = file["strings"]
        .as_array()
        .expect("the snapshot carries a strings array")
        .iter()
        .filter_map(serde_json::Value::as_str)
        .collect();

    assert!(
        !strings.is_empty(),
        "the third leg produced nothing, so the label's claim is still empty: {recorded}"
    );
    assert!(
        strings.iter().any(|s| s.contains("--version")),
        "the strings must come from this backend's own artefact: {strings:?}"
    );
    // The artefact is a scratch script, far below the read cap, so the read is whole and
    // there is no caveat to state.
    assert_eq!(
        file["strings_note"],
        serde_json::Value::Null,
        "a whole read states no caveat: {recorded}"
    );
}

/// The read reaches EOF, not the first chunk.
///
/// The previous reader took a fixed prefix, and against the real `agy.exe` (~193 MB) that
/// prefix stopped long before any flag. A fixture smaller than one read chunk cannot tell a
/// prefix read from a whole one, so this artefact is larger than a chunk and carries the
/// flag only in its tail: a driver that read one chunk and stopped would record nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_artefact_larger_than_one_read_chunk_is_read_to_eof() {
    let home = tempfile::tempdir().expect("a scratch home");

    // Larger than the driver's 1 MiB chunk, and the flag is past every boundary but the
    // last. The pad is one long printable run, so it also exercises the scanner's bounded
    // state rather than a run the reader could hold whole.
    let pad = "x".repeat(2 * 1024 * 1024);
    let trailing = format!("\r\n{pad}\r\n--print-timeout\r\n");
    let stand_in = StandIn::with_trailing("agy version 9.9.9", HELP, &trailing);
    let cfg = config_with(&stand_in.script);

    watch::execute(
        &cfg,
        home.path(),
        &Options::default(),
        &FakeHttp::refusing(),
        TS,
    )
    .await
    .expect("a real run");

    let recorded = std::fs::read_to_string(watch::snapshot_path(home.path(), "agy"))
        .expect("the agy snapshot was written");
    let file: serde_json::Value = serde_json::from_str(&recorded).expect("a readable snapshot");
    let strings: Vec<&str> = file["strings"]
        .as_array()
        .expect("the snapshot carries a strings array")
        .iter()
        .filter_map(serde_json::Value::as_str)
        .collect();

    assert!(
        strings.contains(&"--print-timeout"),
        "a flag in the artefact's tail must reach the snapshot, so the read reaches EOF: \
         {strings:?}"
    );

    // The artefact is under the streaming bound, so the read was whole and there is no
    // caveat to state; a prefix read would have said so here.
    assert_eq!(
        file["strings_note"],
        serde_json::Value::Null,
        "a whole read states no caveat: {recorded}"
    );
}

/// `ACCEPTANCE.md` §13 B34: codex's leg must read codex's **own** artefact.
///
/// codex is `node <codex.js>` on this platform, so its resolved program is node — and the
/// note that kept the third leg switched off for two milestones said exactly this: reading
/// the program would record *node's* strings as codex's.
///
/// A probe argv that spawns nothing cannot test this, so the two resolved paths are compared
/// directly. The launcher only resolves when a `node` is on `PATH`; without one,
/// `program_and_prefix` falls back to the script itself and the two agree for a reason that
/// has nothing to do with the override — hence the explicit branch rather than an assertion
/// that could be green for the wrong reason.
#[test]
fn codex_names_its_script_as_the_artefact_and_never_the_launcher() {
    let dir = tempfile::tempdir().expect("a scratch dir");
    let script = dir.path().join("codex.js");
    std::fs::write(&script, "// codex, as a fixture\n").expect("write the script fixture");

    let mut cfg = Config::default();
    cfg.codex.bin = script.display().to_string();
    cfg.codex.use_node_shim = false;

    let codex = pie::provider::by_id("codex").expect("codex is registered");

    assert_eq!(
        codex.artefact(&cfg).as_deref(),
        Some(script.as_path()),
        "the artefact is the script itself"
    );

    match codex.binary(&cfg) {
        Some(program) if program != script => {
            // The `node <codex.js>` branch — the trap, live.
            assert_ne!(
                codex.artefact(&cfg).as_deref(),
                Some(program.as_path()),
                "the artefact must never be the launcher: extracting from node's bytes \
                 would report node's strings as codex's"
            );
        }
        _ => {
            eprintln!(
                "note: no `node` resolves here, so codex's launcher branch did not run; the \
                 assertion above still pins the artefact to the script"
            );
        }
    }
}
