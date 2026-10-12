//! Offline tests for the agy backend (milestone M1).
//!
//! Every fixture in `tests/fixtures/` is a recorded sample — see
//! `tests/fixtures/README.md` for the provenance of each one. No test here
//! spawns the real CLI: `AgyRouter` needs no process (which is the whole point
//! of `EventRouter` being a trait), and the three tests that do spawn use a
//! stand-in script, so the suite spends no subscription quota.
//!
//! Covered, per `planning/ACCEPTANCE.md` §4: every event shape, the usage field
//! mapping, an `ERROR` status, garbage + unknown events around a valid `result`
//! (invariant I5), the truncated stream, and the model-catalog cache.

use std::path::{Path, PathBuf};
use std::time::Duration;

use pie::cache::{self, CacheEntry};
use pie::config::Config;
use pie::error::PieError;
use pie::provider::agy::{parse_models, parse_usage, AgyRouter, UNTERMINATED_STATUS};
use pie::provider::driver::EventRouter;
use pie::provider::{Bucket, Model, Provider, StreamEvent, ToolState};

/// The single agy backend, as `pie` would resolve it.
fn agy() -> Box<dyn Provider> {
    pie::provider::by_id("agy").expect("agy is in the registry")
}

// ---------------------------------------------------------------- fixtures --

fn fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

fn fixture_lines(name: &str) -> Vec<String> {
    std::fs::read_to_string(fixture_path(name))
        .unwrap_or_else(|e| panic!("fixture {name} is unreadable: {e}"))
        .lines()
        .map(str::to_string)
        .collect()
}

/// Feed a fixture through the router line by line, collecting what it emits.
fn replay(name: &str) -> (AgyRouter, Vec<StreamEvent>) {
    let mut router = AgyRouter::new();
    let events = fixture_lines(name)
        .iter()
        .filter_map(|line| router.on_line(line))
        .collect();
    (router, events)
}

fn kinds(events: &[StreamEvent]) -> Vec<&'static str> {
    events.iter().map(StreamEvent::kind).collect()
}

// ------------------------------------------------------------ event shapes --

#[test]
fn the_init_line_becomes_an_init_event() {
    let (_, events) = replay("agy_turn.jsonl");

    match &events[0] {
        StreamEvent::Init {
            conversation_id,
            cwd,
            tools,
        } => {
            assert_eq!(
                conversation_id.as_deref(),
                Some("8f3d9b2a-4c17-4e6a-9f0b-2d5c1a7e6b93")
            );
            assert_eq!(cwd.as_deref(), Some(Path::new("/work/pie")));
            assert_eq!(
                tools,
                &vec![
                    "view_file".to_string(),
                    "run_command".to_string(),
                    "grep_search".to_string()
                ]
            );
        }
        other => panic!("wrong first event: {other:?}"),
    }
}

#[test]
fn an_agent_response_step_becomes_a_text_delta() {
    let (_, events) = replay("agy_turn.jsonl");

    let deltas: Vec<&str> = events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::TextDelta(text) => Some(text.as_str()),
            _ => None,
        })
        .collect();

    assert_eq!(deltas, vec!["OK\n"]);
}

#[test]
fn a_tool_step_reports_active_then_done() {
    let (_, events) = replay("agy_turn.jsonl");

    let steps: Vec<(String, ToolState)> = events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::ToolStep { name, state, .. } => Some((name.clone(), *state)),
            _ => None,
        })
        .collect();

    assert_eq!(
        steps,
        vec![
            ("run_command".to_string(), ToolState::Active),
            ("run_command".to_string(), ToolState::Done),
        ]
    );
}

#[test]
fn a_failed_tool_step_reports_error() {
    let mut router = AgyRouter::new();
    let event = router
        .on_line(
            r#"{"event":"step_update","step_update":{"step_type":"tool","state":"ERROR","tool_name":"run_command","tool_info":"exit 1"}}"#,
        )
        .expect("a tool step is user-visible");

    match event {
        StreamEvent::ToolStep { name, state, info } => {
            assert_eq!(name, "run_command");
            assert_eq!(state, ToolState::Error);
            assert_eq!(info.as_deref(), Some("exit 1"));
        }
        other => panic!("wrong event: {other:?}"),
    }
}

#[test]
fn a_bare_step_update_progress_tick_produces_no_event() {
    let mut router = AgyRouter::new();

    assert!(
        router
            .on_line(
                r#"{"event":"step_update","step_update":{"step_index":1,"state":"DONE","step_type":"thinking"}}"#
            )
            .is_none(),
        "a progress tick carries no user-visible state"
    );
    assert!(router.on_line(r#"{"event":"step_update"}"#).is_none());
    assert!(router.on_line("   ").is_none(), "a blank line is skipped");
    assert!(!router.saw_terminal());
    assert_eq!(router.text(), "");
}

#[test]
fn the_result_line_becomes_a_done_event_and_a_turn_outcome() {
    let (router, events) = replay("agy_turn.jsonl");
    assert!(router.saw_terminal());

    match events.last().expect("the fixture has events") {
        StreamEvent::Done { text, status } => {
            assert_eq!(text, "OK\n");
            assert_eq!(status, "SUCCESS");
        }
        other => panic!("expected a terminal Done, got {other:?}"),
    }

    let outcome = router.outcome(Some(0), "exit 0", "").expect("success");
    assert_eq!(outcome.text, "OK\n");
    assert_eq!(outcome.status, "SUCCESS");
    assert_eq!(
        outcome.conversation_id.as_deref(),
        Some("8f3d9b2a-4c17-4e6a-9f0b-2d5c1a7e6b93")
    );
    assert_eq!(outcome.usage.input_tokens, 20704);
    assert_eq!(outcome.usage.output_tokens, 20);
    assert_eq!(outcome.usage.thinking_tokens, 19);
    assert_eq!(outcome.usage.cache_read_tokens, 0);
    assert_eq!(outcome.usage.total_tokens, 20724);
}

#[test]
fn usage_fields_map_by_agy_name_not_by_codex_name() {
    let mut router = AgyRouter::new();
    router.on_line(
        r#"{"event":"result","result":{"status":"SUCCESS","response":"OK\n","usage":{"input_tokens":10,"output_tokens":20,"thinking_tokens":30,"cache_read_tokens":40,"total_tokens":100,"cached_input_tokens":999,"reasoning_output_tokens":999}}}"#,
    );

    let outcome = router.outcome(Some(0), "exit 0", "").expect("success");
    assert_eq!(
        outcome.usage.thinking_tokens, 30,
        "thinking_tokens → thinking_tokens"
    );
    assert_eq!(
        outcome.usage.cache_read_tokens, 40,
        "cache_read_tokens → cache_read_tokens"
    );
    assert_eq!(outcome.usage.input_tokens, 10);
    assert_eq!(outcome.usage.output_tokens, 20);
}

// ----------------------------------------------------------------- errors --

#[test]
fn an_error_status_is_a_backend_error_with_exit_code_one() {
    let (router, _) = replay("agy_error_result.jsonl");

    let error = router
        .outcome(Some(1), "exit 1", "")
        .expect_err("an ERROR status is a failure");

    match &error {
        PieError::Backend {
            backend,
            status,
            detail,
        } => {
            assert_eq!(backend, "agy");
            assert_eq!(status, "ERROR");
            assert!(
                detail.contains("quota exhausted for gemini-3.8-flash"),
                "the payload's own `error` field is the detail: {detail}"
            );
        }
        other => panic!("expected PieError::Backend, got {other:?}"),
    }
    assert_eq!(error.exit_code(), 1);
}

#[test]
fn a_cancelled_status_is_also_a_backend_error() {
    let (router, _) = replay("agy_cancelled_result.jsonl");

    let error = router
        .outcome(Some(130), "signal 2", "interrupted by the user\n")
        .expect_err("CANCELLED is a failure");

    match error {
        PieError::Backend { status, detail, .. } => {
            assert_eq!(status, "CANCELLED");
            // This fixture has no `error` field, so the bounded stderr tail is
            // the fallback explanation.
            assert!(
                detail.contains("interrupted by the user"),
                "detail: {detail}"
            );
        }
        other => panic!("expected PieError::Backend, got {other:?}"),
    }
}

#[test]
fn a_secret_shaped_error_detail_is_redacted() {
    let blob = "A".repeat(90);
    let mut router = AgyRouter::new();
    router.on_line(&format!(
        r#"{{"event":"result","result":{{"status":"ERROR","error":"auth failed token={blob}"}}}}"#
    ));

    let error = router
        .outcome(Some(1), "exit 1", "")
        .expect_err("ERROR is a failure");

    assert!(
        !error.to_string().contains(&blob),
        "a credential-shaped run must never reach an error message: {error}"
    );
}

#[test]
fn an_unrecognised_status_fails_loudly_rather_than_claiming_success() {
    let mut router = AgyRouter::new();
    router.on_line(r#"{"event":"result","result":{"status":"PARTIAL","response":"half"}}"#);

    match router.outcome(Some(0), "exit 0", "") {
        Err(PieError::Backend { status, .. }) => assert_eq!(status, "PARTIAL"),
        other => panic!("a new status must be visible, not silently success: {other:?}"),
    }
}

#[test]
fn a_result_without_a_status_field_still_succeeds_on_a_zero_exit() {
    let mut router = AgyRouter::new();
    router.on_line(r#"{"event":"result","result":{"response":"OK\n","usage":{"input_tokens":5}}}"#);

    let outcome = router.outcome(Some(0), "exit 0", "").expect("success");
    assert_eq!(outcome.text, "OK\n");
    assert_eq!(outcome.status, "UNKNOWN", "the missing field stays visible");

    // …but the same payload with a failing exit code is not a success.
    assert!(router.outcome(Some(2), "exit 2", "").is_err());
}

// ------------------------------------------------------- tolerance (I5) ----

#[test]
fn garbage_and_unknown_events_around_a_valid_result_still_succeed() {
    let (router, events) = replay("agy_garbage.jsonl");

    assert!(
        events
            .iter()
            .any(|event| matches!(event, StreamEvent::Unparsed(_))),
        "a non-JSON line is Unparsed, never fatal: {events:?}"
    );
    assert!(
        events.iter().any(
            |event| matches!(event, StreamEvent::Other { kind, .. } if kind == "mystery_future_event")
        ),
        "an unknown event kind is forwarded: {events:?}"
    );
    assert!(events
        .iter()
        .any(|event| matches!(event, StreamEvent::TextDelta(text) if text == "OK\n")));
    assert!(events
        .iter()
        .any(|event| matches!(event, StreamEvent::Done { .. })));

    let outcome = router.outcome(Some(0), "exit 0", "").expect("success");
    assert_eq!(outcome.text, "OK\n");
    assert_eq!(outcome.usage.input_tokens, 20704);
}

#[test]
fn a_json_line_without_an_event_field_is_forwarded_not_dropped() {
    let mut router = AgyRouter::new();
    let event = router
        .on_line(r#"{"phase":"warp","detail":{"n":1}}"#)
        .expect("parsed JSON is never silently dropped");

    match event {
        StreamEvent::Other { kind, raw } => {
            assert_eq!(kind, "unknown");
            assert_eq!(raw["phase"], "warp");
        }
        other => panic!("wrong event: {other:?}"),
    }
}

#[test]
fn a_malformed_payload_of_any_shape_does_not_abort_the_run() {
    let mut router = AgyRouter::new();

    // Wrong types everywhere: a string where an array belongs, a number where a
    // string belongs, a tool step with no name and an object for its info.
    router.on_line(r#"{"event":"init","conversation_id":42,"init":{"cwd":7,"tools":"view_file"}}"#);
    router.on_line(
        r#"{"event":"step_update","step_update":{"step_type":"tool","state":17,"tool_name":null,"tool_info":{"cmd":"ls"}}}"#,
    );
    router.on_line(
        r#"{"event":"step_update","step_update":{"step_type":"agent_response","text_delta":42}}"#,
    );
    // A delta on a step that is not `agent_response` is ignored: only that step
    // type carries answer text (`knowledge/agy.md`), and accepting any other type
    // let a future one — `thinking` here, or a step with no `step_type` at all —
    // inject text into the final answer.
    router.on_line(
        r#"{"event":"step_update","step_update":{"step_type":"thinking","text_delta":"NOPE"}}"#,
    );
    router.on_line(r#"{"event":"step_update","step_update":{"text_delta":"NOPE"}}"#);
    router.on_line(
        r#"{"event":"result","result":{"status":"SUCCESS","response":"done","usage":"nonsense"}}"#,
    );

    assert_eq!(
        router.text(),
        "42",
        "a non-string delta is stringified, and non-answer steps are ignored"
    );
    let outcome = router.outcome(Some(0), "exit 0", "").expect("success");
    assert_eq!(outcome.text, "done");
    assert_eq!(
        outcome.usage,
        pie::provider::Usage::default(),
        "a `usage` that is not an object leaves the accumulation alone"
    );
}

// ------------------------------------------------------- truncated stream --

/// Truncated stream, non-zero exit.
///
/// **The rule, and why.** `planning/DESIGN.md` §6 rule 7 and the M1 brief agree
/// that "no terminal event + non-zero exit" is an error, but name different
/// variants (`Backend` vs `protocol`). We follow the brief — `PieError::Protocol`,
/// exit 4 — and keep rule 7's diagnostics by folding the exit status and the
/// bounded stderr tail into the detail, so nothing rule 7 would have shown is
/// lost. The reason the error class matters at all is the session log: a killed
/// or crashed turn must never be written as `ok: true` with a `SUCCESS`
/// stop reason, because that is how a broken backend would silently burn quota.
#[test]
fn a_truncated_stream_with_a_non_zero_exit_is_a_protocol_error() {
    let (router, events) = replay("agy_truncated.jsonl");
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, StreamEvent::Done { .. })),
        "this fixture must have no terminal event"
    );

    let error = router
        .outcome(Some(1), "exit 1", "connection reset by peer\n")
        .expect_err("no terminal event on a failing exit");

    assert!(matches!(error, PieError::Protocol { .. }), "got {error:?}");
    assert_eq!(error.exit_code(), 4);
    assert!(
        error.to_string().contains("connection reset by peer"),
        "the stderr tail must survive into the error: {error}"
    );
}

/// Truncated stream, exit 0, text captured: a partial answer, not a failure.
/// The status is `UNTERMINATED` so the truncation is visible in the session log
/// instead of being laundered into `SUCCESS`.
#[test]
fn a_truncated_stream_with_exit_zero_and_text_is_a_success() {
    let (router, _) = replay("agy_truncated.jsonl");

    let outcome = router.outcome(Some(0), "exit 0", "").expect("success");
    assert_eq!(outcome.text, "partial answer\n");
    assert_eq!(outcome.status, UNTERMINATED_STATUS);
}

#[test]
fn a_truncated_stream_with_no_text_at_all_is_a_protocol_error() {
    let mut router = AgyRouter::new();
    router.on_line(r#"{"event":"init","conversation_id":"c","init":{"cwd":"/w","tools":[]}}"#);

    let error = router
        .outcome(Some(0), "exit 0", "")
        .expect_err("no answer");
    assert!(matches!(error, PieError::Protocol { .. }), "got {error:?}");
    assert_eq!(error.exit_code(), 4);
}

#[test]
fn a_child_killed_by_a_signal_is_treated_as_a_failure() {
    let (router, _) = replay("agy_truncated.jsonl");

    let error = router
        .outcome(None, "signal 9", "")
        .expect_err("a killed child produced no terminal event");
    assert!(matches!(error, PieError::Protocol { .. }), "got {error:?}");
}

// ------------------------------------------------- model + quota surfaces --

#[test]
fn the_recorded_model_catalog_groups_into_seven_models() {
    let models = parse_models(&fixture_lines("agy_models.txt"));

    // 18 rows in the capture, 7 distinct base ids.
    let sets: Vec<(&str, &[&str])> = vec![
        ("gemini-3.8-flash", &["low", "medium", "high"]),
        ("gemini-3.7-flash", &["low", "medium", "high"]),
        ("gemini-3.6-flash", &["low", "medium", "high"]),
        // No `medium`: the sets are per-model, which is why the catalog has to
        // be the source of truth for `--effort`.
        ("gemini-3.1-pro", &["low", "high"]),
        ("claude-opus-5-5", &["low", "medium", "high"]),
        ("claude-sonnet-5-5", &["low", "medium", "high"]),
        ("gpt-oss-120b", &["medium"]),
    ];

    assert_eq!(models.len(), sets.len(), "got {models:?}");
    for (model, (id, efforts)) in models.iter().zip(sets.iter()) {
        assert_eq!(model.id, *id, "got {models:?}");
        assert_eq!(model.efforts, efforts.to_vec(), "efforts for {id}");
        // The display name is the base name, with the effort parenthetical
        // removed — not the raw row.
        assert!(!model.display.contains('('), "got {}", model.display);
        assert!(
            !model.display.starts_with("Fetching"),
            "the banner is not a model: {models:?}"
        );
    }
}

/// The two effort encodings in the capture — the id suffix and the display
/// parenthetical — must agree on every row, otherwise preferring one over the
/// other would silently drop or invent an effort.
#[test]
fn the_recorded_catalog_agrees_with_itself_on_every_effort() {
    let mut checked = 0usize;

    for line in fixture_lines("agy_models.txt") {
        let Some((id, display)) = line.split_once(char::is_whitespace) else {
            continue; // the banner
        };
        let Some((base, suffix)) = id.rsplit_once('-') else {
            continue;
        };
        let display = display.trim();
        let Some(open) = display.rfind('(') else {
            panic!("{id} has an effort suffix but no parenthetical: {display}");
        };
        let parenthetical = display[open + 1..]
            .strip_suffix(')')
            .unwrap_or_default()
            .to_ascii_lowercase();

        assert_eq!(suffix, parenthetical, "row `{line}` disagrees with itself");
        assert!(!base.is_empty());
        checked += 1;
    }

    assert_eq!(checked, 18, "every catalog row must be checked");
}

/// The verified `/usage` capture, asserted field by field.
///
/// Every earlier version of this test was written against a **corrupted**
/// capture: the probe had run from git-bash, which rewrites a leading-slash
/// argument into a Windows path, so agy received `C:/Program Files/Git/usage` as
/// prompt text and answered with no quota payload at all. The real capture has the
/// table at `command.data.groups[].buckets[]`, and this is what it must map.
#[test]
fn the_recorded_usage_capture_maps_both_groups() {
    let report = parse_usage(&fixture_lines("agy_usage.json"));

    assert_eq!(report.backend, "agy");
    assert_eq!(report.source, "agy --print /usage");
    assert_eq!(
        report.buckets.len(),
        4,
        "2 groups x 2 buckets: {:?}",
        report.buckets
    );

    let labels: Vec<&str> = report.buckets.iter().map(|b| b.label.as_str()).collect();
    assert_eq!(
        labels,
        vec![
            "Gemini Models / weekly",
            "Gemini Models / 5h",
            "Claude and GPT models / weekly",
            "Claude and GPT models / 5h",
        ]
    );

    // Both groups carry a bucket whose `window` is `weekly`, and both are named
    // "Weekly Limit Remaining" in the payload. Only the group name tells the two
    // rows apart, which is why the label leads with it.
    let weekly: Vec<&Bucket> = report
        .buckets
        .iter()
        .filter(|bucket| bucket.label.ends_with("/ weekly"))
        .collect();
    assert_eq!(weekly.len(), 2, "got {:?}", report.buckets);
    assert_ne!(
        weekly[0].label, weekly[1].label,
        "the two weekly rows must be distinguishable"
    );

    // `remaining_fraction` and `reset_time` land on the shared `Bucket` fields.
    assert_eq!(
        report.buckets[0].remaining_fraction,
        Some(0.884968638420105)
    );
    assert_eq!(weekly[0].remaining_fraction, Some(0.884968638420105));
    assert_eq!(weekly[1].remaining_fraction, Some(0.0));
    assert_eq!(
        report.buckets[0].reset_at.as_deref(),
        Some("2026-10-15T01:32:32Z")
    );
    assert_eq!(
        report.buckets[1].reset_at.as_deref(),
        Some("2026-10-12T16:00:37Z")
    );

    // `3p-5h` has no `reset_time` at all: absent is `None`, not an error, and
    // the bucket still maps.
    let claude_5h = report.buckets.last().expect("four buckets");
    assert_eq!(claude_5h.label, "Claude and GPT models / 5h");
    assert_eq!(claude_5h.reset_at, None);
    assert_eq!(claude_5h.remaining_fraction, Some(1.0));

    // A free probe that returns `num_turns: 0` and all-zero `usage` is expected,
    // not a failure — so a clean payload must produce no caveat at all.
    assert!(
        report.notes.is_empty(),
        "the verified payload needs no note: {:?}",
        report.notes
    );
}

/// A payload with no `command` key at all: the buckets are genuinely absent.
///
/// An empty table must not read as "quota available" or "out of quota", and the
/// note must say the *payload* was absent — not that the CLI cannot report quota,
/// which the verified capture disproves.
#[test]
fn a_usage_envelope_without_command_data_is_a_missing_bucket_note() {
    let lines = vec![
        r#"{"conversation_id":"","status":"SUCCESS","response":"","duration_seconds":0,"num_turns":0,"usage":{"input_tokens":0,"output_tokens":0,"thinking_tokens":0,"cache_read_tokens":0,"total_tokens":0}}"#
            .to_string(),
    ];

    let report = parse_usage(&lines);
    assert!(report.buckets.is_empty());
    assert_eq!(report.source, "agy --print /usage");

    let notes = report.notes.join(" ");
    assert!(
        notes.contains("no quota buckets"),
        "notes: {:?}",
        report.notes
    );
    assert!(
        notes.contains("MISSING, not zero"),
        "notes: {:?}",
        report.notes
    );
    assert!(
        notes.contains("command.data"),
        "the note names the absent payload, not a CLI limitation: {:?}",
        report.notes
    );
}

// ------------------------------------------- the three tests that spawn ----

/// Serialises tests that mutate `PIE_HOME` (process-global, parallel threads).
static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Points `PIE_HOME` at a scratch directory, restoring it on drop.
struct PinHome {
    previous: Option<String>,
}

impl PinHome {
    fn set(path: &Path) -> Self {
        let previous = std::env::var("PIE_HOME").ok();
        std::env::set_var("PIE_HOME", path);
        Self { previous }
    }
}

impl Drop for PinHome {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(value) => std::env::set_var("PIE_HOME", value),
            None => std::env::remove_var("PIE_HOME"),
        }
    }
}

/// A stand-in `agy` that ignores its argv and prints a recorded fixture.
///
/// The vendor CLI is never invoked, so these tests spend no quota; they exist to
/// prove the *wiring* (resolve → spawn → parse → cache), not the vendor.
fn stand_in_agy(dir: &Path, fixture: &str) -> PathBuf {
    let payload = fixture_path(fixture);

    #[cfg(windows)]
    {
        let script = dir.join("stand-in-agy.bat");
        std::fs::write(
            &script,
            format!("@echo off\r\ntype \"{}\"\r\n", payload.display()),
        )
        .expect("write the stand-in");
        script
    }

    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;
        let script = dir.join("stand-in-agy.sh");
        std::fs::write(
            &script,
            format!("#!/bin/sh\nexec cat \"{}\"\n", payload.display()),
        )
        .expect("write the stand-in");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
            .expect("chmod the stand-in");
        script
    }
}

#[tokio::test]
async fn a_fresh_model_cache_is_served_without_spawning_the_cli() {
    let _guard = ENV_LOCK.lock().await;
    let scratch = tempfile::tempdir().expect("tempdir");
    let _home = PinHome::set(scratch.path());

    let cached = vec![
        Model::plain("gemini-3.8-flash-high", "Gemini 3.8 Flash (High)"),
        Model::plain("gpt-oss-120b-medium", "GPT-OSS 120B (Medium)"),
    ];
    cache::write_now(&pie::paths::agy_model_cache(), &cached).expect("seed the cache");

    // `agy` points at a path that does not exist: if this test spawned anything
    // at all it would fail with `BinaryMissing` instead of hitting the cache.
    let mut cfg = Config::default();
    cfg.agy.bin = scratch
        .path()
        .join("not-installed-agy")
        .display()
        .to_string();

    let models = agy().list_models(&cfg).await.expect("a cache hit");
    assert_eq!(models, cached);
}

#[tokio::test]
async fn a_stale_model_cache_is_refreshed_from_the_cli() {
    let _guard = ENV_LOCK.lock().await;
    let scratch = tempfile::tempdir().expect("tempdir");
    let _home = PinHome::set(scratch.path());

    let path = pie::paths::agy_model_cache();
    let stale = CacheEntry::new(vec![Model::plain("stale-model-1", "Stale")], 1);
    cache::write(&path, &stale).expect("seed a stale entry");

    let mut cfg = Config::default();
    cfg.agy.bin = stand_in_agy(scratch.path(), "agy_models.txt")
        .display()
        .to_string();

    let models = agy().list_models(&cfg).await.expect("a refresh");
    assert!(
        !models.iter().any(|m| m.id == "stale-model-1"),
        "a stale entry must not be served: {models:?}"
    );
    let from_cli = parse_models(&fixture_lines("agy_models.txt"));
    assert_eq!(
        models
            .iter()
            .map(|m| (m.id.clone(), m.display.clone(), m.efforts.clone()))
            .collect::<Vec<_>>(),
        from_cli
            .iter()
            .map(|m| (m.id.clone(), m.display.clone(), m.efforts.clone()))
            .collect::<Vec<_>>(),
        "the served catalog is the recorded one, base ids and effort sets included"
    );
    assert_eq!(models.len(), 7, "seven base ids, not eighteen rows");
    // `list_models` fills in the effort a bare `--model` will use: `medium`
    // everywhere it exists, and the closest supported one where it does not.
    for model in &models {
        let expected = if model.efforts.contains(&"medium".to_string()) {
            "medium"
        } else {
            "low"
        };
        assert_eq!(model.default_effort.as_deref(), Some(expected), "{model:?}");
    }

    // …and the refreshed catalog was written back, fresh.
    let entry: CacheEntry<Vec<Model>> = cache::read(&path).expect("the cache was rewritten");
    assert_eq!(entry.value, models);
    assert!(entry.is_fresh(
        Duration::from_secs(cfg.agy.model_cache_ttl_secs),
        pie::session_log::now_millis()
    ));
}

#[tokio::test]
async fn an_unparseable_catalog_is_an_error_and_is_not_cached() {
    let _guard = ENV_LOCK.lock().await;
    let scratch = tempfile::tempdir().expect("tempdir");
    let _home = PinHome::set(scratch.path());

    let mut cfg = Config::default();
    cfg.agy.bin = stand_in_agy(scratch.path(), "agy_usage.json")
        .display()
        .to_string();

    let error = agy()
        .list_models(&cfg)
        .await
        .expect_err("JSON is not a model catalog");
    assert!(matches!(error, PieError::Protocol { .. }), "got {error:?}");
    assert!(
        !pie::paths::agy_model_cache().exists(),
        "an empty catalog must never be cached"
    );
}

#[tokio::test]
async fn an_effort_the_model_does_not_offer_never_reaches_the_child() {
    let _guard = ENV_LOCK.lock().await;
    let scratch = tempfile::tempdir().expect("tempdir");
    let _home = PinHome::set(scratch.path());

    // The recorded catalog's real, non-uniform effort sets.
    cache::write_now(
        &pie::paths::agy_model_cache(),
        &parse_models(&fixture_lines("agy_models.txt")),
    )
    .expect("seed the catalog");

    let mut cfg = Config::default();
    // No binary is needed: the argv is validated before anything is spawned.
    cfg.agy.bin = scratch
        .path()
        .join("not-installed-agy")
        .display()
        .to_string();

    let mut request = pie::provider::ChatRequest::new("hi");
    request.model = Some("gemini-3.1-pro".to_string());
    request.effort = Some("medium".to_string());
    request.cwd = scratch.path().to_path_buf();

    let mut sink = |_event: StreamEvent| {};
    let error = agy()
        .stream(&cfg, &request, &mut sink)
        .await
        .expect_err("gemini-3.1-pro has no medium effort");

    match &error {
        PieError::Config(message) => {
            assert!(message.contains("gemini-3.1-pro"), "{message}");
            assert!(message.contains("low|high"), "names the set: {message}");
        }
        other => panic!("expected a config error, got {other:?}"),
    }
    assert_eq!(
        error.exit_code(),
        2,
        "ACCEPTANCE.md: usage/config is exit 2"
    );
}

#[tokio::test]
async fn stream_normalises_a_recorded_turn_end_to_end() {
    let scratch = tempfile::tempdir().expect("tempdir");

    let mut cfg = Config::default();
    cfg.agy.bin = stand_in_agy(scratch.path(), "agy_turn.jsonl")
        .display()
        .to_string();

    let mut request = pie::provider::ChatRequest::new("hi");
    request.model = Some("gemini-3.8-flash".to_string());
    request.cwd = scratch.path().to_path_buf();

    let mut events: Vec<StreamEvent> = Vec::new();
    let outcome = {
        let mut sink = |event: StreamEvent| events.push(event);
        agy()
            .stream(&cfg, &request, &mut sink)
            .await
            .expect("the recorded turn streams")
    };

    assert_eq!(kinds(&events), vec!["init", "tool", "tool", "text", "done"]);
    assert_eq!(outcome.text, "OK\n");
    assert_eq!(outcome.status, "SUCCESS");
    assert_eq!(outcome.usage.input_tokens, 20704);
    assert_eq!(outcome.usage.thinking_tokens, 19);
}

#[tokio::test]
async fn usage_parses_a_recorded_slash_expansion() {
    let scratch = tempfile::tempdir().expect("tempdir");

    let mut cfg = Config::default();
    cfg.agy.bin = stand_in_agy(scratch.path(), "agy_usage.json")
        .display()
        .to_string();

    let report = agy()
        .usage(&cfg)
        .await
        .expect("the recorded capture parses");
    // No `assert_eq!(report, parse_usage(...))` here: that compared the result to
    // itself, since `Agy::usage` *is* `parse_usage`. It would have passed for any
    // parser at all. The assertions below are the ones with teeth.
    assert_eq!(report.buckets.len(), 4, "got {:?}", report.buckets);
    assert_eq!(report.buckets[0].label, "Gemini Models / weekly");
    assert!(
        report.source.contains("/usage"),
        "the report must name the probe it came from: {}",
        report.source
    );
}

/// A stand-in `agy` that emits one event and then never finishes.
fn stand_in_agy_that_hangs(dir: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        let script = dir.join("stand-in-agy-hang.bat");
        std::fs::write(
            &script,
            "@echo off\r\necho {\"event\":\"init\",\"conversation_id\":\"conv-hang\",\"init\":{\"cwd\":\"/w\",\"tools\":[]}}\r\nping -n 30 127.0.0.1 >nul\r\n",
        )
        .expect("write the hanging stand-in");
        script
    }

    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;
        let script = dir.join("stand-in-agy-hang.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\necho '{\"event\":\"init\",\"conversation_id\":\"conv-hang\",\"init\":{\"cwd\":\"/w\",\"tools\":[]}}'\nsleep 30\n",
        )
        .expect("write the hanging stand-in");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
            .expect("chmod the hanging stand-in");
        script
    }
}

/// `planning/ACCEPTANCE.md` §4 (M1 checks): "timeout ⇒ process tree killed,
/// `PieError::Timeout`, exit 5". Proven through the agy path itself, with a
/// stand-in child, so it costs no quota.
#[tokio::test]
async fn a_turn_that_exceeds_its_deadline_is_a_timeout_with_exit_code_five() {
    let scratch = tempfile::tempdir().expect("tempdir");

    let mut cfg = Config::default();
    cfg.agy.bin = stand_in_agy_that_hangs(scratch.path())
        .display()
        .to_string();

    let mut request = pie::provider::ChatRequest::new("hi");
    request.cwd = scratch.path().to_path_buf();
    request.timeout = Some(Duration::from_millis(300));

    let mut sink = |_event: StreamEvent| {};
    let started = std::time::Instant::now();
    let error = agy()
        .stream(&cfg, &request, &mut sink)
        .await
        .expect_err("the deadline must fire");

    assert!(matches!(error, PieError::Timeout { .. }), "got {error:?}");
    assert_eq!(error.exit_code(), 5);
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "the child was killed, not waited on"
    );
}

#[tokio::test]
async fn a_missing_binary_is_reported_before_anything_is_spawned() {
    let scratch = tempfile::tempdir().expect("tempdir");

    let mut cfg = Config::default();
    cfg.agy.bin = scratch
        .path()
        .join("not-installed-agy")
        .display()
        .to_string();

    let mut request = pie::provider::ChatRequest::new("hi");
    request.cwd = scratch.path().to_path_buf();

    let mut sink = |_event: StreamEvent| {};
    let error = agy()
        .stream(&cfg, &request, &mut sink)
        .await
        .expect_err("no binary, no turn");

    assert!(
        matches!(error, PieError::BinaryMissing { .. }),
        "got {error:?}"
    );
    assert_eq!(error.exit_code(), 6);
}
