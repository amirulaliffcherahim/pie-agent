//! M7 stage D gate: the command surface that makes stages A–C usable.
//!
//! Contract: `planning/DESIGN.md` §16; criteria: `planning/ACCEPTANCE.md` §15 (row
//! "D commands"). The gate is `cargo test --test memory_cmd`.
//!
//! Every test here runs **offline**: no network, no vendor CLI, and no `PIE_LIVE`. The
//! one model call this file exercises is served by a local stand-in — the
//! counting-stand-in pattern `tests/cache.rs` established — which is what makes the
//! central claim countable rather than described:
//!
//! * the default config makes **zero** provider spawns, asserted by *count*;
//! * a job the budget cannot cover is refused **before** the count could move;
//! * a `--dry-run` leaves the memory directory **byte-identical**;
//! * a `revert` restores the pre-apply bytes exactly, not something that looks right.
//!
//! `HOME`/`USERPROFILE` are redirected wherever a path could reach a vendor's own state
//! (gate G3d, after B32): a test that passes only because the developer's machine has a
//! vendor login is green for a reason nobody can reproduce.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};

use pie::commands::learn::Options;
use pie::commands::{learn, memory as memory_cmd, schedule};
use pie::config::{Config, JobCadence};
use pie::error::PieError;
use pie::session::{Session, TurnRow};
use pie::tui::{self, Action, App, Effect, Entry, InFlight, Input, LocalCommand, Status};

/// A fixed clock reading for hand-built turns. Decades in the past on purpose: a
/// timestamp near *now* would make a clock read inside the pipeline indistinguishable
/// from the turns' own `ts`.
const TS: u64 = 1_700_000_000_000;

/// The prompt every run below learns something from.
const PREFERENCE: &str = "always run cargo fmt before committing.";

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn fixture(name: &str) -> PathBuf {
    repo_root().join("tests").join("fixtures").join(name)
}

// ------------------------------------------------------------- fixtures ------

/// One successful turn, timestamped from its index so provenance is pinnable.
fn row(index: usize, prompt: &str) -> TurnRow {
    TurnRow {
        ts: TS + index as u64,
        provider: "agy".to_string(),
        model: "gemini-3.8-flash".to_string(),
        prompt: prompt.to_string(),
        ok: true,
        ..Default::default()
    }
}

/// Record a session under `home/sessions`, the way `pie -p` records one.
fn write_session(home: &Path, prompts: &[&str]) {
    let mut session = Session::start_in(home.join("sessions"), "agy", Path::new("/work"));
    for (index, prompt) in prompts.iter().enumerate() {
        session
            .record(&row(index, prompt))
            .expect("record a fixture turn");
    }
}

/// Every file under `root` with its bytes, so "wrote nothing" is a comparison rather
/// than an inspection.
fn snapshot(root: &Path) -> BTreeMap<String, Vec<u8>> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(root, &path, out);
            } else if let Ok(bytes) = std::fs::read(&path) {
                let rel = path
                    .strip_prefix(root)
                    .unwrap_or(&path)
                    .display()
                    .to_string();
                out.insert(rel, bytes);
            }
        }
    }

    let mut files = BTreeMap::new();
    walk(root, root, &mut files);
    files
}

/// A stand-in backend that appends a line to `<dir>/calls.txt` every time it runs, then
/// prints `fixture`.
///
/// The counter is the whole point: it distinguishes "no provider was reached" from "a
/// provider was reached and returned something harmless", which no assertion on the
/// output alone can do.
fn counting_stand_in(dir: &Path, fixture_name: &str) -> PathBuf {
    let payload = fixture(fixture_name);
    let counter = dir.join("calls.txt");

    #[cfg(windows)]
    let script = dir.join("counting-agy.bat");
    #[cfg(not(windows))]
    let script = dir.join("counting-agy.sh");

    #[cfg(windows)]
    let body = format!(
        "@echo off\r\necho call>> \"{}\"\r\ntype \"{}\"\r\n",
        counter.display(),
        payload.display()
    );
    #[cfg(not(windows))]
    let body = format!(
        "#!/bin/sh\necho call >> '{}'\ncat '{}'\n",
        counter.display(),
        payload.display()
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

/// How many times the stand-in ran. A missing counter is zero runs, not an error.
fn calls(dir: &Path) -> usize {
    std::fs::read_to_string(dir.join("calls.txt"))
        .map(|text| text.lines().count())
        .unwrap_or(0)
}

/// A `Config` whose only backend points at a local stand-in.
fn config_with(sentinel: &Path) -> Config {
    let mut cfg = Config::default();
    cfg.agy.bin = sentinel.display().to_string();
    cfg
}

/// Points `PIE_HOME` and the vendor home at scratch directories for a block.
///
/// `src/testing.rs`'s `EnvGuard` is the house guard and is `pub(crate)`, so an
/// integration test cannot use it; this is the shape `tests/learn.rs`'s `EnvPin` uses,
/// with its own lock so the two binaries never contend. Held for the shortest possible
/// scope: a guard taken twice on one thread would deadlock the binary (B24).
#[derive(Debug)]
struct EnvPin {
    saved: Vec<(&'static str, Option<String>)>,
    _guard: MutexGuard<'static, ()>,
}

impl EnvPin {
    fn new(pie_home: &Path, vendor_home: &Path) -> Self {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        let guard = LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        let mut saved = Vec::new();
        for (key, value) in [
            ("PIE_HOME", pie_home),
            ("HOME", vendor_home),
            ("USERPROFILE", vendor_home),
        ] {
            saved.push((key, std::env::var(key).ok()));
            std::env::set_var(key, value);
        }
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

/// A scratch machine: pie's home, a vendor home agy's own credential heuristic accepts,
/// and a directory for stand-ins.
fn machine() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let home = scratch.path().join("pie");
    let vendor = scratch.path().join("vendor-home");

    // agy's credential heuristic, satisfied by a fixture rather than by this machine's
    // real login (`src/auth.rs::agy_credential`). Without it the one test that does spawn
    // would fail on a CI runner and pass here — the whole defect G3d exists to catch.
    std::fs::create_dir_all(vendor.join(".gemini").join("config")).expect("scratch ~/.gemini");
    std::fs::write(
        vendor.join(".gemini").join("config").join("config.json"),
        "{}\n",
    )
    .expect("scratch agy config");

    (scratch, home, vendor)
}

/// The undo id `pie memory show` prints, as a user would copy it.
fn undo_id_from(shown: &str) -> String {
    shown
        .lines()
        .find_map(|line| line.strip_prefix("undo: "))
        .and_then(|rest| rest.split_whitespace().next())
        .map(str::to_string)
        .unwrap_or_else(|| panic!("no undo id was printed:\n{shown}"))
}

fn doc_bytes(home: &Path) -> Vec<u8> {
    std::fs::read(pie::memory::doc_path(home)).expect("read the learned document")
}

// --------------------------------------------------- the default is free -----

/// `ACCEPTANCE.md` §15, first Checks bullet: with the **default** config a run performs
/// **zero** provider spawns, and the *count* is asserted.
///
/// The sentinel would have recorded every call if there had been one, which the very
/// next test proves by making one happen.
///
/// A multi-threaded runtime, deliberately: `learn::review` bridges the synchronous spawn
/// seam to an async backend with `block_in_place`, which panics on a current-thread
/// runtime. Under that flavour a regression which *did* spawn would be reported as a
/// runtime panic instead of as this test's count — a red gate with the wrong reason.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_default_config_makes_no_model_call_even_with_a_sentinel_in_place() {
    let (_scratch, home, vendor) = machine();
    write_session(&home, &[PREFERENCE, PREFERENCE]);

    let stand_in = tempfile::tempdir().expect("a stand-in directory");
    let sentinel = counting_stand_in(stand_in.path(), "agy_turn.jsonl");
    let cfg = config_with(&sentinel);

    let _env = EnvPin::new(&home, &vendor);
    learn::run(&cfg, &Options::default())
        .await
        .expect("the default run must succeed");

    assert_eq!(
        calls(stand_in.path()),
        0,
        "`pie learn` with the default config must reach no backend at all"
    );
    // The run has to have *done* something, or the assertion above would hold for a
    // command that did nothing.
    assert!(
        pie::memory::doc_path(&home).is_file(),
        "the run must still have written what it learned"
    );
}

/// The control for the test above: the same sentinel, a budget that covers the job, and
/// the count moves.
///
/// Without this, "zero spawns" would be indistinguishable from "this stand-in does not
/// count" — the shape of evidence `HANDOFF.md` §9 asks for.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_budget_that_covers_the_job_reaches_the_backend_so_the_zero_above_is_evidence() {
    let (_scratch, home, vendor) = machine();
    write_session(&home, &[PREFERENCE, PREFERENCE]);

    let stand_in = tempfile::tempdir().expect("a stand-in directory");
    let sentinel = counting_stand_in(stand_in.path(), "agy_turn.jsonl");
    let mut cfg = config_with(&sentinel);
    cfg.jobs.learn_budget_tokens = 200_000;

    let _env = EnvPin::new(&home, &vendor);
    let options = Options {
        model: Some("agy/gemini-3.8-flash".to_string()),
        ..Options::default()
    };
    learn::run(&cfg, &options)
        .await
        .expect("the run must succeed");

    assert_eq!(
        calls(stand_in.path()),
        1,
        "a covered budget must reach the backend exactly once"
    );

    // And the spend is recorded where `pie schedule status` reads it, so a later run
    // inside the same window spends the remainder rather than the whole budget again.
    let ledger: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(home.join("schedule.json")).unwrap())
            .unwrap();
    assert!(
        ledger["spent"].as_u64().unwrap_or(0) > 0,
        "the ledger must record what the run spent: {ledger}"
    );
    assert!(ledger["last_run_ms"].as_u64().is_some(), "{ledger}");
}

/// `--dry-run` writes nothing at all: not a document, not a journal, not even the
/// `memory/` directory — asserted by comparing every file under the home before and
/// after.
///
/// A budget that *would* cover the job is configured deliberately, so "nothing was
/// spent" is a fact about `--dry-run` rather than about the budget.
#[tokio::test]
async fn a_dry_run_leaves_the_memory_directory_byte_identical() {
    let (_scratch, home, vendor) = machine();
    write_session(&home, &[PREFERENCE, PREFERENCE]);

    let stand_in = tempfile::tempdir().expect("a stand-in directory");
    let sentinel = counting_stand_in(stand_in.path(), "agy_turn.jsonl");
    let mut cfg = config_with(&sentinel);
    cfg.jobs.learn_budget_tokens = 200_000;

    let _env = EnvPin::new(&home, &vendor);
    let before = snapshot(&home);

    let options = Options {
        dry_run: true,
        report: true,
        model: Some("agy/gemini-3.8-flash".to_string()),
    };
    learn::run(&cfg, &options)
        .await
        .expect("the dry run must succeed");

    assert_eq!(
        before,
        snapshot(&home),
        "a dry run must not create or modify a single file"
    );
    assert!(
        !home.join("memory").exists(),
        "a dry run must not even create the memory directory"
    );
    assert_eq!(
        calls(stand_in.path()),
        0,
        "a dry run must spend nothing, even when the budget would cover the job"
    );
}

// ------------------------------------------------------ real runs and undo ----

/// A real run writes the document; a second run over the same store **reinforces**
/// rather than duplicates, and leaves the file byte-identical.
#[test]
fn a_second_run_reinforces_rather_than_duplicating() {
    let (_scratch, home, _vendor) = machine();
    write_session(&home, &[PREFERENCE, PREFERENCE]);

    let first = learn::plan(&home, TS).expect("plan the first run");
    let journal = learn::apply(&first, &home).expect("write the first run");
    assert!(
        journal.previous.is_empty(),
        "the first run had no document to preserve"
    );

    let after_first = doc_bytes(&home);
    let text = String::from_utf8(after_first.clone()).unwrap();
    assert!(
        text.contains("always run cargo fmt before committing"),
        "{text}"
    );
    assert_eq!(
        text.matches("always run cargo fmt before committing")
            .count(),
        1,
        "{text}"
    );

    let second = learn::plan(&home, TS).expect("plan the second run");
    assert!(
        second.consolidation.added.is_empty(),
        "nothing new was observed, so nothing may be added: {:#?}",
        second.consolidation
    );
    assert!(
        !second.consolidation.reinforced.is_empty(),
        "the fact was re-observed, so it must be reinforced: {:#?}",
        second.consolidation
    );
    let journal = learn::apply(&second, &home).expect("re-apply");
    assert!(
        journal.added.is_empty(),
        "a run that changed nothing appends nothing"
    );

    assert_eq!(
        doc_bytes(&home),
        after_first,
        "a second run over an unchanged store must not rewrite the document"
    );
    assert_eq!(
        String::from_utf8(doc_bytes(&home))
            .unwrap()
            .matches("always run cargo fmt before committing")
            .count(),
        1,
        "reinforcement merges by max, so the fact appears once — a sum would double it"
    );
}

/// `show` prints provenance, and `revert` restores the pre-apply bytes **exactly**.
#[test]
fn show_prints_provenance_and_revert_restores_the_exact_previous_bytes() {
    let (_scratch, home, _vendor) = machine();
    write_session(&home, &[PREFERENCE, PREFERENCE]);
    let first = learn::plan(&home, TS).unwrap();
    learn::apply(&first, &home).unwrap();
    let before = doc_bytes(&home);

    // A second session, so the second apply has something to add and the journal has
    // something to undo.
    write_session(&home, &["never touch the vendor store."]);
    let second = learn::plan(&home, TS).unwrap();
    learn::apply(&second, &home).unwrap();
    assert_ne!(doc_bytes(&home), before, "the second run must have written");

    let shown = memory_cmd::render(&home).expect("render the memory report");
    assert!(
        shown.contains("pie-session"),
        "provenance must name the source store:\n{shown}"
    );
    assert!(
        shown.contains("turn 1"),
        "provenance must name the turn the fact was observed in:\n{shown}"
    );
    assert!(shown.contains("never touch the vendor store"), "{shown}");

    let id = undo_id_from(&shown);
    memory_cmd::revert_at(&home, &id).expect("revert the second apply");

    assert_eq!(
        doc_bytes(&home),
        before,
        "a revert restores the previous bytes, not a reconstruction of them"
    );

    // The journal is consumed: an undo that outlived its undo would invite a second
    // revert of a state that is already current.
    let error = memory_cmd::revert_at(&home, &id).expect_err("the undo handle is spent");
    assert!(matches!(error, PieError::Config(_)), "{error}");
    assert!(error.to_string().contains("nothing to revert"), "{error}");
}

/// The report is deterministic, which is what makes it worth keeping: two renders of one
/// plan are byte-identical, so a report can be diffed against yesterday's
/// (`ACCEPTANCE.md` §15, "a report can be diffed").
#[test]
fn two_renders_of_the_same_plan_are_byte_identical() {
    let (_scratch, home, _vendor) = machine();
    write_session(&home, &[PREFERENCE, PREFERENCE, "audit the renderer."]);

    let plan = learn::plan(&home, TS).unwrap();
    let journal = learn::apply(&plan, &home).unwrap();
    let options = Options {
        report: true,
        ..Options::default()
    };

    let render = || {
        learn::Report {
            plan: &plan,
            journal: Some(&journal),
            review: None,
            options: &options,
            home: &home,
        }
        .render()
    };

    let first = render();
    assert_eq!(first, render(), "the report must be diffable");
    assert!(
        first.contains("added") && first.contains("candidates (ranked:"),
        "the fixture must produce a report with content, or this proves nothing:\n{first}"
    );
}

/// `ACCEPTANCE.md` §15: no M7 path writes a vendor store, and no M7 path reads one
/// unless the config opts in. A full run leaves `~/.gemini`, `~/.codex` and `~/.claude`
/// byte-identical, and the shipped default is what makes that a claim about the default
/// rather than about a setting.
#[tokio::test]
async fn a_full_run_leaves_every_vendor_store_byte_identical() {
    let (scratch, home, vendor) = machine();
    write_session(&home, &[PREFERENCE, PREFERENCE]);

    // Vendor stores with real, plausible content: a path that read them would have
    // something to read, and one that wrote them would have something to change.
    for (path, body) in [
        (".gemini/antigravity-cli/history.jsonl", "{\"agy\":1}\n"),
        (".codex/history.jsonl", "{\"codex\":1}\n"),
        (".codex/sessions/rollout.jsonl", "{\"rollout\":1}\n"),
        (".claude/sessions/a.jsonl", "{\"claude\":1}\n"),
    ] {
        let path = vendor.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    let cfg = Config::default();
    assert!(
        !cfg.jobs.read_vendor_history,
        "the shipped default must not read a vendor's own history"
    );

    let _env = EnvPin::new(&home, &vendor);
    let before = snapshot(&vendor);
    learn::run(&cfg, &Options::default()).await.expect("learn");
    memory_cmd::show().expect("memory show");

    assert!(
        !before.is_empty(),
        "the fixture must have written vendor stores, or this proves nothing"
    );
    assert_eq!(
        before,
        snapshot(&vendor),
        "a full run must leave every vendor store exactly as it found it"
    );
    assert!(
        home.join("memory").join("learned.md").is_file(),
        "and it must still have written what it learned: {}",
        scratch.path().display()
    );
}

/// A stale or invented id is refused, and refusing changes nothing.
#[test]
fn a_wrong_undo_id_is_refused_and_leaves_the_document_alone() {
    let (_scratch, home, _vendor) = machine();
    write_session(&home, &[PREFERENCE, PREFERENCE]);
    let first = learn::plan(&home, TS).unwrap();
    learn::apply(&first, &home).unwrap();

    let stale = undo_id_from(&memory_cmd::render(&home).unwrap());

    write_session(&home, &["never touch the vendor store."]);
    let second = learn::plan(&home, TS).unwrap();
    learn::apply(&second, &home).unwrap();
    let current = doc_bytes(&home);

    let error = memory_cmd::revert_at(&home, &stale).expect_err("a stale id must be refused");
    assert!(matches!(error, PieError::Config(_)), "{error}");
    assert!(error.to_string().contains("not the undo id"), "{error}");
    assert_eq!(
        doc_bytes(&home),
        current,
        "a refused revert must not have touched the document"
    );
}

/// `--model` without a configured budget is a **config error**, not a silent spend
/// (`ACCEPTANCE.md` §15).
#[tokio::test]
async fn learn_with_a_model_and_no_configured_budget_is_a_config_error() {
    let (_scratch, home, vendor) = machine();
    write_session(&home, &[PREFERENCE, PREFERENCE]);

    let stand_in = tempfile::tempdir().expect("a stand-in directory");
    let sentinel = counting_stand_in(stand_in.path(), "agy_turn.jsonl");
    // The shipped default: `learn_budget_tokens = 0`.
    let cfg = config_with(&sentinel);

    let _env = EnvPin::new(&home, &vendor);
    let before = snapshot(&home);

    let options = Options {
        model: Some("agy/gemini-3.8-flash".to_string()),
        ..Options::default()
    };
    let error = learn::run(&cfg, &options)
        .await
        .expect_err("an unbudgeted model call must be refused");

    assert!(matches!(error, PieError::Config(_)), "{error}");
    assert!(error.to_string().contains("budget"), "{error}");
    assert!(
        error.to_string().contains("Nothing was spawned"),
        "the refusal must say nothing ran: {error}"
    );
    assert_eq!(
        calls(stand_in.path()),
        0,
        "the refusal happens before the spawn, so the sentinel must not move"
    );
    assert_eq!(
        before,
        snapshot(&home),
        "a refused run must not write anything at all"
    );
}

/// `--report` names what was skipped and why, and lists the candidates the extractor
/// actually produced — which the summary does not.
#[test]
fn the_report_names_what_was_skipped_and_why() {
    let (_scratch, home, _vendor) = machine();
    // A sentence past the entry budget: `extract` truncates to 240 chars plus an
    // ellipsis, and `consolidate` refuses to write a 241-character "fact".
    let long = format!("always {}", "x".repeat(300));
    write_session(&home, &[long.as_str()]);

    let plan = learn::plan(&home, TS).expect("plan the run");
    assert!(
        plan.consolidation.added.is_empty(),
        "{:#?}",
        plan.consolidation
    );
    assert_eq!(
        plan.consolidation.skipped.len(),
        1,
        "one oversized candidate, one skip: {:#?}",
        plan.consolidation.skipped
    );

    let summary = learn::Report {
        plan: &plan,
        journal: None,
        review: None,
        options: &Options::default(),
        home: &home,
    }
    .render();
    assert!(
        summary.contains("past the 240-char entry budget"),
        "the reason must be printed, not the fact alone:\n{summary}"
    );

    let verbose = learn::Report {
        plan: &plan,
        journal: None,
        review: None,
        options: &Options {
            report: true,
            ..Options::default()
        },
        home: &home,
    }
    .render();
    assert!(verbose.contains("candidates (ranked:"), "{verbose}");
    assert!(
        verbose.contains("past the 240-char entry budget"),
        "the detailed report still names the reason:\n{verbose}"
    );
    assert!(
        verbose.contains("hits 1"),
        "the detailed report lists the recurrence the extractor counted:\n{verbose}"
    );
}

/// I7 applied to M7: none of the three commands writes a session-log row.
#[tokio::test]
async fn none_of_the_three_commands_writes_a_session_log_row() {
    let (_scratch, home, vendor) = machine();
    write_session(&home, &[PREFERENCE, PREFERENCE]);

    let _env = EnvPin::new(&home, &vendor);
    learn::run(&Config::default(), &Options::default())
        .await
        .expect("learn");
    memory_cmd::show().expect("memory show");
    schedule::status(&Config::default()).expect("schedule status");

    assert!(
        !home.join("usage.jsonl").exists(),
        "a command that spends no model quota records no turn: `usage.jsonl` must not exist"
    );
    assert!(
        !home.join("schedule.json").exists(),
        "`status` is read-only: it must not create the ledger it reads"
    );
}

// -------------------------------------------------------------- schedule -----

/// `schedule status` reports what is due and the whole budget state, and writes nothing.
#[test]
fn schedule_status_reports_the_budget_state_without_spending() {
    let (scratch, home, _vendor) = machine();
    std::fs::create_dir_all(&home).unwrap();
    // The ledger as a real run leaves it. Written by hand here so this test pins the
    // on-disk shape as well as the report.
    std::fs::write(
        home.join(schedule::LEDGER_FILE),
        "{\n  \"last_run_ms\": 1700000000000,\n  \"spent\": 100\n}\n",
    )
    .unwrap();

    let mut cfg = Config::default();
    cfg.jobs.learn_cadence = JobCadence::Daily;
    cfg.jobs.learn_budget_tokens = 4096;

    let before = snapshot(&home);
    let status = schedule::status_text(&cfg, &home, TS + 25 * 3_600_000).expect("render status");

    assert!(status.contains("4096 max"), "{status}");
    assert!(status.contains("100 spent"), "{status}");
    assert!(status.contains("3996 remaining"), "{status}");
    assert!(status.contains("due       yes"), "{status}");
    assert!(status.contains("1d ago"), "{status}");
    assert!(
        status.contains("spawned nothing"),
        "the report must say it spent nothing:\n{status}"
    );
    assert_eq!(before, snapshot(&home), "`status` must not write");

    // The shipped defaults, which is what makes M7 opt-in: nothing is ever due, and a
    // budget of zero refuses every budgeted job.
    let defaults = schedule::status_text(&Config::default(), &home, TS).unwrap();
    assert!(defaults.contains("cadence   never"), "{defaults}");
    assert!(defaults.contains("0 max"), "{defaults}");
    assert!(
        defaults.contains("refuses every budgeted job"),
        "{defaults}"
    );
    assert!(defaults.contains("never"), "{defaults}");

    // And the same thing through the command itself, on a scratch `PIE_HOME`.
    let _env = EnvPin::new(&home, &scratch.path().join("vendor-home"));
    let before = snapshot(&home);
    schedule::status(&Config::default()).expect("schedule status");
    assert_eq!(before, snapshot(&home), "`status` must not write");
}

/// With the shipped `learn_budget_tokens = 0`, an explicit run refuses **before**
/// anything is spawned and says so.
#[tokio::test]
async fn a_scheduled_run_with_the_default_budget_refuses_before_anything_spawns() {
    let (_scratch, home, vendor) = machine();
    write_session(&home, &[PREFERENCE, PREFERENCE]);

    let stand_in = tempfile::tempdir().expect("a stand-in directory");
    let sentinel = counting_stand_in(stand_in.path(), "agy_turn.jsonl");
    let cfg = config_with(&sentinel);

    let _env = EnvPin::new(&home, &vendor);
    let error = schedule::run(&cfg, Some("learn"))
        .await
        .expect_err("a zero budget must refuse the job");

    assert!(matches!(error, PieError::Config(_)), "{error}");
    assert!(error.to_string().contains("budget"), "{error}");
    assert!(error.to_string().contains("Nothing was spawned"), "{error}");
    assert_eq!(
        calls(stand_in.path()),
        0,
        "the refusal happens before the spawn"
    );
    assert!(
        !home.join(schedule::LEDGER_FILE).exists(),
        "a refused run records nothing"
    );

    let error = schedule::run(&cfg, Some("nope"))
        .await
        .expect_err("an unknown job must be a usage error");
    assert!(error.to_string().contains("unknown job"), "{error}");
}

/// With a budget that covers it, the same job runs — the positive half of the refusal
/// above, without which "it refused" could not be told from "it never runs".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_scheduled_run_with_a_covered_budget_runs_the_job_once() {
    let (_scratch, home, vendor) = machine();
    write_session(&home, &[PREFERENCE, PREFERENCE]);

    let stand_in = tempfile::tempdir().expect("a stand-in directory");
    let sentinel = counting_stand_in(stand_in.path(), "agy_turn.jsonl");
    let mut cfg = config_with(&sentinel);
    cfg.jobs.learn_budget_tokens = 200_000;
    // `pie schedule run learn` takes no `--model`, so the backend comes from the
    // configuration — `DESIGN.md` §16's "config naming a backend and a token budget".
    cfg.general.default_model = Some("agy/gemini-3.8-flash".to_string());

    let _env = EnvPin::new(&home, &vendor);
    schedule::run(&cfg, Some("learn"))
        .await
        .expect("a covered budget must run the job");

    assert_eq!(
        calls(stand_in.path()),
        1,
        "the job must reach the backend once"
    );
    let ledger: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(home.join(schedule::LEDGER_FILE)).unwrap())
            .unwrap();
    assert!(ledger["spent"].as_u64().unwrap_or(0) > 0, "{ledger}");
    assert!(
        ledger["last_run_ms"].as_u64().is_some(),
        "the window's ledger must record when it ran, which is what `status` reports: {ledger}"
    );
}

/// A ledger pie cannot read refuses the run rather than handing the window's budget
/// back: a spend it cannot see is a spend it would repeat (`RISKS.md` §13).
#[tokio::test]
async fn a_corrupt_ledger_refuses_rather_than_handing_the_budget_back() {
    let (_scratch, home, vendor) = machine();
    write_session(&home, &[PREFERENCE, PREFERENCE]);
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join(schedule::LEDGER_FILE), "not json at all\n").unwrap();

    let stand_in = tempfile::tempdir().expect("a stand-in directory");
    let sentinel = counting_stand_in(stand_in.path(), "agy_turn.jsonl");
    let mut cfg = config_with(&sentinel);
    // A budget that would otherwise allow the job, so the refusal is about the ledger.
    cfg.jobs.learn_budget_tokens = 200_000;

    let _env = EnvPin::new(&home, &vendor);
    let error = schedule::run(&cfg, Some("learn"))
        .await
        .expect_err("a corrupt ledger must refuse the run");

    assert!(matches!(error, PieError::Config(_)), "{error}");
    assert!(
        error.to_string().contains("ledger pie cannot read"),
        "{error}"
    );
    assert_eq!(
        calls(stand_in.path()),
        0,
        "nothing may be spawned against a ledger pie cannot read"
    );
}

// ------------------------------------------------------------------- TUI -----

/// `DESIGN.md` §16: `/memory` routes through the ordinary `Submit` path, and M6's
/// `Input` is not extended.
#[test]
fn the_tui_routes_slash_commands_through_submit_and_never_through_a_new_key() {
    assert_eq!(
        tui::effect_for(Action::Submit, "/memory".to_string()),
        Effect::Local(LocalCommand::Memory)
    );
    assert_eq!(
        tui::effect_for(Action::Submit, "  /memory".to_string()),
        Effect::Local(LocalCommand::Memory),
        "leading whitespace is still a command"
    );
    assert_eq!(
        tui::effect_for(Action::Submit, "summarise the log".to_string()),
        Effect::Run("summarise the log".to_string()),
        "everything that is not a slash command is a turn, exactly as before"
    );
    assert_eq!(
        tui::effect_for(Action::Quit, "/memory".to_string()),
        Effect::Quit,
        "a slash command must not change what any other action means"
    );

    assert_eq!(
        tui::local_command("/hlep"),
        Some(LocalCommand::Unknown("hlep".to_string())),
        "an unknown command is recognised rather than sent to a backend"
    );
    assert_eq!(tui::local_command("no slash here"), None);
    assert_eq!(
        tui::local_command("/"),
        Some(LocalCommand::Unknown(String::new()))
    );
}

/// A submitted slash command is not a turn: no clock starts, nothing is recorded, and
/// the transcript says what the user typed without any backend seeing it.
#[test]
fn submitting_a_slash_command_starts_no_turn() {
    let scratch = tempfile::tempdir().unwrap();
    let mut session = Session::start_in(scratch.path(), "codex", Path::new("."));
    let mut app = App::new("codex", None, "s-1-2");
    app.prompt = "/memory".to_string();
    let mut in_flight = InFlight::default();

    let effect = tui::act(
        &mut app,
        &mut session,
        &mut in_flight,
        tui::Step::Key(Input::Enter),
    );

    assert_eq!(effect, Effect::Local(LocalCommand::Memory));
    assert!(
        in_flight.started.is_none() && in_flight.prompt.is_empty(),
        "a local command is not a turn, so no turn clock may start"
    );
    assert_eq!(
        session.turns(),
        0,
        "and nothing may be recorded in the store"
    );
    assert_eq!(
        app.transcript,
        vec![Entry::User("/memory".to_string())],
        "the reducer still shows the user what they typed"
    );
    // `reduce` accepted a submission, so the status is `Running` and only
    // `apply_local_command` can take it off — which is why the next test exists.
    assert!(app.status.is_running());
}

/// The loop's half of that: the report lands in the transcript and the status leaves
/// `Running`, because no turn will ever end it.
#[test]
fn applying_a_local_command_clears_the_running_status_and_shows_every_line() {
    let mut app = App::new("codex", None, "s-1-2");
    app.status = Status::Running;
    app.transcript.push(Entry::User("/memory".to_string()));

    tui::apply_local_command(
        &mut app,
        vec![
            "learned memory: /tmp/pie".to_string(),
            "0 fact(s)".to_string(),
        ],
    );

    assert_eq!(
        app.status,
        Status::Idle,
        "no turn is in flight, so the spinner must stop"
    );
    assert_eq!(app.transcript.len(), 3);
    assert_eq!(
        app.transcript[1],
        Entry::Notice("learned memory: /tmp/pie".to_string())
    );
    assert_eq!(app.transcript[2], Entry::Notice("0 fact(s)".to_string()));
}

/// `/memory` in the TUI prints the same bytes `pie memory show` prints.
#[test]
fn the_tui_memory_command_shows_what_pie_learned_with_provenance() {
    let (scratch, home, _vendor) = machine();
    write_session(&home, &[PREFERENCE, PREFERENCE]);
    let plan = learn::plan(&home, TS).unwrap();
    learn::apply(&plan, &home).unwrap();

    let _env = EnvPin::new(&home, &scratch.path().join("vendor-home"));
    let lines = memory_cmd::local_lines(&LocalCommand::Memory);
    let joined = lines.join("\n");

    assert!(joined.contains("always run cargo fmt"), "{joined}");
    assert!(
        joined.contains("pie-session"),
        "the TUI must show provenance too:\n{joined}"
    );
    assert!(joined.contains("undo: "), "{joined}");

    let unknown = memory_cmd::local_lines(&LocalCommand::Unknown("hlep".to_string()));
    let joined = unknown.join(" ");
    assert!(joined.contains("is not a pie command"), "{joined}");
    assert!(
        joined.contains("nothing was sent to a backend"),
        "a typo must not cost quota:\n{joined}"
    );
}

/// M6's `Input` is frozen. The exhaustive match below has **no wildcard arm**, so a
/// variant added to `Input` stops this test compiling — which is the only way a
/// frozen enum can be enforced by a gate rather than by a comment.
#[test]
fn m6s_input_enum_is_still_frozen_at_its_ten_variants() {
    let inputs = [
        Input::Char('x'),
        Input::Enter,
        Input::Esc,
        Input::Backspace,
        Input::Up,
        Input::Down,
        Input::Left,
        Input::Right,
        Input::Tab,
        Input::Ctrl('c'),
    ];

    let labels: Vec<String> = inputs
        .into_iter()
        .map(|input| match input {
            Input::Char(c) => c.to_string(),
            Input::Enter => "enter".to_string(),
            Input::Esc => "escape".to_string(),
            Input::Backspace => "backspace".to_string(),
            Input::Up => "up".to_string(),
            Input::Down => "down".to_string(),
            Input::Left => "left".to_string(),
            Input::Right => "right".to_string(),
            Input::Tab => "tab".to_string(),
            Input::Ctrl(c) => c.to_string(),
        })
        .collect();

    assert_eq!(labels.len(), 10, "the frozen keymap has ten inputs");
}
