//! Gate G4 for M5 sessions: persistence (stage A), continuation (stage B) and the
//! backend switch (stage F).
//!
//! `planning/ACCEPTANCE.md` §8: row A is *"a turn writes `<id>.jsonl` plus an
//! index row; a write failure never fails the turn"*, row B is *"`--continue`
//! picks the newest session for provider+cwd; `--session <id>` targets one; a
//! vendor id that no longer exists re-bootstraps instead of erroring; **the
//! session id is surfaced after a turn**"*, and row F is *"switching backends
//! rebuilds the prompt from pie's transcript and records the new vendor id —
//! payload-level, no quota"*. `planning/DESIGN.md` §11 is the contract. This file
//! is where all three are enforced — one binary for all three stages, because they
//! are three halves of the same store.
//!
//! Every turn here runs against a **stand-in** `agy` that prints a recorded
//! fixture (the same mechanism `tests/parse_agy.rs` and `tests/cache.rs` use), so
//! the suite exercises the real `pie -p` path — resolve, spawn, parse, record —
//! without spending a single token of subscription quota. The one live assertion
//! left is G5, and it is the user's call, not a test's.
//!
//! The stage B selection and prompt-assembly paths are also called **directly**
//! (`print::select`, `print::apply_selection`, `print::seed_prompt`), because the
//! payload is what the contract is about and a `ChatRequest` is easier to assert
//! on than an argv that a stand-in cannot reliably read (see [`Seeded`]).
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

use clap::Parser;
use pie::cli::Cli;
use pie::commands::print;
use pie::error::PieError;
use pie::provider::{ChatRequest, TurnOutcome, Usage};
use pie::session::{self, Session, SessionSummary, TurnRow};

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

    /// The argv a user would type for one turn in this fixture's directory.
    fn args(&self, prompt: &str, extra: &[&str]) -> Vec<String> {
        let mut args = vec![
            "-p".to_string(),
            prompt.to_string(),
            "--model".to_string(),
            "agy/gemini-3.8-flash".to_string(),
            "--cwd".to_string(),
            self.work().display().to_string(),
        ];
        args.extend(extra.iter().map(|arg| (*arg).to_string()));
        args
    }

    /// Write the stand-in `agy` for `kind` beside a copy of the answer fixture.
    ///
    /// Each stand-in counts its invocations in `calls.log`, which is how
    /// "retries exactly once" is asserted: a counter (
    /// `echo invoked`) survives an argv the batch language cannot parse, where a
    /// logged command line would not (cmd.exe stops at the first newline inside
    /// an argument, and a re-bootstrap prompt is many lines long).
    fn stub(&self, kind: Stub, fixture: &str) -> PathBuf {
        let bin = self.root.path().join("bin");
        std::fs::copy(fixture_path(fixture), bin.join("answer.jsonl")).unwrap();

        #[cfg(windows)]
        let script = bin.join(format!("{}.bat", kind.name()));
        #[cfg(windows)]
        std::fs::write(
            &script,
            match kind {
                // Answers every turn, and records the argv it was handed. Only
                // used with single-line prompts: cmd.exe stops at the first
                // newline inside an argument, so a re-bootstrap prompt — which is
                // many lines long — could not be read back this way.
                Stub::Args => concat!(
                    "@echo off\r\n",
                    "echo invoked >> \"%~dp0calls.log\"\r\n",
                    "echo %* >> \"%~dp0argv.log\"\r\n",
                    "type \"%~dp0answer.jsonl\"\r\n",
                ),
                // The first invocation fails the way a purged conversation does;
                // the second answers. That is exactly the shape of RISKS §6.
                Stub::Stale => concat!(
                    "@echo off\r\n",
                    "if exist \"%~dp0calls.log\" goto fresh\r\n",
                    "echo invoked >> \"%~dp0calls.log\"\r\n",
                    ">&2 echo Error: conversation not found\r\n",
                    "exit /b 1\r\n",
                    ":fresh\r\n",
                    "echo invoked >> \"%~dp0calls.log\"\r\n",
                    "type \"%~dp0answer.jsonl\"\r\n",
                ),
                // Every invocation fails, and not because anything is missing.
                Stub::Unrelated => concat!(
                    "@echo off\r\n",
                    "echo invoked >> \"%~dp0calls.log\"\r\n",
                    ">&2 echo Error: model overloaded, try again later\r\n",
                    "exit /b 1\r\n",
                ),
            },
        )
        .unwrap();

        #[cfg(not(windows))]
        let script = bin.join(format!("{}.sh", kind.name()));
        #[cfg(not(windows))]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::write(
                &script,
                match kind {
                    Stub::Args => concat!(
                        "#!/bin/sh\n",
                        "d=\"$(dirname \"$0\")\"\n",
                        "echo invoked >> \"$d/calls.log\"\n",
                        "printf '%s\\n' \"$*\" >> \"$d/argv.log\"\n",
                        "cat \"$d/answer.jsonl\"\n",
                    ),
                    Stub::Stale => concat!(
                        "#!/bin/sh\n",
                        "d=\"$(dirname \"$0\")\"\n",
                        "if [ -f \"$d/calls.log\" ]; then\n",
                        "  echo invoked >> \"$d/calls.log\"\n",
                        "  cat \"$d/answer.jsonl\"\n",
                        "  exit 0\n",
                        "fi\n",
                        "echo invoked >> \"$d/calls.log\"\n",
                        "echo 'Error: conversation not found' >&2\n",
                        "exit 1\n",
                    ),
                    Stub::Unrelated => concat!(
                        "#!/bin/sh\n",
                        "d=\"$(dirname \"$0\")\"\n",
                        "echo invoked >> \"$d/calls.log\"\n",
                        "echo 'Error: model overloaded, try again later' >&2\n",
                        "exit 1\n",
                    ),
                },
            )
            .unwrap();
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        script
    }

    /// Run `pie` against a stand-in, with this fixture's environment.
    fn run_stub(&self, kind: Stub, args: &[String]) -> Output {
        let stub = self.stub(kind, "agy_turn.jsonl");

        Command::new(env!("CARGO_BIN_EXE_pie"))
            .args(args)
            .env("PIE_HOME", self.pie_home())
            .env("HOME", self.root.path().join("home"))
            .env("USERPROFILE", self.root.path().join("home"))
            .env("PIE_AGY_BIN", stub)
            // Recorded on purpose: the stand-ins are outside pie's home, so a
            // "nothing was written" check stays honest.
            .env("PIE_LOG", "pie=info,warn")
            .output()
            .expect("failed to run the pie binary")
    }

    /// How many times the stand-in was invoked.
    fn stub_calls(&self) -> usize {
        std::fs::read_to_string(self.root.path().join("bin").join("calls.log"))
            .map(|text| text.lines().count())
            .unwrap_or(0)
    }

    /// The argv the stand-in was handed, joined. Empty unless [`Stub::Args`].
    fn stub_argv(&self) -> String {
        std::fs::read_to_string(self.root.path().join("bin").join("argv.log")).unwrap_or_default()
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

// ------------------------------------------------- continuing a session ----

/// Which stand-in `agy` a test wants. None of them is a real CLI.
#[derive(Clone, Copy)]
enum Stub {
    /// Answer every turn, and record the argv it was handed.
    Args,
    /// The first invocation fails the way a purged conversation does.
    Stale,
    /// Every invocation fails for a reason that is not a missing conversation.
    Unrelated,
}

impl Stub {
    fn name(&self) -> &'static str {
        match self {
            Stub::Args => "stub-args",
            Stub::Stale => "stub-stale",
            Stub::Unrelated => "stub-unrelated",
        }
    }
}

/// A session to put in the store, described rather than written.
///
/// Stage A's writer has its own tests. What stage B needs is a store whose
/// `updated_at` values are **fixed numbers**: "newest by `updated_at`" is the
/// property under test, and two real turns landing in the same millisecond would
/// turn the assertion into a race.
struct Seeded<'a> {
    id: &'a str,
    provider: &'a str,
    cwd: &'a Path,
    updated_at: u64,
    prompt: &'a str,
    response: &'a str,
    vendor: Option<&'a str>,
}

/// Write one session's transcript, and return the index frame describing it.
fn seed(fixture: &Fixture, spec: &Seeded) -> SessionSummary {
    std::fs::create_dir_all(fixture.sessions_dir()).unwrap();

    let row = TurnRow {
        ts: spec.updated_at,
        provider: spec.provider.to_string(),
        model: "gemini-3.8-flash".to_string(),
        prompt: spec.prompt.to_string(),
        response: spec.response.to_string(),
        conversation_id: spec.vendor.map(str::to_string),
        usage: Usage::default(),
        ok: true,
        stop_reason: "SUCCESS".to_string(),
    };
    std::fs::write(
        session::transcript_path(&fixture.sessions_dir(), spec.id),
        format!("{}\n", serde_json::to_string(&row).unwrap()),
    )
    .unwrap();

    SessionSummary {
        id: spec.id.to_string(),
        provider: spec.provider.to_string(),
        cwd: spec.cwd.display().to_string(),
        started_at: spec.updated_at,
        updated_at: spec.updated_at,
        turns: 1,
        preview: spec.prompt.to_string(),
    }
}

/// Write the whole index, in the order given (the format is append-only frames).
fn write_index(fixture: &Fixture, frames: &[SessionSummary]) {
    let mut text = String::new();
    for frame in frames {
        text.push_str(&serde_json::to_string(frame).unwrap());
        text.push('\n');
    }
    std::fs::write(session::index_path(&fixture.sessions_dir()), text).unwrap();
}

/// Two agy sessions in this fixture's directory, the second one newer.
fn two_sessions(fixture: &Fixture) -> (SessionSummary, SessionSummary) {
    let older = seed(
        fixture,
        &Seeded {
            id: "s-1000-1",
            provider: "agy",
            cwd: &fixture.work(),
            updated_at: 1_000,
            prompt: "older task",
            response: "OLDER ANSWER",
            vendor: Some("conv-old"),
        },
    );
    let newer = seed(
        fixture,
        &Seeded {
            id: "s-2000-2",
            provider: "agy",
            cwd: &fixture.work(),
            updated_at: 2_000,
            prompt: "newer task",
            response: "NEWER ANSWER",
            vendor: Some("conv-new"),
        },
    );
    write_index(fixture, &[older.clone(), newer.clone()]);
    (older, newer)
}

/// A parsed command line, as `pie -p` would see it.
fn cli(args: &[&str]) -> Cli {
    let mut argv = vec!["pie"];
    argv.extend_from_slice(args);
    Cli::try_parse_from(argv).expect("these arguments parse")
}

// ----------------------------------------- which conversation, payload level --

/// ACCEPTANCE §8, stage B: `--continue` picks the newest session for
/// provider+cwd, and the id that session recorded is what the turn resumes.
#[test]
fn continue_takes_the_newest_session_for_this_provider_and_directory() {
    let fixture = Fixture::new();
    let (_, newer) = two_sessions(&fixture);

    let selection = print::select(
        &fixture.sessions_dir(),
        &cli(&["-p", "next task", "--continue"]),
        "agy",
        &fixture.work(),
    )
    .unwrap();

    let continuation = selection.continuation().expect("a session was selected");
    assert_eq!(
        continuation.summary.id, newer.id,
        "the newest matching session is the one continued"
    );
    assert_eq!(
        continuation.rows.len(),
        1,
        "with its transcript, not a summary"
    );
    assert_eq!(selection.vendor_id(), Some("conv-new"));

    // …and that vendor id is what the request carries: the vendor's id, not
    // pie's session id.
    let mut request = ChatRequest::new("next task");
    print::apply_selection(&selection, &mut request, "next task");
    assert_eq!(request.resume.as_deref(), Some("conv-new"));
    assert_eq!(
        request.prompt, "next task",
        "a resumed turn sends the task as given"
    );
}

/// "Newest" means by `updated_at`, and *both* the provider and the working
/// directory have to match — either alone would resume a conversation the user
/// did not ask for.
#[test]
fn the_newest_is_by_updated_at_and_a_foreign_directory_or_provider_is_not_selected() {
    let fixture = Fixture::new();
    let elsewhere = fixture.root.path().join("other-work");
    std::fs::create_dir_all(&elsewhere).unwrap();

    let frames = vec![
        seed(
            &fixture,
            &Seeded {
                id: "s-4000-4",
                provider: "agy",
                cwd: &elsewhere,
                updated_at: 4_000,
                prompt: "elsewhere",
                response: "A",
                vendor: Some("conv-elsewhere"),
            },
        ),
        seed(
            &fixture,
            &Seeded {
                id: "s-3000-3",
                provider: "codex",
                cwd: &fixture.work(),
                updated_at: 3_000,
                prompt: "other backend",
                response: "B",
                vendor: Some("conv-codex"),
            },
        ),
        seed(
            &fixture,
            &Seeded {
                id: "s-2000-2",
                provider: "agy",
                cwd: &fixture.work(),
                updated_at: 2_000,
                prompt: "matching",
                response: "C",
                vendor: Some("conv-agy"),
            },
        ),
    ];
    write_index(&fixture, &frames);

    let selection = print::select(
        &fixture.sessions_dir(),
        &cli(&["-p", "next", "--continue"]),
        "agy",
        &fixture.work(),
    )
    .unwrap();

    assert_eq!(
        selection.vendor_id(),
        Some("conv-agy"),
        "two sessions are newer; one is another backend's and one another directory's"
    );
    assert_eq!(selection.continuation().unwrap().summary.id, "s-2000-2");
}

#[test]
fn session_targets_exactly_that_session() {
    let fixture = Fixture::new();
    let (older, _) = two_sessions(&fixture);

    let selection = print::select(
        &fixture.sessions_dir(),
        &cli(&["-p", "next", "--session", &older.id]),
        "agy",
        &fixture.work(),
    )
    .unwrap();
    assert_eq!(
        selection.vendor_id(),
        Some("conv-old"),
        "named explicitly, so the newer session is not preferred"
    );

    // Naming a session is an explicit act, so the working directory is not part
    // of the match — the user asked for this conversation, wherever it ran.
    let elsewhere = fixture.root.path().join("other-work");
    std::fs::create_dir_all(&elsewhere).unwrap();
    let selection = print::select(
        &fixture.sessions_dir(),
        &cli(&["-p", "next", "--session", &older.id]),
        "agy",
        &elsewhere,
    )
    .unwrap();
    assert_eq!(selection.vendor_id(), Some("conv-old"));

    // The provider, though, is not negotiable: the id belongs to the backend
    // that issued it, and moving a session between backends is stage F's job.
    let error = print::select(
        &fixture.sessions_dir(),
        &cli(&["-p", "next", "--session", &older.id]),
        "codex",
        &fixture.work(),
    )
    .unwrap_err();
    assert!(matches!(error, PieError::Config(_)), "{error:?}");
    let message = error.to_string();
    assert!(
        message.contains("agy") && message.contains("codex"),
        "{message}"
    );
}

#[test]
fn an_unknown_session_id_is_a_config_error_that_names_it() {
    let fixture = Fixture::new();
    two_sessions(&fixture);

    let error = print::select(
        &fixture.sessions_dir(),
        &cli(&["-p", "next", "--session", "s-9999-9"]),
        "agy",
        &fixture.work(),
    )
    .unwrap_err();
    assert!(matches!(error, PieError::Config(_)), "{error:?}");
    assert_eq!(error.exit_code(), 2, "a configuration error, not a crash");
    let message = error.to_string();
    assert!(message.contains("s-9999-9"), "{message}");
    assert!(message.contains("pie sessions"), "{message}");

    // An id that could never name a transcript is refused for that reason.
    let error = print::select(
        &fixture.sessions_dir(),
        &cli(&["-p", "next", "--session", "../usage"]),
        "agy",
        &fixture.work(),
    )
    .unwrap_err();
    assert!(matches!(error, PieError::Config(_)), "{error:?}");
    assert!(error.to_string().contains("session id"), "{error}");
}

/// Three flags for one decision, so any two of them together is refused rather
/// than silently resolved in one direction.
#[test]
fn each_pair_of_selectors_is_a_config_error_naming_both() {
    let fixture = Fixture::new();
    two_sessions(&fixture);

    let pairs: [(&str, &str, &[&str]); 3] = [
        (
            "--continue",
            "--session",
            &["-p", "next", "--continue", "--session", "s-1000-1"],
        ),
        (
            "--continue",
            "--resume",
            &["-p", "next", "--continue", "--resume", "conv-old"],
        ),
        (
            "--session",
            "--resume",
            &[
                "-p",
                "next",
                "--session",
                "s-1000-1",
                "--resume",
                "conv-old",
            ],
        ),
    ];

    for (first, second, args) in pairs {
        let error =
            print::select(&fixture.sessions_dir(), &cli(args), "agy", &fixture.work()).unwrap_err();
        assert!(matches!(error, PieError::Config(_)), "{error:?}");
        assert_eq!(error.exit_code(), 2);
        let message = error.to_string();
        assert!(
            message.contains(first) && message.contains(second),
            "the message must name the conflict: {message}"
        );
    }
}

/// Silently starting fresh when the user asked to continue is the failure mode
/// this milestone exists to avoid, so "nothing to continue" is an error that
/// says *which* provider and directory had nothing.
#[test]
fn continue_with_nothing_to_continue_names_the_provider_and_directory() {
    let fixture = Fixture::new();
    let args = cli(&["-p", "next", "--continue"]);

    // An empty store…
    let error = print::select(&fixture.sessions_dir(), &args, "agy", &fixture.work()).unwrap_err();
    assert!(matches!(error, PieError::Config(_)), "{error:?}");
    assert_eq!(error.exit_code(), 2);
    let message = error.to_string();
    assert!(message.contains("agy"), "{message}");
    assert!(
        message.contains(&fixture.work().display().to_string()),
        "{message}"
    );

    // …and a store with sessions, none of them for this backend in this place.
    let elsewhere = fixture.root.path().join("other-work");
    std::fs::create_dir_all(&elsewhere).unwrap();
    let frame = seed(
        &fixture,
        &Seeded {
            id: "s-5000-5",
            provider: "codex",
            cwd: &elsewhere,
            updated_at: 5_000,
            prompt: "someone else's session",
            response: "X",
            vendor: Some("conv-x"),
        },
    );
    write_index(&fixture, &[frame]);

    let error = print::select(&fixture.sessions_dir(), &args, "agy", &fixture.work()).unwrap_err();
    assert!(matches!(error, PieError::Config(_)), "{error:?}");
}

/// A session whose every turn failed has no conversation to hand back, so it is
/// re-bootstrapped from pie's own transcript rather than resuming nothing.
#[test]
fn a_session_with_no_vendor_id_is_rebootstrapped_from_its_own_transcript() {
    let fixture = Fixture::new();
    let frame = seed(
        &fixture,
        &Seeded {
            id: "s-1000-1",
            provider: "agy",
            cwd: &fixture.work(),
            updated_at: 1_000,
            prompt: "first task",
            response: "",
            vendor: None,
        },
    );
    write_index(&fixture, &[frame]);

    let selection = print::select(
        &fixture.sessions_dir(),
        &cli(&["-p", "next task", "--continue"]),
        "agy",
        &fixture.work(),
    )
    .unwrap();
    assert_eq!(
        selection.vendor_id(),
        None,
        "there is no conversation to hand back"
    );

    let mut request = ChatRequest::new("next task");
    print::apply_selection(&selection, &mut request, "next task");
    assert!(request.resume.is_none());
    assert!(request.prompt.contains("first task"), "{}", request.prompt);
    assert!(request.prompt.ends_with("next task"), "{}", request.prompt);
}

/// The `RISKS.md` §6 classifier. It reads vendor text, so its safety comes from
/// requiring two independent signals: a marker that something is missing, and a
/// noun that names a conversation. A false positive would re-run a turn that
/// failed for a real reason — spending subscription quota twice and replacing the
/// real error with the retry's — so the negative cases matter as much as these.
#[test]
fn a_stale_conversation_is_recognised_and_an_unrelated_failure_is_not() {
    let stale = [
        PieError::protocol(
            "agy",
            "stdout closed without a terminal `result` event (exit code: 1); stderr: Error: \
             conversation not found",
        ),
        PieError::Backend {
            backend: "codex".into(),
            status: "exit code: 1".into(),
            detail: "no rollout found for this thread".into(),
        },
        PieError::Backend {
            backend: "agy".into(),
            status: "ERROR".into(),
            detail: "conversation 8f3d9b2a no longer exists".into(),
        },
    ];
    for error in stale {
        assert!(print::is_stale_conversation(&error), "{error}");
    }

    let unrelated = [
        PieError::Backend {
            backend: "agy".into(),
            status: "ERROR".into(),
            detail: "model overloaded, try again later".into(),
        },
        PieError::Timeout {
            backend: "agy".into(),
            secs: 300,
        },
        PieError::AuthRequired {
            backend: "codex".into(),
            hint: "run `codex login`".into(),
        },
        PieError::protocol("agy", "stderr: no space left on device"),
        PieError::BinaryMissing {
            backend: "agy".into(),
            path: "C:/nowhere/agy.exe".into(),
        },
    ];
    for error in unrelated {
        assert!(!print::is_stale_conversation(&error), "{error}");
    }
}

/// The prompt a re-bootstrap sends: the history pie still has, then the task.
///
/// Stage B pinned this shape when the replay was raw, and it is the seam stage C
/// filled: the bytes now come from `compact::assemble`, so what is asserted here
/// is the property stage B was actually protecting — the whole history, in order,
/// with the volatile task text last — rather than the old framing. The failed
/// turn's `stop_reason` is not in this payload: `compact` renders only a row's
/// prompt and response outside the state block, which is its own documented rule.
#[test]
fn seeding_renders_the_earlier_turns_then_the_task() {
    let rows = vec![
        TurnRow {
            ts: 1_000,
            provider: "agy".into(),
            model: "gemini-3.8-flash".into(),
            prompt: "first task".into(),
            response: "FIRST ANSWER".into(),
            conversation_id: Some("conv-old".into()),
            usage: Usage::default(),
            ok: true,
            stop_reason: "SUCCESS".into(),
        },
        TurnRow {
            ts: 2_000,
            provider: "agy".into(),
            model: String::new(),
            prompt: "second task".into(),
            response: String::new(),
            conversation_id: None,
            usage: Usage::default(),
            ok: false,
            stop_reason: "timeout".into(),
        },
    ];

    let seeded = print::seed_prompt(&rows, "third task");

    // Both halves of the earlier turn, which is what makes the rebuild useful.
    assert!(seeded.contains("first task"), "{seeded}");
    assert!(seeded.contains("FIRST ANSWER"), "{seeded}");
    // A failed turn is history too: what was asked, with no answer after it.
    assert!(seeded.contains("second task"), "{seeded}");
    assert!(
        !seeded.contains("timeout"),
        "the reason is rendered in the state block, not in a replayed turn: {seeded}"
    );

    // The task is last, and the history precedes it in order.
    assert!(seeded.ends_with("third task"), "{seeded}");
    let task = seeded.rfind("third task").expect("the task text");
    let history = &seeded[..task];
    let first = history.find("first task").expect("the first turn");
    let second = history.find("second task").expect("the second turn");
    assert!(first < second, "chronological: {seeded}");

    // Nothing is elided below the compaction threshold, and the note says what the
    // payload is: not the backend's own context, but a reconstruction.
    assert!(seeded.starts_with(print::REBUILD_NOTE), "{seeded}");

    // Seeding is not recursive: a recorded row holds the user's text, never a
    // payload, so a re-bootstrapped turn cannot nest history inside history.
    assert_eq!(seeded.matches("first task").count(), 1, "{seeded}");
}

// ------------------------------------------- which conversation, end to end --

#[test]
fn continue_reuses_the_selected_session_and_hands_its_vendor_id_back() {
    let fixture = Fixture::new();
    let (_, newer) = two_sessions(&fixture);

    let output = fixture.run_stub(Stub::Args, &fixture.args("next task", &["--continue"]));
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).contains("OK"), "{}", stdout(&output));

    // The vendor id comes from the newest session's transcript, and the older
    // session's id is nowhere in the argv.
    let argv = fixture.stub_argv();
    assert!(argv.contains("--conversation conv-new"), "{argv}");
    assert!(!argv.contains("conv-old"), "{argv}");

    // …and the turn joined that session instead of starting a third one.
    let sessions = session::list_from(&fixture.sessions_dir()).unwrap();
    assert_eq!(sessions.len(), 2, "no new session was minted");
    let continued = sessions
        .iter()
        .find(|summary| summary.id == newer.id)
        .expect("the continued session is still listed");
    assert_eq!(continued.turns, 2, "the index folded the turn onto it");
    assert_eq!(
        session::load_from(&fixture.sessions_dir(), &newer.id)
            .unwrap()
            .len(),
        2,
        "the transcript holds both turns"
    );

    // The ids are surfaced, which is what makes `--session <id>` reachable
    // without running `pie sessions` first.
    let err = stderr(&output);
    assert!(err.contains(&format!("[session] {}", newer.id)), "{err}");
    assert!(
        err.contains(&format!("[resume] {FIXTURE_CONVERSATION_ID}")),
        "{err}"
    );
}

#[test]
fn session_targets_one_session_and_the_turn_lands_in_it() {
    let fixture = Fixture::new();
    let (older, newer) = two_sessions(&fixture);

    let output = fixture.run_stub(
        Stub::Args,
        &fixture.args("next task", &["--session", &older.id]),
    );
    assert!(output.status.success(), "{}", stderr(&output));

    let argv = fixture.stub_argv();
    assert!(argv.contains("--conversation conv-old"), "{argv}");
    assert!(!argv.contains("conv-new"), "{argv}");

    assert!(
        stderr(&output).contains(&format!("[session] {}", older.id)),
        "{}",
        stderr(&output)
    );
    assert_eq!(
        session::load_from(&fixture.sessions_dir(), &older.id)
            .unwrap()
            .len(),
        2,
        "the named session got the turn"
    );
    assert_eq!(
        session::load_from(&fixture.sessions_dir(), &newer.id)
            .unwrap()
            .len(),
        1,
        "and the newer one is untouched"
    );
}

/// ACCEPTANCE §8: "`--continue` is opt-in: a plain `pie -p` starts a fresh
/// conversation". Resume is context continuity, not a latency win, and reusing a
/// conversation the user did not ask for can answer in the wrong context.
#[test]
fn a_plain_run_starts_a_fresh_conversation() {
    let fixture = Fixture::new();
    let (older, newer) = two_sessions(&fixture);

    let output = fixture.run_stub(Stub::Args, &fixture.args("next task", &[]));
    assert!(output.status.success(), "{}", stderr(&output));

    let argv = fixture.stub_argv();
    assert!(
        !argv.contains("--conversation"),
        "a plain turn resumes nothing: {argv}"
    );

    let sessions = session::list_from(&fixture.sessions_dir()).unwrap();
    assert_eq!(
        sessions.len(),
        3,
        "the turn started its own session even though one could have been continued"
    );
    assert_eq!(
        session::load_from(&fixture.sessions_dir(), &newer.id)
            .unwrap()
            .len(),
        1,
        "the continuable session was left alone"
    );
    assert_eq!(
        session::load_from(&fixture.sessions_dir(), &older.id)
            .unwrap()
            .len(),
        1
    );

    let fresh = sessions
        .iter()
        .find(|summary| summary.id != older.id && summary.id != newer.id)
        .expect("the new session is listed");
    assert_eq!(fresh.turns, 1);
    assert_eq!(fresh.provider, "agy");
    assert_eq!(fresh.cwd, fixture.work().display().to_string());
    assert!(
        stderr(&output).contains(&format!("[session] {}", fresh.id)),
        "its id is surfaced like any other turn's: {}",
        stderr(&output)
    );
}

#[test]
fn the_command_line_refuses_two_selectors_and_spawns_nothing() {
    let combos: [&[&str]; 3] = [
        &["--continue", "--session", "s-1000-1"],
        &["--continue", "--resume", "conv-old"],
        &["--session", "s-1000-1", "--resume", "conv-old"],
    ];

    for extra in combos {
        let fixture = Fixture::new();
        two_sessions(&fixture);

        let output = fixture.run_stub(Stub::Args, &fixture.args("next task", extra));
        assert_eq!(
            output.status.code(),
            Some(2),
            "{} gave {:?}",
            extra.join(" "),
            output.status.code()
        );
        assert!(
            stderr(&output).contains("select the conversation to continue"),
            "{}",
            stderr(&output)
        );
        assert_eq!(
            fixture.stub_calls(),
            0,
            "a contradictory selection is refused before anything is spawned"
        );
    }
}

#[test]
fn continue_on_an_empty_store_is_an_error_rather_than_a_fresh_turn() {
    let fixture = Fixture::new();

    let output = fixture.run_stub(Stub::Args, &fixture.args("next task", &["--continue"]));

    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    let err = stderr(&output);
    assert!(err.contains("no session to continue"), "{err}");
    assert!(err.contains("agy"), "{err}");
    assert!(
        err.contains(&fixture.work().display().to_string()),
        "the directory it looked in: {err}"
    );
    assert_eq!(fixture.stub_calls(), 0, "no turn was run");
    assert!(
        !fixture.sessions_dir().exists(),
        "and no session was started behind the user's back"
    );
}

#[test]
fn an_unknown_session_id_is_refused_on_the_command_line() {
    let fixture = Fixture::new();
    two_sessions(&fixture);

    let output = fixture.run_stub(
        Stub::Args,
        &fixture.args("next task", &["--session", "s-9999-9"]),
    );

    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert!(stderr(&output).contains("s-9999-9"), "{}", stderr(&output));
    assert_eq!(fixture.stub_calls(), 0);
}

/// `RISKS.md` §6, payload level: a resumed attempt that fails because the
/// conversation is gone is replaced by pie's own transcript — the earlier turn's
/// prompt and answer, then the task.
#[test]
fn a_stale_resume_is_rebuilt_from_the_transcript() {
    let fixture = Fixture::new();
    two_sessions(&fixture);

    let selection = print::select(
        &fixture.sessions_dir(),
        &cli(&["-p", "next task", "--continue"]),
        "agy",
        &fixture.work(),
    )
    .unwrap();

    let stale = PieError::protocol(
        "agy",
        "stdout closed without a terminal `result` event (exit code: 1); stderr: Error: \
         conversation not found",
    );

    let seeded = print::rebootstrap_prompt(&selection, &stale, false, "next task")
        .expect("a purged conversation is rebuilt, not reported");
    assert!(
        seeded.contains("newer task"),
        "the earlier prompt: {seeded}"
    );
    assert!(seeded.contains("NEWER ANSWER"), "and its answer: {seeded}");
    assert!(seeded.ends_with("next task"), "then the task: {seeded}");

    // Anything else is reported instead. A resumed attempt that already produced
    // answer text did not fail because the conversation was empty…
    assert!(print::rebootstrap_prompt(&selection, &stale, true, "next task").is_none());
    // …an unrelated failure is not a re-bootstrap trigger…
    let unrelated = PieError::Backend {
        backend: "agy".into(),
        status: "ERROR".into(),
        detail: "model overloaded, try again later".into(),
    };
    assert!(print::rebootstrap_prompt(&selection, &unrelated, false, "next task").is_none());
    // …and a turn that resumed nothing has nothing to rebuild.
    assert!(
        print::rebootstrap_prompt(&print::Selection::Fresh, &stale, false, "next task").is_none()
    );
}

/// `RISKS.md` §6, end to end: the resumed attempt fails because the backend no
/// longer has the id, and the turn is rebuilt from pie's transcript exactly once.
///
/// What is visible here is the *decision* and the count. The rebuilt prompt itself
/// cannot be handed to the stand-in on Windows: `std` refuses to spawn a batch
/// file with a multi-line argument (`batch file arguments are invalid`), and the
/// seeded prompt is many lines. That text is asserted above, by calling the same
/// function `run` calls.
#[test]
fn a_stale_vendor_id_is_rebuilt_once_and_not_repeatedly() {
    let fixture = Fixture::new();
    let frame = seed(
        &fixture,
        &Seeded {
            id: "s-1000-1",
            provider: "agy",
            cwd: &fixture.work(),
            updated_at: 1_000,
            prompt: "first task",
            response: "EARLIER ANSWER",
            vendor: Some("conv-old"),
        },
    );
    write_index(&fixture, &[frame]);

    let output = fixture.run_stub(Stub::Stale, &fixture.args("next task", &["--continue"]));

    assert_eq!(
        stderr(&output).matches("re-bootstrapping").count(),
        1,
        "one rebuild, never a retry loop: {}",
        stderr(&output)
    );
    assert!(
        stderr(&output).contains("[session] s-1000-1"),
        "{}",
        stderr(&output)
    );

    // One row for the turn, in the session that was being continued: the failed
    // attempt is not a second turn, and a retry must not double the history.
    let rows = session::load_from(&fixture.sessions_dir(), "s-1000-1").unwrap();
    assert_eq!(rows.len(), 2, "the seeded turn plus one row for this turn");
    assert_eq!(
        rows[1].prompt, "next task",
        "the row keeps the user's text, never the seeded payload"
    );

    // Where the rebuilt turn can actually be delivered, it is an ordinary
    // successful turn, and the *new* vendor id is what gets recorded.
    #[cfg(not(windows))]
    {
        assert!(output.status.success(), "{}", stderr(&output));
        assert!(stdout(&output).contains("OK"), "{}", stdout(&output));
        assert_eq!(
            fixture.stub_calls(),
            2,
            "the resumed attempt plus exactly one re-bootstrap"
        );
        assert!(rows[1].ok);
        assert_eq!(
            rows[1].conversation_id.as_deref(),
            Some(FIXTURE_CONVERSATION_ID),
            "the id from the re-bootstrapped turn is the one worth recording"
        );
        assert!(
            stderr(&output).contains(&format!("[resume] {FIXTURE_CONVERSATION_ID}")),
            "{}",
            stderr(&output)
        );
    }

    // On Windows the second spawn never reaches the stand-in, so all this can say
    // is that the rebuild was attempted once. The vendor's own answer to a
    // multi-line prompt is covered by the payload test above and by G5.
    #[cfg(windows)]
    {
        assert_eq!(
            fixture.stub_calls(),
            1,
            "the rebuilt prompt cannot be delivered to a batch-file stand-in"
        );
        assert!(!output.status.success());
    }
}

/// The other direction, which is the expensive one: a resumed turn that failed
/// for a real reason must not be retried, because that spends subscription quota
/// twice and reports the retry's outcome instead of the real error.
#[test]
fn an_unrelated_failure_while_resuming_is_not_retried() {
    let fixture = Fixture::new();
    let frame = seed(
        &fixture,
        &Seeded {
            id: "s-1000-1",
            provider: "agy",
            cwd: &fixture.work(),
            updated_at: 1_000,
            prompt: "first task",
            response: "EARLIER ANSWER",
            vendor: Some("conv-old"),
        },
    );
    write_index(&fixture, &[frame]);

    let output = fixture.run_stub(Stub::Unrelated, &fixture.args("next task", &["--continue"]));

    assert!(!output.status.success(), "the failure stands");
    assert_eq!(
        fixture.stub_calls(),
        1,
        "a failure that is not a missing conversation is not a re-bootstrap trigger"
    );
    assert!(
        stderr(&output).contains("model overloaded"),
        "the real error is what the user sees: {}",
        stderr(&output)
    );

    let rows = session::load_from(&fixture.sessions_dir(), "s-1000-1").unwrap();
    assert_eq!(
        rows.len(),
        2,
        "the failed attempt is recorded in the session"
    );
    assert!(!rows[1].ok);
}

#[test]
fn the_json_init_event_carries_pie_session_id() {
    let fixture = Fixture::new();

    let output = fixture.run_stub(Stub::Args, &fixture.args("next task", &["--json"]));
    assert!(output.status.success(), "{}", stderr(&output));

    let sessions = session::list_from(&fixture.sessions_dir()).unwrap();
    let id = sessions[0].id.clone();

    let init = stdout(&output)
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|value| value["event"] == "init")
        .expect("the fixture emits an init event");

    assert_eq!(init["data"]["session_id"], id, "{init}");
    assert_eq!(
        init["data"]["conversation_id"], FIXTURE_CONVERSATION_ID,
        "the vendor's id is untouched beside it"
    );
    assert!(
        stderr(&output).contains(&format!("[session] {id}")),
        "and stderr names it too: {}",
        stderr(&output)
    );
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

// ----------------------------------------------- the backend switch (stage F) --

/// A transcript of `rows`, written directly, with fixed timestamps.
///
/// Stage A's writer has its own tests. What stage F needs is a transcript whose
/// *shape* is chosen — how long it is, which backend it ran on, which vendor id it
/// recorded — without running a turn for each, because a turn costs quota and the
/// switch is a payload transformation.
fn seed_rows(
    fixture: &Fixture,
    id: &str,
    provider: &str,
    vendor: Option<&str>,
    rows: &[(&str, &str)],
) -> SessionSummary {
    std::fs::create_dir_all(fixture.sessions_dir()).unwrap();

    let mut text = String::new();
    for (index, (prompt, response)) in rows.iter().enumerate() {
        let row = TurnRow {
            ts: 1_000 + index as u64,
            provider: provider.to_string(),
            model: "gemini-3.8-flash".to_string(),
            prompt: (*prompt).to_string(),
            response: (*response).to_string(),
            conversation_id: vendor.map(str::to_string),
            usage: Usage::default(),
            ok: true,
            stop_reason: "SUCCESS".to_string(),
        };
        text.push_str(&serde_json::to_string(&row).unwrap());
        text.push('\n');
    }
    std::fs::write(session::transcript_path(&fixture.sessions_dir(), id), text).unwrap();

    SessionSummary {
        id: id.to_string(),
        provider: provider.to_string(),
        cwd: fixture.work().display().to_string(),
        started_at: 1_000,
        updated_at: 1_000,
        turns: rows.len() as u64,
        preview: rows
            .first()
            .map(|(prompt, _)| (*prompt).to_string())
            .unwrap_or_default(),
    }
}

/// A recorded session with an empty transcript, and its own index frame.
///
/// The store tolerates one on purpose ("Empty transcript files read as empty"), and
/// it is the one shape whose rebuilt payload is a single line — which is the only
/// payload a batch-file stand-in can be handed on Windows, where `std` refuses to
/// spawn a `.bat` with a multi-line argument. The multi-line rebuild is asserted at
/// the payload level on every platform, and end to end where it can be delivered.
fn seed_empty(fixture: &Fixture, id: &str, provider: &str) -> SessionSummary {
    std::fs::create_dir_all(fixture.sessions_dir()).unwrap();
    std::fs::write(session::transcript_path(&fixture.sessions_dir(), id), "").unwrap();

    SessionSummary {
        id: id.to_string(),
        provider: provider.to_string(),
        cwd: fixture.work().display().to_string(),
        started_at: 1_000,
        updated_at: 1_000,
        turns: 0,
        preview: String::new(),
    }
}

/// A transcript of `turns` successful turns, for the compaction threshold.
fn long_transcript(turns: usize) -> Vec<TurnRow> {
    (0..turns)
        .map(|i| TurnRow {
            ts: 1_000 + i as u64,
            provider: "agy".into(),
            model: "gemini-3.8-flash".into(),
            prompt: format!("ask {i}"),
            response: format!("ANSWER-{i}"),
            conversation_id: Some("conv-old".into()),
            usage: Usage::default(),
            ok: true,
            stop_reason: "SUCCESS".into(),
        })
        .collect()
}

/// agy's argv for a request, as text. Pure: no process, so no quota.
fn agy_argv(request: &ChatRequest) -> Vec<String> {
    pie::provider::agy::build_args(&pie::config::Config::default(), request)
        .expect("a request this test built is buildable")
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect()
}

/// ACCEPTANCE §8, stage F: switching backends rebuilds the prompt from pie's own
/// transcript. The old vendor id belongs to the backend being left, so it is not
/// handed over; the transcript is.
#[test]
fn switching_rebuilds_the_prompt_from_the_transcript() {
    let fixture = Fixture::new();
    let frame = seed_rows(
        &fixture,
        "s-1000-1",
        "agy",
        Some("conv-old"),
        &[
            ("first task", "FIRST ANSWER"),
            ("second task", "SECOND ANSWER"),
        ],
    );
    write_index(&fixture, &[frame]);

    // `codex` is the new backend, so that is the provider `run` passes — and the
    // session it finds is an agy one, which is exactly what a switch is for.
    let selection = print::select(
        &fixture.sessions_dir(),
        &cli(&["-p", "next task", "--continue", "--switch", "codex"]),
        "codex",
        &fixture.work(),
    )
    .unwrap();

    assert!(selection.is_switch(), "the selection is a switch");
    let continuation = selection
        .continuation()
        .expect("the agy session is what is continued");
    assert_eq!(continuation.summary.id, "s-1000-1");

    let mut request = ChatRequest::new("next task");
    let rebuilt = print::apply_selection(&selection, &mut request, "next task")
        .expect("a switch rebuilds the payload");

    // Both halves of every earlier turn: that is what makes a rebuild a
    // continuation rather than a fresh question.
    assert!(request.prompt.contains("first task"), "{}", request.prompt);
    assert!(
        request.prompt.contains("FIRST ANSWER"),
        "{}",
        request.prompt
    );
    assert!(request.prompt.contains("second task"), "{}", request.prompt);
    assert!(
        request.prompt.contains("SECOND ANSWER"),
        "{}",
        request.prompt
    );
    assert!(request.prompt.ends_with("next task"), "{}", request.prompt);

    // Nothing was elided, so the counts say so rather than being absent.
    assert_eq!(rebuilt.turns, 2);
    assert_eq!(rebuilt.kept, 2);
    assert_eq!(rebuilt.dropped, 0);
    assert_eq!(rebuilt.prompt, request.prompt);
    assert!(print::switch_notice("agy", "codex", &rebuilt).contains("all 2 turn(s) re-sent"));

    // `--continue` under a switch matches the directory, not the backend: the
    // backend is what is being left, so a session elsewhere is still not this one.
    let elsewhere = fixture.root.path().join("other-work");
    std::fs::create_dir_all(&elsewhere).unwrap();
    let error = print::select(
        &fixture.sessions_dir(),
        &cli(&["-p", "next task", "--switch", "codex", "--continue"]),
        "codex",
        &elsewhere,
    )
    .unwrap_err();
    assert!(matches!(error, PieError::Config(_)), "{error:?}");
    assert!(
        error.to_string().contains("no session to continue"),
        "{error}"
    );
}

/// The cache-safety ordering (`TOKEN-ECONOMICS.md` §4, `DESIGN.md` §11): persona,
/// then the stable history, then the volatile task text — so a compaction can never
/// invalidate the prefix a vendor has already cached.
#[test]
fn a_switched_payload_keeps_the_persona_first_and_the_task_last() {
    const PERSONA: &str = "# Persona\nname: pie\ntone: terse";

    let fixture = Fixture::new();
    let frame = seed_rows(
        &fixture,
        "s-1000-1",
        "agy",
        Some("conv-old"),
        &[("first task", "FIRST ANSWER")],
    );
    write_index(&fixture, &[frame]);

    let selection = print::select(
        &fixture.sessions_dir(),
        &cli(&[
            "-p",
            "next task",
            "--session",
            "s-1000-1",
            "--switch",
            "codex",
        ]),
        "codex",
        &fixture.work(),
    )
    .unwrap();

    let mut request = ChatRequest::new("next task");
    request.persona = Some(PERSONA.to_string());
    let rebuilt = print::apply_selection(&selection, &mut request, "next task")
        .expect("a switch rebuilds the payload");

    // The bytes the backend receives: agy and codex fold the persona in front of the
    // prompt themselves, so the folded payload is what the ordering rule is about.
    // `fold` is the one implementation both providers use.
    let sent = request.folded_prompt();
    assert!(sent.starts_with(PERSONA), "{sent}");
    assert!(sent.ends_with("next task"), "{sent}");
    assert_eq!(
        sent,
        pie::profile::fold(Some(PERSONA), &rebuilt.prompt),
        "the rebuilt prompt is the folded payload minus the persona, never minus the task"
    );

    // …and inside the payload the order is the one `compact::assemble` owns: the
    // note, the head turns, and then the task.
    assert!(
        rebuilt.prompt.starts_with(print::REBUILD_NOTE),
        "{rebuilt:?}"
    );
    let first = rebuilt.prompt.find("first task").expect("the head turn");
    let task = rebuilt.prompt.rfind("next task").expect("the task text");
    assert!(first < task, "history before the task: {}", rebuilt.prompt);

    // The persona is not folded into the prompt as well: agy and codex would then
    // receive it twice, which is the drift `compact::assemble(None, ..)` prevents.
    assert!(
        !rebuilt.prompt.contains(PERSONA),
        "the persona travels beside the prompt, not inside it: {}",
        rebuilt.prompt
    );
}

/// ACCEPTANCE §8, stage F: a long session is compacted rather than replayed in full,
/// and the dropped count is reported instead of being silent.
#[test]
fn a_long_session_is_compacted_rather_than_replayed_in_full() {
    let mut rows = long_transcript(41);
    // One elided turn that failed, so the state block's outcome ledger has something
    // to say: `stop_reason` is where a failure class lives (§11), and the block is the
    // only place a rebuild renders it.
    rows[5].ok = false;
    rows[5].response = String::new();
    rows[5].stop_reason = "timeout".into();

    let rebuilt = print::rebuild(&rows, "next task");

    assert_eq!(rebuilt.turns, 41, "the transcript is 41 turns");
    assert_eq!(
        rebuilt.kept, 10,
        "the head plus the tail window survive verbatim"
    );
    assert_eq!(
        rebuilt.dropped, 31,
        "the middle is what the block stands in for"
    );

    // The head and the tail are the transcript's own text…
    assert!(rebuilt.prompt.contains("ask 0"), "{}", rebuilt.prompt);
    assert!(rebuilt.prompt.contains("ANSWER-0"), "{}", rebuilt.prompt);
    assert!(rebuilt.prompt.contains("ask 40"), "{}", rebuilt.prompt);
    assert!(rebuilt.prompt.contains("ANSWER-40"), "{}", rebuilt.prompt);

    // …and the elided middle is not: its answers never reach the payload, because the
    // block lists what was asked and what changed, never half an answer.
    assert!(
        !rebuilt.prompt.contains("ANSWER-5"),
        "a dropped turn's answer is not replayed: {}",
        rebuilt.prompt
    );
    assert!(
        rebuilt.prompt.contains("- ask 5"),
        "but what was asked still is: {}",
        rebuilt.prompt
    );
    assert!(
        rebuilt
            .prompt
            .contains("## Compacted history (31 of 41 turns elided)"),
        "{}",
        rebuilt.prompt
    );
    assert!(
        rebuilt.prompt.contains("- 1 failed: timeout"),
        "the failure class of a dropped turn is kept: {}",
        rebuilt.prompt
    );
    assert!(rebuilt.prompt.ends_with("next task"), "{}", rebuilt.prompt);

    // The count is *reported*: silence would be history disappearing from a payload
    // the user paid for.
    let notice = print::switch_notice("agy", "codex", &rebuilt);
    assert!(notice.contains("[switch] agy -> codex"), "{notice}");
    assert!(notice.contains("31 of 41 turns compacted"), "{notice}");
    assert!(notice.contains("10 re-sent"), "{notice}");
    assert!(notice.contains("from pie's transcript"), "{notice}");

    // The same call leaves a short session alone, which is what makes the threshold a
    // threshold rather than a second code path.
    let short = print::rebuild(&rows[..3], "next task");
    assert_eq!(short.turns, 3);
    assert_eq!(short.kept, 3);
    assert_eq!(short.dropped, 0);
    assert!(short.prompt.contains("ask 2"), "{}", short.prompt);
    assert!(short.prompt.contains("ANSWER-1"), "{}", short.prompt);
    assert!(print::switch_notice("agy", "codex", &short).contains("all 3 turn(s) re-sent"));
}

/// The core of the switch: the old vendor id belongs to the backend being left, and
/// must not be handed to the new one — not in the request, not in the payload, and
/// not in the argv the new backend would be spawned with.
#[test]
fn a_switched_turn_never_hands_the_old_vendor_id_to_the_new_backend() {
    let fixture = Fixture::new();
    let frame = seed_rows(
        &fixture,
        "s-1000-1",
        "codex",
        Some("conv-old"),
        &[("first task", "FIRST ANSWER")],
    );
    write_index(&fixture, &[frame]);

    let selection = print::select(
        &fixture.sessions_dir(),
        &cli(&["-p", "next task", "--continue", "--switch", "agy"]),
        "agy",
        &fixture.work(),
    )
    .unwrap();

    assert_eq!(
        selection.vendor_id(),
        None,
        "a switch hands the new backend no conversation id"
    );
    assert_eq!(
        selection.continuation().unwrap().vendor_id.as_deref(),
        Some("conv-old"),
        "…even though the transcript recorded one"
    );

    let mut request = ChatRequest::new("next task");
    print::apply_selection(&selection, &mut request, "next task");

    assert!(
        request.resume.is_none(),
        "the request resumes nothing: {:?}",
        request.resume
    );
    assert!(
        !request.prompt.contains("conv-old"),
        "and the id is not smuggled through the payload either: {}",
        request.prompt
    );

    // The proof at the level that would actually spend quota: the argv the new
    // backend would be spawned with. `build_args` is pure, so this holds on every
    // platform without a process.
    let argv = agy_argv(&request);
    assert!(
        !argv.iter().any(|arg| arg.contains("conv-old")),
        "the old id must not reach agy's argv: {argv:?}"
    );
    assert!(
        !argv.contains(&"--conversation".to_string()),
        "nor the flag that carries it: {argv:?}"
    );

    // The control: a turn that *does* resume an id still builds the flag, so the
    // assertions above are not passing because it could never appear.
    let mut resumed = ChatRequest::new("next task");
    resumed.resume = Some("conv-old".to_string());
    assert!(
        agy_argv(&resumed).contains(&"--conversation".to_string()),
        "the flag is built when there is an id to resume"
    );
}

/// ACCEPTANCE §8, stage F: the **new** vendor id is recorded in the session's next
/// turn — the same session, so the switch continued it rather than starting over.
#[test]
fn a_switched_turn_records_the_new_vendor_id_in_the_session() {
    let fixture = Fixture::new();
    // An empty transcript, so the rebuilt payload is the task text alone — the one
    // shape a batch-file stand-in can be handed on Windows. What is asserted here is
    // the recording path and the session identity; the multi-line payload is asserted
    // above, at the payload level.
    let frame = seed_empty(&fixture, "s-1000-1", "codex");
    write_index(&fixture, &[frame]);

    let output = fixture.run_stub(
        Stub::Args,
        &fixture.args("next task", &["--switch", "agy", "--session", "s-1000-1"]),
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).contains("OK"), "{}", stdout(&output));

    // The turn landed in the session it switched…
    let rows = session::load_from(&fixture.sessions_dir(), "s-1000-1").unwrap();
    assert_eq!(rows.len(), 1, "one turn, in the switched session");
    assert_eq!(rows[0].provider, "agy", "it ran on the new backend");
    assert_eq!(rows[0].prompt, "next task", "the row keeps the user's text");
    assert_eq!(
        rows[0].conversation_id.as_deref(),
        Some(FIXTURE_CONVERSATION_ID),
        "…and records the id the *new* backend minted"
    );

    // …and the index agrees with the row beneath it, which is what lets a later
    // `--continue` on the new backend find it.
    let sessions = session::list_from(&fixture.sessions_dir()).unwrap();
    assert_eq!(sessions.len(), 1, "no second session was minted");
    assert_eq!(sessions[0].provider, "agy");
    assert_eq!(sessions[0].turns, 1);

    let err = stderr(&output);
    assert!(err.contains("[switch] codex -> agy"), "{err}");
    assert!(err.contains("from pie's transcript"), "{err}");
    assert!(err.contains("[session] s-1000-1"), "{err}");
}

/// An unknown backend name is a configuration error naming the valid ones, and costs
/// nothing: nothing is spawned, so no quota is spent on a typo.
#[test]
fn an_unknown_switch_backend_is_refused_naming_the_valid_ones() {
    let error = print::Switch::from_cli(&cli(&["-p", "next", "--continue", "--switch", "gemini"]))
        .unwrap_err();
    assert!(matches!(error, PieError::Config(_)), "{error:?}");
    assert_eq!(error.exit_code(), 2, "a configuration error, not a crash");
    let message = error.to_string();
    assert!(message.contains("gemini"), "{message}");
    for known in ["agy", "codex", "claude"] {
        assert!(
            message.contains(known),
            "the valid ids are named: {message}"
        );
    }

    let fixture = Fixture::new();
    two_sessions(&fixture);
    let output = fixture.run_stub(
        Stub::Args,
        &fixture.args("next task", &["--continue", "--switch", "gemini"]),
    );
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("agy, codex, claude"),
        "{}",
        stderr(&output)
    );
    assert_eq!(fixture.stub_calls(), 0, "an unknown backend spawns nothing");
}

/// `--resume` names a vendor conversation on one backend, and leaving that backend is
/// the point of a switch, so the pair is refused rather than resolved.
#[test]
fn switching_while_resuming_a_vendor_id_is_refused() {
    let args = [
        "-p",
        "next",
        "--continue",
        "--resume",
        "conv-old",
        "--switch",
        "codex",
    ];
    let error = print::Switch::from_cli(&cli(&args)).unwrap_err();
    assert!(matches!(error, PieError::Config(_)), "{error:?}");
    assert_eq!(error.exit_code(), 2);
    let message = error.to_string();
    assert!(
        message.contains("--switch") && message.contains("--resume"),
        "the message names the conflict: {message}"
    );

    let fixture = Fixture::new();
    two_sessions(&fixture);
    let output = fixture.run_stub(Stub::Args, &fixture.args("next task", &args));
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert_eq!(
        fixture.stub_calls(),
        0,
        "refused before anything is spawned"
    );
}

/// A switch needs a session to switch. Silently running a fresh turn when the user
/// asked to leave one backend is the failure this milestone keeps designing against,
/// so it is an error that says what to pass instead.
#[test]
fn switching_without_a_session_is_refused_rather_than_starting_fresh() {
    let error = print::Switch::from_cli(&cli(&["-p", "next", "--switch", "codex"])).unwrap_err();
    assert!(matches!(error, PieError::Config(_)), "{error:?}");
    assert_eq!(error.exit_code(), 2);
    let message = error.to_string();
    assert!(
        message.contains("--continue") && message.contains("--session"),
        "{message}"
    );
    assert!(message.contains("--switch codex"), "{message}");

    // On the command line: exit 2, and no session started behind the user's back.
    // Built without `--model`, because the fixture's `--model agy/…` beside
    // `--switch codex` is a second, separate contradiction.
    let fixture = Fixture::new();
    let args = vec![
        "-p".to_string(),
        "next task".to_string(),
        "--cwd".to_string(),
        fixture.work().display().to_string(),
        "--switch".to_string(),
        "codex".to_string(),
    ];
    let output = fixture.run_stub(Stub::Args, &args);
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("needs a session to switch"),
        "{}",
        stderr(&output)
    );
    assert_eq!(fixture.stub_calls(), 0, "nothing was spawned");
    assert!(
        !fixture.sessions_dir().exists(),
        "and no session was started behind the user's back"
    );

    // A selector does not by itself make one: an empty store still has nothing to
    // switch, and says so in the switch's own terms — the backend was dropped from
    // the match, so naming it would send the user looking for the wrong thing.
    let args = vec![
        "-p".to_string(),
        "next task".to_string(),
        "--cwd".to_string(),
        fixture.work().display().to_string(),
        "--continue".to_string(),
        "--switch".to_string(),
        "codex".to_string(),
    ];
    let output = fixture.run_stub(Stub::Args, &args);
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    let err = stderr(&output);
    assert!(err.contains("no session to continue"), "{err}");
    assert!(err.contains("nothing to switch"), "{err}");
    assert_eq!(fixture.stub_calls(), 0);
}

/// `--model` may name the model *on* the new backend, and nothing else: a spec naming
/// a third backend is a contradiction, and silently preferring one of the two would
/// leave the subscription it names.
#[test]
fn a_switch_model_must_name_the_new_backend_or_none() {
    // The model on the new backend is carried into the turn.
    let switch = print::Switch::from_cli(&cli(&[
        "-p",
        "next",
        "--continue",
        "--switch",
        "codex",
        "--model",
        "codex/gpt-6-luna",
    ]))
    .unwrap()
    .expect("a switch");
    assert_eq!(switch.backend, "codex");
    assert_eq!(switch.model.as_deref(), Some("gpt-6-luna"));

    // No model: the new CLI picks its own, exactly as a `default_backend` turn does.
    let switch = print::Switch::from_cli(&cli(&["-p", "next", "--continue", "--switch", "codex"]))
        .unwrap()
        .expect("a switch");
    assert_eq!(switch.backend, "codex");
    assert_eq!(switch.model, None);

    // A model from the backend being left is refused.
    let error = print::Switch::from_cli(&cli(&[
        "-p",
        "next",
        "--continue",
        "--switch",
        "codex",
        "--model",
        "agy/gemini-3.8-flash",
    ]))
    .unwrap_err();
    assert!(matches!(error, PieError::Config(_)), "{error:?}");
    assert_eq!(error.exit_code(), 2);
    let message = error.to_string();
    assert!(
        message.contains("codex") && message.contains("agy"),
        "{message}"
    );

    // And no `--switch` at all reads nothing of this: the flag is opt-in.
    assert_eq!(
        print::Switch::from_cli(&cli(&["-p", "next"])).unwrap(),
        None
    );
}
