//! Live smoke tests — gate **G5**.
//!
//! # This spends real subscription quota
//!
//! These tests drive the real `agy` and `codex` CLIs. They are `#[ignore]`d, so
//! a plain `cargo test` never runs them, and they additionally require
//! `PIE_LIVE=1` so that `--ignored` cannot be used by accident. Both belts are
//! deliberate: quota is real money on a subscription plan.
//!
//! Run them with the gate runner:
//!
//! ```text
//! bash scripts/qc.sh --live        # Linux / WSL / macOS
//! pwsh scripts/qc.ps1 -Live        # Windows
//! ```
//!
//! # Approved models only
//!
//! `planning/ACCEPTANCE.md` §1 fixes the models: **`agy/gemini-3.8-flash`** and
//! **`codex/gpt-6-luna`**. [`APPROVED_MODELS`] is asserted rather than trusted,
//! so a careless edit to a prompt in this file cannot silently start billing a
//! more expensive model.
//!
//! # Why an end-to-end test and not a unit test
//!
//! Everything else in the suite tests parsers against recorded fixtures. Only
//! this file proves the three things fixtures cannot: that the CLI accepts the
//! argv pie builds, that the real stream matches the contract pie was written
//! against, and that a turn is actually recorded in the session log.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::process::Command;

/// The only models a live gate may use (`planning/ACCEPTANCE.md` §1).
const APPROVED_MODELS: [&str; 2] = ["agy/gemini-3.8-flash", "codex/gpt-6-luna"];

/// A trivial prompt: enough to produce a turn, cheap enough to be polite.
const PROMPT: &str = "Reply with the single word: ok";

/// A generous deadline so a slow cold start does not look like a failure, but
/// a hung CLI still fails instead of hanging the suite.
const TURN_TIMEOUT_SECS: &str = "300";

/// Whether live tests are allowed to run at all.
fn live_enabled() -> bool {
    std::env::var("PIE_LIVE").is_ok_and(|v| v == "1")
}

/// Refuse a vacuous pass.
///
/// Returning early when `PIE_LIVE` is unset made `cargo test -- --ignored`
/// report "5 passed" while executing nothing, with the "skipping" lines hidden
/// in captured stderr. The QC sweep flagged that as failing open. Panicking makes
/// the misuse loud instead of green.
fn require_live(what: &str) {
    assert!(
        live_enabled(),
        "refusing a vacuous pass: set PIE_LIVE=1 to run `{what}` - this spends real \
         subscription quota"
    );
}

/// Guard a live test, checking both the opt-in and the approved model.
fn ready(model: &str) {
    assert!(
        APPROVED_MODELS.contains(&model),
        "`{model}` is not an approved live model. Approved: {APPROVED_MODELS:?} (ACCEPTANCE.md §1)"
    );
    require_live(model);
}

/// Run `pie` with an isolated state directory.
///
/// `PIE_HOME` points at a throwaway directory for two reasons: the developer's
/// real `~/.pie/usage.jsonl` is not polluted with test rows, and the presence of
/// a session-log row becomes an assertion we can make.
fn run_pie(home: &Path, args: &[&str]) -> std::process::Output {
    run_pie_with_env(home, args, &[])
}

/// The same, with extra environment for the child.
///
/// Exists because one live test has to *opt in* to something the shipped default
/// turns off: `codex.quota_probe` defaults to `off`, so a test that asserts real
/// quota buckets must ask for the probe explicitly. Reaching a vendor endpoint from
/// a default is a decision the user owns; reaching it when a test says so is what
/// proves the capability still exists.
fn run_pie_with_env(home: &Path, args: &[&str], env: &[(&str, &str)]) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_pie"));
    command
        .args(args)
        .env("PIE_HOME", home)
        // Live runs must not inherit a strip-env opt-out from the shell.
        .env_remove("PIE_STRIP_ENV");
    for (key, value) in env {
        command.env(key, value);
    }
    command.output().expect("failed to run the pie binary")
}

/// Rows in the session log written under `home`.
fn session_rows(home: &Path) -> Vec<serde_json::Value> {
    let path = home.join("usage.jsonl");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

/// Assert one turn succeeded and was recorded.
fn assert_turn_recorded(home: &Path, provider: &str, output: &std::process::Output) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "`{provider}` turn failed with {:?}\nstdout: {stdout}\nstderr: {stderr}",
        output.status.code()
    );
    assert!(
        !stdout.trim().is_empty(),
        "`{provider}` produced no answer on stdout"
    );

    let rows = session_rows(home);
    assert_eq!(
        rows.len(),
        1,
        "expected exactly one session-log row, got {rows:#?}"
    );

    let row = &rows[0];
    assert_eq!(row["provider"], provider);
    assert_eq!(row["ok"], true, "row should record success: {row}");
    assert!(
        row["input_tokens"].as_u64().unwrap_or(0) > 0,
        "a real turn must report input tokens: {row}"
    );
    assert!(
        row["duration_ms"].as_u64().unwrap_or(0) > 0,
        "a real turn must report a duration: {row}"
    );
    // The frozen schema (DESIGN §7) must be honoured by every backend.
    for key in [
        "ts",
        "provider",
        "model",
        "input_tokens",
        "cached_input_tokens",
        "output_tokens",
        "reasoning_tokens",
        "cost_usd_micros",
        "credits_consumed",
        "credits_remaining",
        "duration_ms",
        "ok",
        "error_kind",
        "stop_reason",
    ] {
        assert!(
            row.get(key).is_some(),
            "session row is missing `{key}`: {row}"
        );
    }
}

#[test]
#[ignore = "spends real subscription quota; requires PIE_LIVE=1 and a logged-in agy"]
fn agy_turn_succeeds_and_is_recorded() {
    ready("agy/gemini-3.8-flash");
    let home = tempfile::tempdir().unwrap();

    let output = run_pie(
        home.path(),
        &[
            "-p",
            PROMPT,
            "--model",
            "agy/gemini-3.8-flash",
            "--timeout",
            TURN_TIMEOUT_SECS,
        ],
    );

    assert_turn_recorded(home.path(), "agy", &output);

    let row = &session_rows(home.path())[0];
    assert_eq!(row["model"], "gemini-3.8-flash");
}

/// The agy quota probe is a slash expansion, so it must take the
/// `--disable-slash-commands`-free path (`knowledge/agy.md`).
#[test]
#[ignore = "spends real subscription quota; requires PIE_LIVE=1 and a logged-in agy"]
fn agy_usage_probe_reports_buckets() {
    require_live("agy_usage_probe_reports_buckets");
    let home = tempfile::tempdir().unwrap();

    let output = run_pie(home.path(), &["usage", "agy"]);
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        output.status.success(),
        "`pie usage agy` failed: {stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("agy quota"),
        "expected a quota table, got:\n{stdout}"
    );
    // Asserting only the header, which `render_report` prints unconditionally,
    // let a parser returning zero buckets pass this gate. The QC sweep called that
    // out: the test name promised more than it checked.
    assert!(
        !stdout.contains("no buckets reported"),
        "the probe returned no buckets at all:\n{stdout}"
    );
    assert!(
        stdout.contains('%'),
        "expected at least one rendered remaining fraction:\n{stdout}"
    );

    // A slash expansion must not be a model turn, so it must not be logged.
    assert!(
        session_rows(home.path()).is_empty(),
        "a quota probe must not spend a turn"
    );
}

#[test]
#[ignore = "spends real subscription quota; requires PIE_LIVE=1 and a logged-in codex"]
fn codex_turn_succeeds_and_is_recorded() {
    ready("codex/gpt-6-luna");
    let home = tempfile::tempdir().unwrap();

    let output = run_pie(
        home.path(),
        &[
            "-p",
            PROMPT,
            "--model",
            "codex/gpt-6-luna",
            "--timeout",
            TURN_TIMEOUT_SECS,
        ],
    );

    assert_turn_recorded(home.path(), "codex", &output);

    let row = &session_rows(home.path())[0];
    assert_eq!(row["model"], "gpt-6-luna");
}

/// `pie usage codex` still reaches the endpoint **when the user opts in**.
///
/// This test is the opt-in half of a pair, and the pair exists because the default
/// was reversed on 2026-10-12: `codex.quota_probe` now ships as `off`, so the private
/// `wham/usage` endpoint is never called unless it is asked for. This test originally
/// asserted buckets with no environment set and therefore started failing the moment
/// the default changed — a live test is still an artefact derived from a decision, and
/// changing the decision did not change it until the failure forced the issue.
///
/// The other half — that the **default** reports `disabled by config` and spends
/// nothing — is asserted offline in `tests/acceptance.rs`, because it needs no model
/// tokens and no network, and a property that CAN be checked for free should not be
/// checked only by a gate that costs money.
#[test]
#[ignore = "spends real subscription quota; requires PIE_LIVE=1 and a logged-in codex"]
fn codex_usage_probe_reports_buckets_when_it_is_opted_in() {
    require_live("codex_usage_probe_reports_buckets_when_it_is_opted_in");
    let home = tempfile::tempdir().unwrap();

    let output = run_pie_with_env(
        home.path(),
        &["usage", "codex"],
        &[("PIE_CODEX_QUOTA_PROBE", "http")],
    );
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        output.status.success(),
        "`pie usage codex` failed: {stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("codex quota"),
        "expected a quota table, got:\n{stdout}"
    );
    assert!(
        !stdout.contains("no buckets reported"),
        "the opted-in probe returned no buckets at all:\n{stdout}"
    );
    assert!(
        stdout.contains('%'),
        "expected at least one rendered remaining fraction:\n{stdout}"
    );
    assert!(
        session_rows(home.path()).is_empty(),
        "a quota probe must not spend a turn"
    );
}

/// A live turn must never leak a credential into its output, even if the vendor
/// CLI prints one in an error path. `I3` / `I10`.
#[test]
#[ignore = "spends real subscription quota; requires PIE_LIVE=1 and a logged-in agy"]
fn a_live_turn_does_not_leak_key_material() {
    require_live("a_live_turn_does_not_leak_key_material");
    let home = tempfile::tempdir().unwrap();

    let output = run_pie(
        home.path(),
        &[
            "-p",
            PROMPT,
            "--model",
            "agy/gemini-3.8-flash",
            "--timeout",
            TURN_TIMEOUT_SECS,
        ],
    );

    for stream in [&output.stdout, &output.stderr] {
        let text = String::from_utf8_lossy(stream);
        for needle in ["sk-", "Bearer ", "eyJ", "access_token"] {
            assert!(
                !text.contains(needle),
                "live output contained `{needle}`:\n{text}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// M6 — the TUI renders a real turn
// ---------------------------------------------------------------------------

/// The token the model is asked to echo, so the assertion is about the *answer*
/// and not about the word "ok".
///
/// Same technique M3's persona gate uses: a payload-level test proves the bytes
/// were sent, and only a live turn proves the answer came back. Keystrokes cannot
/// be automated, but "the TUI renders a real vendor turn" is the substance of
/// M6's Done condition and *is* checkable (`ACCEPTANCE.md` §9, DESIGN §12).
const TUI_MARKER: &str = "PIE-TUI-7Q";

/// A prompt that cannot be satisfied by a model replying conversationally.
fn marker_prompt() -> String {
    format!("Reply with exactly this token and nothing else: {TUI_MARKER}")
}

/// Draw the app and return the screen as one whitespace-free string.
///
/// Whitespace-bearing comparison would be wrong here: a wrapped row break and a
/// real space are indistinguishable once the buffer is read back, and the marker
/// could land across a wrap on a narrow terminal. Stripping whitespace from both
/// sides makes the assertion independent of the layout.
fn rendered_flat(app: &pie::tui::App) -> String {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let backend = TestBackend::new(140, 40);
    let mut terminal = Terminal::new(backend).expect("in-memory terminal");
    terminal
        .draw(|frame| pie::tui::render(frame, app))
        .expect("draw");

    let buffer = terminal.backend().buffer();
    let area = buffer.area;
    let mut out = String::new();
    for y in 0..area.height {
        for x in 0..area.width {
            out.push_str(buffer[(x, y)].symbol());
        }
    }
    out.chars().filter(|c| !c.is_whitespace()).collect()
}

/// Fold a `pie -p --json` run's stdout into an `App`, the way the TUI's channel
/// would, and return it.
///
/// `--json` emits one `StreamEvent` per line on stdout, which is exactly the
/// normalised shape `apply_event` consumes — so a real turn can be driven through
/// the reducer without a terminal, without a channel, and without a second
/// code path that could disagree with the one the TUI actually uses.
fn app_from_json_session(model: &str, stdout: &str) -> pie::tui::App {
    let mut app = pie::tui::App::new(
        model.split('/').next().unwrap_or("agy"),
        Some(model.split('/').nth(1).unwrap_or("").to_string()),
        "live-g5",
    );

    let mut events = 0usize;
    for line in stdout.lines().filter(|l| !l.trim().is_empty()) {
        match serde_json::from_str::<pie::provider::StreamEvent>(line) {
            Ok(event) => {
                events += 1;
                pie::tui::apply_event(&mut app, &event);
            }
            // A non-JSON line on stdout would be a protocol fault, not a fixture
            // problem, so it is reported rather than skipped.
            Err(error) => panic!("stdout line is not a StreamEvent: {error}\nline: {line}"),
        }
    }
    assert!(
        events > 0,
        "a live turn must emit at least one event on stdout"
    );
    app
}

/// Assert a live turn reached the reducer, produced an answer, and that the
/// answer is visible in the rendered buffer.
fn assert_live_turn_renders(model: &str) {
    ready(model);
    let home = tempfile::tempdir().unwrap();
    let prompt = marker_prompt();

    let output = run_pie(
        home.path(),
        &[
            "--json",
            "-p",
            &prompt,
            "--model",
            model,
            "--timeout",
            TURN_TIMEOUT_SECS,
        ],
    );

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        output.status.success(),
        "live turn on `{model}` failed: {stderr}\nstdout: {stdout}"
    );

    // The turn is recorded once, with the frozen schema — the same parity claim
    // the TUI's own recording path makes (`ACCEPTANCE.md` §8, stage A).
    assert_turn_recorded(home.path(), model.split('/').next().unwrap_or(""), &output);

    let app = app_from_json_session(model, &stdout);

    let assistant: String = app
        .transcript
        .iter()
        .filter_map(|entry| match entry {
            pie::tui::Entry::Assistant(text) => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        !assistant.trim().is_empty(),
        "the reducer must hold the answer text; transcript: {:?}",
        app.transcript
    );
    assert!(
        assistant.contains(TUI_MARKER),
        "the model was asked to echo `{TUI_MARKER}`; reducer holds: {assistant:?}"
    );

    // The substance of M6's Done condition: the answer is *on the screen*, not
    // merely in the state. A reducer-only assertion would pass for a `render`
    // that dropped the transcript entirely.
    let screen = rendered_flat(&app);
    assert!(
        screen.contains(TUI_MARKER),
        "the rendered buffer must contain the answer; screen: {screen:?}"
    );

    // And it is a screen, not a debugging dump: no raw protocol or escape bytes.
    assert!(
        !screen.contains('\u{1b}'),
        "escape sequences must not be in the buffer"
    );
    assert!(
        !screen.contains("\"event\""),
        "raw JSON must not reach the screen: {screen:?}"
    );
}

#[test]
#[ignore = "spends real subscription quota; requires PIE_LIVE=1 and a logged-in agy"]
fn a_real_agy_turn_renders_through_the_tui_reducer() {
    // M6's Done condition is parity across BOTH live backends, so both are
    // asserted rather than one standing in for the other.
    assert_live_turn_renders("agy/gemini-3.8-flash");
}

#[test]
#[ignore = "spends real subscription quota; requires PIE_LIVE=1 and a logged-in codex"]
fn a_real_codex_turn_renders_through_the_tui_reducer() {
    assert_live_turn_renders("codex/gpt-6-luna");
}

// ---------------------------------------------------------------------------
// M9 — claude
// ---------------------------------------------------------------------------

/// M9's live check: the envelope when a subscription exists, and the **refusal** when it
/// does not.
///
/// `planning/ACCEPTANCE.md` §17 gives M9's G5 row as `n/a` rather than skipped: there is no
/// Claude subscription here. `~/.claude/` exists but holds no credential and no history
/// (checked 2026-10-12), so no envelope could be captured — which is why every claude
/// fixture in `tests/parse_claude.rs` is constructed from `knowledge/claude.md` rather than
/// recorded.
///
/// It is one test with two honest branches rather than an `#[ignore]`d one that would fail
/// the moment the `-Live` gate ran it. Without a credential the assertion is that the turn
/// **fails cleanly and names claude** — which is worth asserting today, because "refuses for
/// a reason the user can act on" is the property that keeps a missing login from looking
/// like a broken binary. With a credential it asserts the recorded turn, which is the
/// evidence `knowledge/claude.md` never had: that document says its own shapes are
/// *expected*, and only a live run can confirm them.
#[test]
#[ignore = "needs a Claude subscription to be meaningful; until then the credential branch \
            runs and the envelope branch is unreachable"]
fn a_live_claude_turn_matches_the_documented_envelope_or_refuses_cleanly() {
    let home = tempfile::tempdir().unwrap();
    let output = run_pie(
        home.path(),
        &[
            "-p",
            "Reply with the single word: ok",
            "--model",
            "claude/claude-sonnet-4-6",
            "--timeout",
            TURN_TIMEOUT_SECS,
        ],
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let credential = dirs_claude_credential();

    if !credential {
        assert!(
            !output.status.success(),
            "with no Claude credential the turn must not report success: \nstdout: {stdout}\nstderr: {stderr}"
        );
        assert!(
            stderr.contains("claude") || stdout.contains("claude"),
            "the refusal must name the backend, or a missing login looks like a broken binary: {stderr}"
        );
        // A refused turn **is** recorded, deliberately (M5 stage A: "the attempt is
        // recorded even when it failed" — a timeout still costs wall clock and belongs in
        // the totals). The first version of this assertion said the opposite, and the code
        // was right: what must hold is that the row says the turn **failed** and spent
        // nothing, not that no row exists.
        let rows = session_rows(home.path());
        assert!(
            rows.len() <= 1,
            "at most one attempt may be logged, got {rows:#?}"
        );
        if let Some(row) = rows.first() {
            assert_eq!(
                row["ok"], false,
                "a refused turn must not log as a success: {row}"
            );
            assert_eq!(
                row["input_tokens"].as_u64().unwrap_or(0),
                0,
                "a refused turn spent nothing, so it must not report tokens: {row}"
            );
        }
        return;
    }

    assert!(
        output.status.success(),
        "a live claude turn failed: {stderr}\nstdout: {stdout}"
    );
    // The token must not appear anywhere (I3), and the turn must be recorded exactly once
    // with the frozen schema — the same pair every other backend's live test asserts.
    assert_turn_recorded(home.path(), "claude", &output);
}

/// Whether claude has a stored subscription credential.
///
/// Read *for presence only*, through the same helper the auth path uses — never parsed, and
/// never logged (`RISKS.md` §10).
fn dirs_claude_credential() -> bool {
    pie::auth::claude_credentials_file().is_file()
}
