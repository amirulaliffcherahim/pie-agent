//! Gate G4 for M5 stage A: session persistence.
//!
//! `planning/ACCEPTANCE.md` §8, row A: *"a turn writes `<id>.jsonl` plus an index
//! row; a write failure never fails the turn"*. `planning/DESIGN.md` §11 is the
//! contract. This file is where both are enforced.
//!
//! Every turn here runs against a **stand-in** `agy` that prints a recorded
//! fixture (the same mechanism `tests/parse_agy.rs` and `tests/cache.rs` use), so
//! the suite exercises the real `pie -p` path — resolve, spawn, parse, record —
//! without spending a single token of subscription quota. The one live assertion
//! left is G5, and it is the user's call, not a test's.
//!
//! Nothing touches the real `~/.pie`: every test builds its own home in a
//! `tempfile::tempdir()` and passes it to the child through `PIE_HOME`. `HOME`
//! and `USERPROFILE` are pointed at a second temp dir so agy's credential
//! heuristic (`~/.gemini/config/config.json` present ⇒ subscription) is
//! satisfied by a fixture rather than by whatever this machine happens to have
//! logged in — a test that only passes on a logged-in machine is a test that
//! proves nothing on CI.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use pie::provider::{TurnOutcome, Usage};
use pie::session::{self, Session, TurnRow};

/// The nine keys `DESIGN.md` §11 fixes for a transcript row.
const SESSION_ROW_KEYS: [&str; 9] = [
    "conversation_id",
    "model",
    "ok",
    "prompt",
    "provider",
    "response",
    "stop_reason",
    "ts",
    "usage",
];

/// The fourteen keys of the **frozen** `usage.jsonl` row (`DESIGN.md` §7).
///
/// Spelled out here on purpose: this is the one schema in the project that an
/// outside implementation reads, so a rename must break a test in the M5 suite
/// and not merely a test inside the module that did the renaming.
const FROZEN_USAGE_KEYS: [&str; 14] = [
    "cached_input_tokens",
    "cost_usd_micros",
    "credits_consumed",
    "credits_remaining",
    "duration_ms",
    "error_kind",
    "input_tokens",
    "model",
    "ok",
    "output_tokens",
    "provider",
    "reasoning_tokens",
    "stop_reason",
    "ts",
];

/// The conversation id recorded in `tests/fixtures/agy_turn.jsonl`.
const FIXTURE_CONVERSATION_ID: &str = "8f3d9b2a-4c17-4e6a-9f0b-2d5c1a7e6b93";

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn fixture_path(name: &str) -> PathBuf {
    repo_root().join("tests").join("fixtures").join(name)
}

/// An isolated machine: pie's home, a working directory, and a fake `$HOME`.
struct Fixture {
    root: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();

        // agy's credential heuristic, satisfied by a fixture instead of by this
        // machine's real login (`src/auth.rs::agy_credential`).
        let gemini_config = root.path().join("home").join(".gemini").join("config");
        std::fs::create_dir_all(&gemini_config).unwrap();
        std::fs::write(gemini_config.join("config.json"), "{}\n").unwrap();

        for dir in ["pie", "work", "bin"] {
            std::fs::create_dir_all(root.path().join(dir)).unwrap();
        }

        Self { root }
    }

    fn pie_home(&self) -> PathBuf {
        self.root.path().join("pie")
    }

    /// `$PIE_HOME/sessions`
    fn sessions_dir(&self) -> PathBuf {
        self.pie_home().join("sessions")
    }

    fn work(&self) -> PathBuf {
        self.root.path().join("work")
    }

    /// A stand-in `agy` that ignores its argv and prints a recorded fixture.
    ///
    /// The vendor CLI is never invoked, so no test here spends quota.
    fn stand_in_agy(&self, fixture: &str) -> PathBuf {
        let payload = fixture_path(fixture);
        let bin = self.root.path().join("bin");

        #[cfg(windows)]
        let script = bin.join("stand-in-agy.bat");
        #[cfg(windows)]
        std::fs::write(
            &script,
            format!("@echo off\r\ntype \"{}\"\r\n", payload.display()),
        )
        .unwrap();

        #[cfg(not(windows))]
        let script = bin.join("stand-in-agy.sh");
        #[cfg(not(windows))]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::write(
                &script,
                format!("#!/bin/sh\nexec cat \"{}\"\n", payload.display()),
            )
            .unwrap();
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        script
    }

    /// Run `pie` with this fixture's environment and no inherited surprises.
    fn run(&self, args: &[&str]) -> Output {
        self.run_with_agy(args, "agy_turn.jsonl")
    }

    /// Run `pie` with a stand-in agy that prints `fixture`.
    fn run_with_agy(&self, args: &[&str], fixture: &str) -> Output {
        let agy = self.stand_in_agy(fixture);

        let mut command = Command::new(env!("CARGO_BIN_EXE_pie"));
        command
            .args(args)
            .env("PIE_HOME", self.pie_home())
            // agy's own state root: a fixture, not this machine's login.
            .env("HOME", self.root.path().join("home"))
            .env("USERPROFILE", self.root.path().join("home"))
            .env("PIE_AGY_BIN", agy)
            // Pin the log filter so the write-failure warning is asserted on
            // rather than inherited away.
            .env("PIE_LOG", "pie=info,warn");

        command.output().expect("failed to run the pie binary")
    }

    /// One delegated turn, as a user would run it.
    fn turn(&self, fixture: &str, prompt: &str) -> Output {
        self.run_with_agy(
            &[
                "-p",
                prompt,
                "--model",
                "agy/gemini-3.8-flash",
                "--cwd",
                &self.work().display().to_string(),
            ],
            fixture,
        )
    }

    /// One successful turn, asserted to have actually succeeded.
    fn successful_turn(&self, prompt: &str) -> Output {
        let output = self.turn("agy_turn.jsonl", prompt);
        assert!(
            output.status.success(),
            "`pie -p` failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Every file under `root`, with its bytes, for a "nothing was written" check.
fn snapshot(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    fn walk(dir: &Path, out: &mut Vec<(PathBuf, Vec<u8>)>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else if let Ok(bytes) = std::fs::read(&path) {
                out.push((path, bytes));
            }
        }
    }

    let mut out = Vec::new();
    walk(root, &mut out);
    out.sort();
    out
}

fn keys_of(value: &serde_json::Value) -> BTreeSet<String> {
    value
        .as_object()
        .unwrap_or_else(|| panic!("expected a JSON object, got {value}"))
        .keys()
        .cloned()
        .collect()
}

fn usage_rows(fixture: &Fixture) -> Vec<serde_json::Value> {
    pie::session_log::read_all(&fixture.pie_home().join("usage.jsonl"))
        .expect("the frozen log is readable")
        .iter()
        .map(|row| serde_json::to_value(row).unwrap())
        .collect()
}

/// A finished turn, for the store-level tests that must not run a turn at all.
fn outcome(text: &str, conversation_id: Option<&str>) -> TurnOutcome {
    TurnOutcome {
        text: text.to_string(),
        usage: Usage {
            input_tokens: 20_704,
            output_tokens: 20,
            thinking_tokens: 19,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            total_tokens: 20_724,
        },
        status: "SUCCESS".to_string(),
        conversation_id: conversation_id.map(str::to_string),
        duration: std::time::Duration::from_millis(35_900),
    }
}

// ------------------------------------------------------- a turn is persisted --

#[test]
fn a_turn_writes_one_transcript_row_and_one_index_row_that_agree() {
    let fixture = Fixture::new();
    let output = fixture.successful_turn("audit this repo");
    assert!(stdout(&output).contains("OK"), "{}", stdout(&output));

    // The index is the lookup path, so it must point at a real transcript.
    let sessions = session::list_from(&fixture.sessions_dir()).unwrap();
    assert_eq!(sessions.len(), 1, "one turn, one session");
    let summary = &sessions[0];

    let transcripts: Vec<String> = std::fs::read_dir(fixture.sessions_dir())
        .unwrap()
        .flatten()
        .filter_map(|e| e.file_name().to_str().map(str::to_string))
        .filter(|name| name.ends_with(".jsonl") && name != "index.jsonl")
        .collect();
    assert_eq!(
        transcripts,
        vec![format!("{}.jsonl", summary.id)],
        "the index id names the transcript file"
    );

    let rows = session::load_from(&fixture.sessions_dir(), &summary.id).unwrap();
    assert_eq!(rows.len(), 1, "one turn, one row");
    let row = &rows[0];

    // The two agree on everything they both carry.
    assert_eq!(
        summary.turns,
        rows.len() as u64,
        "the index must not disagree with the transcript it summarises"
    );
    assert_eq!(summary.provider, row.provider);

    // …and the row is the turn that actually ran.
    assert_eq!(row.provider, "agy");
    assert_eq!(row.model, "gemini-3.8-flash");
    assert_eq!(row.prompt, "audit this repo");
    assert_eq!(row.response, "OK\n");
    assert_eq!(
        row.conversation_id.as_deref(),
        Some(FIXTURE_CONVERSATION_ID)
    );
    assert!(row.ok);
    assert_eq!(row.stop_reason, "SUCCESS");
    assert_eq!(row.usage.input_tokens, 20_704);
    assert_eq!(row.usage.thinking_tokens, 19);
    assert!(row.ts > 0);

    assert_eq!(summary.cwd, fixture.work().display().to_string());
    assert_eq!(summary.preview, "audit this repo");
    assert!(summary.started_at > 0);
    assert!(summary.updated_at >= summary.started_at);
}

#[test]
fn a_turn_row_carries_exactly_the_nine_contract_keys() {
    let fixture = Fixture::new();
    fixture.successful_turn("audit this repo");

    let sessions = session::list_from(&fixture.sessions_dir()).unwrap();
    let text = std::fs::read_to_string(session::transcript_path(
        &fixture.sessions_dir(),
        &sessions[0].id,
    ))
    .unwrap();
    assert_eq!(text.lines().count(), 1, "one JSON object per turn");
    assert!(
        text.starts_with("{\"ts\":"),
        "the on-disk row keeps DESIGN's field order: {text}"
    );

    let value: serde_json::Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
    assert_eq!(
        keys_of(&value),
        SESSION_ROW_KEYS
            .iter()
            .map(|k| (*k).to_string())
            .collect::<BTreeSet<_>>(),
        "DESIGN §11 fixes this row"
    );

    // The index frame, likewise.
    let index = std::fs::read_to_string(session::index_path(&fixture.sessions_dir())).unwrap();
    let frame: serde_json::Value = serde_json::from_str(index.lines().next().unwrap()).unwrap();
    assert_eq!(
        keys_of(&frame),
        [
            "cwd",
            "id",
            "preview",
            "provider",
            "started_at",
            "turns",
            "updated_at"
        ]
        .iter()
        .map(|k| (*k).to_string())
        .collect::<BTreeSet<_>>(),
        "DESIGN §11 fixes the index frame too"
    );
}

/// A failure is part of a session: it is the only record of what was asked when
/// nothing came back, which is exactly what a later resume needs.
#[test]
fn a_failed_turn_is_recorded_with_its_reason() {
    let fixture = Fixture::new();
    let output = fixture.turn("agy_error_result.jsonl", "what is the quota");

    assert!(
        !output.status.success(),
        "the fixture is an ERROR status; the turn must fail"
    );

    let sessions = session::list_from(&fixture.sessions_dir()).unwrap();
    assert_eq!(sessions.len(), 1, "a failed turn still records a session");

    let rows = session::load_from(&fixture.sessions_dir(), &sessions[0].id).unwrap();
    assert_eq!(rows.len(), 1);
    assert!(!rows[0].ok, "the row says so");
    assert_eq!(
        rows[0].stop_reason, "backend",
        "with a reason: the error class, since the nine-key schema has no error_kind"
    );
    assert_eq!(rows[0].prompt, "what is the quota", "the ask survives");
    assert_eq!(rows[0].response, "");
    assert!(rows[0].conversation_id.is_none());
    assert_eq!(rows[0].usage, Usage::default());
    assert_eq!(sessions[0].provider, "agy");
    assert_eq!(sessions[0].preview, "what is the quota");
}

#[test]
fn a_session_write_failure_does_not_fail_a_successful_turn() {
    let fixture = Fixture::new();

    // A *file* where the store directory belongs. `create_dir_all` fails on this
    // on Windows and Unix alike, which is why it is used instead of a read-only
    // directory: an owner can write inside his own read-only directory, so the
    // permission trick would silently not break anything here.
    std::fs::write(fixture.sessions_dir(), "not a directory").unwrap();

    let output = fixture.successful_turn("audit this repo");

    assert!(
        output.status.success(),
        "a failed session write must never fail a turn that produced an answer: {}",
        stderr(&output)
    );
    assert!(stdout(&output).contains("OK"), "{}", stdout(&output));
    assert!(
        stderr(&output).contains("could not record the turn in the session store"),
        "the failure is reported, not swallowed silently: {}",
        stderr(&output)
    );

    // The obstruction is untouched, and the frozen log still got its row — the
    // turn's accounting is a separate file on purpose (DESIGN §11).
    assert!(fixture.sessions_dir().is_file());
    assert_eq!(usage_rows(&fixture).len(), 1);
}

// --------------------------------------------------------------- the command --

#[test]
fn pie_sessions_lists_what_was_written_and_writes_nothing_of_its_own() {
    let fixture = Fixture::new();
    fixture.successful_turn("audit this repo");

    let before = snapshot(&fixture.pie_home());
    let output = fixture.run(&["sessions"]);
    assert!(output.status.success(), "{}", stderr(&output));

    let printed = stdout(&output);
    let sessions = session::list_from(&fixture.sessions_dir()).unwrap();
    assert!(
        printed.contains(&sessions[0].id),
        "the id must be listed:\n{printed}"
    );
    assert!(printed.contains("agy"), "the provider:\n{printed}");
    assert!(
        printed.contains("audit this repo"),
        "the preview:\n{printed}"
    );
    assert!(printed.contains("SESSION"), "a header:\n{printed}");
    assert!(printed.contains("1 session(s)"), "{printed}");

    assert_eq!(
        snapshot(&fixture.pie_home()),
        before,
        "`pie sessions` must write no row of its own"
    );
}

#[test]
fn each_turn_is_its_own_session_and_the_newest_is_listed_first() {
    let fixture = Fixture::new();
    fixture.successful_turn("first task");
    fixture.successful_turn("second task");

    let sessions = session::list_from(&fixture.sessions_dir()).unwrap();
    assert_eq!(
        sessions.len(),
        2,
        "stage A has no --continue, so a plain `pie -p` starts a fresh session"
    );

    let output = fixture.run(&["sessions"]);
    let printed = stdout(&output);
    let first = printed.find(&sessions[0].id).expect("newest listed");
    let second = printed.find(&sessions[1].id).expect("oldest listed");
    assert!(
        first < second,
        "newest first:\n{printed}\n{:?}",
        sessions.iter().map(|s| &s.id).collect::<Vec<_>>()
    );
    assert!(printed.contains("2 session(s)"), "{printed}");
}

#[test]
fn sessions_on_an_empty_store_explains_itself_instead_of_failing() {
    let fixture = Fixture::new();
    let output = fixture.run(&["sessions"]);

    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("no sessions recorded yet"),
        "{}",
        stdout(&output)
    );
}

/// `pie sessions` reads pie's own index and nothing else. A spawn would be a
/// quota risk, and a transcript write would make the listing command part of the
/// history it reports.
///
/// Checked statically as well as by the run above, because the run only proves it
/// for the paths this test happens to exercise (`tests/profile.rs` makes the same
/// argument for `pie profile`). The forbidden tokens are the *write* paths, not
/// the module names: this command legitimately borrows `session_log::now_millis`
/// for its age column, and an alias would evade a name check anyway — which is
/// what the byte-for-byte snapshot above is for.
#[test]
fn the_sessions_command_can_neither_spawn_nor_record() {
    let text = std::fs::read_to_string(repo_root().join("src/commands/sessions.rs")).unwrap();

    let mut offenders = Vec::new();
    for (index, line) in text.lines().enumerate() {
        if line.trim_start().starts_with("//") {
            continue;
        }
        for forbidden in [
            "crate::process",
            "process::",
            ".stream(",
            "session_log::append",
            "Command::new",
            "Session::",
            "record(",
            "record_best_effort",
        ] {
            if line.contains(forbidden) {
                offenders.push(format!("{}:{} {forbidden}", index + 1, line.trim()));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "`pie sessions` must neither spawn nor write a session row, but it names: {offenders:?}"
    );
}

// ----------------------------------------------------------- the frozen log --

#[test]
fn the_frozen_usage_row_schema_is_unchanged() {
    let fixture = Fixture::new();
    fixture.successful_turn("audit this repo");

    let rows = usage_rows(&fixture);
    assert_eq!(rows.len(), 1, "one row per turn, as before M5");

    assert_eq!(
        keys_of(&rows[0]),
        FROZEN_USAGE_KEYS
            .iter()
            .map(|k| (*k).to_string())
            .collect::<BTreeSet<_>>(),
        "usage.jsonl is read by the earlier JS implementation: exactly these 14 keys, no more"
    );
    assert_eq!(rows[0]["input_tokens"], 20_704);
    assert_eq!(rows[0]["ok"], true);
    assert!(rows[0]["error_kind"].is_null());
}

/// Writing a session must not touch the frozen log: they are separate files, and
/// the JS implementation on this machine reads the second one.
#[test]
fn writing_a_session_does_not_modify_usage_jsonl() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("pie");
    std::fs::create_dir_all(&home).unwrap();

    let log = home.join("usage.jsonl");
    let frozen = r#"{"ts":1,"provider":"agy","model":"m","input_tokens":1,"cached_input_tokens":0,"output_tokens":1,"reasoning_tokens":0,"cost_usd_micros":0,"credits_consumed":0,"credits_remaining":0,"duration_ms":1,"ok":true,"error_kind":null,"stop_reason":"SUCCESS"}"#;
    std::fs::write(&log, format!("{frozen}\n")).unwrap();
    let before = std::fs::read(&log).unwrap();

    let mut session = Session::start_in(home.join("sessions"), "agy", Path::new("/work"));
    session
        .record(&TurnRow::from_turn(
            "agy",
            "gemini-3.8-flash",
            "audit this repo",
            &outcome("OK\n", Some("conv-1")),
        ))
        .unwrap();

    assert!(session.transcript_path().is_file());
    assert_eq!(
        std::fs::read(&log).unwrap(),
        before,
        "the frozen log must be byte-identical after a session write"
    );

    // The realistic shape of the same check: a recorded turn and then a listing
    // must leave the frozen log alone too.
    let fixture = Fixture::new();
    fixture.successful_turn("audit this repo");
    let before = std::fs::read(fixture.pie_home().join("usage.jsonl")).unwrap();
    assert!(fixture.run(&["sessions"]).status.success());
    assert_eq!(
        std::fs::read(fixture.pie_home().join("usage.jsonl")).unwrap(),
        before
    );
}

// ------------------------------------------------------ nothing else records --

/// ACCEPTANCE §8: no session row is written by `auth`, `usage`, `models`,
/// `doctor` or `profile`.
///
/// The binaries are pointed at paths that do not exist so the commands fail fast
/// and offline; whether a command succeeds is not the question here. The question
/// is whether it recorded a session.
#[test]
fn no_other_command_writes_a_session_row() {
    for command in [
        vec!["auth"],
        vec!["usage"],
        vec!["models"],
        vec!["doctor"],
        vec!["profile"],
    ] {
        let fixture = Fixture::new();
        let missing = fixture
            .root
            .path()
            .join("not-installed")
            .display()
            .to_string();

        let output = Command::new(env!("CARGO_BIN_EXE_pie"))
            .args(&command)
            .env("PIE_HOME", fixture.pie_home())
            .env("HOME", fixture.root.path().join("home"))
            .env("USERPROFILE", fixture.root.path().join("home"))
            .env("PIE_AGY_BIN", &missing)
            .env("PIE_CODEX_BIN", &missing)
            .env("PIE_CLAUDE_BIN", &missing)
            .env("PIE_LOG", "pie=info,warn")
            .output()
            .expect("failed to run the pie binary");

        assert!(
            session::list_from(&fixture.sessions_dir())
                .unwrap()
                .is_empty(),
            "`pie {}` wrote a session row (exit {:?}): {}",
            command.join(" "),
            output.status.code(),
            stderr(&output)
        );
        assert!(
            !fixture.sessions_dir().exists(),
            "`pie {}` created the session store; nothing but a turn may",
            command.join(" ")
        );
    }
}
