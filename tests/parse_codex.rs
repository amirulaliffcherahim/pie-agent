//! M2 acceptance gate: codex JSONL → normalised events, outcome, catalog, quota.
//!
//! Fixtures come from `tests/fixtures/` (provenance in `fixtures/README.md`):
//! recorded samples from `knowledge/codex.md` plus item names read out of the
//! shipped `codex.exe` string table. Nothing here makes a network call or spawns
//! `codex`; `HOME`/`PIE_HOME` point at a `tempfile::tempdir()`, so the real
//! `~/.codex` and `~/.pie` are never touched.
//!
//! Requirement: `planning/ACCEPTANCE.md` §5. Design: `planning/DESIGN.md`
//! §3–4 (types), §6 (process/stream contract), §9 (codex argv).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::sync::{Mutex, MutexGuard, OnceLock};

use pie::auth::classify_codex_mode;
use pie::config::{CodexQuotaProbe, Config};
use pie::error::PieError;
use pie::provider::codex::{
    fallback_models, model_catalog, quota_disabled_report, require_subscription, shape_models,
    Codex, CodexRouter, USAGE_URL,
};
// The quota mapping is part of M2's acceptance, so it is tested in every build
// that has the feature on (all four gates) — and compiled out with it.
#[cfg(feature = "http")]
use pie::provider::codex::{buckets_from_usage, describe_http_failure, iso8601_utc, quota_notes};
use pie::provider::driver::EventRouter;
use pie::provider::{ChatRequest, Model, Provider, StreamEvent, ToolState};

/// The live-recorded four-line turn (`knowledge/codex.md`).
const TURN: &str = include_str!("fixtures/codex_turn.jsonl");

/// The same turn with drift around it: banner noise, tool items, a reasoning
/// item, `item.started`/`item.updated`, and a future event type (I5).
const NOISY: &str = include_str!("fixtures/codex_noisy.jsonl");

/// A trimmed copy of a real `~/.codex/models_cache.json`.
const MODELS: &str = include_str!("fixtures/models_cache.json");

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn lines(fixture: &'static str) -> Vec<&'static str> {
    fixture.lines().filter(|l| !l.trim().is_empty()).collect()
}

/// The first fixture line containing `needle` — keeps tests tied to a recorded
/// line rather than to a brittle index.
fn recorded(needle: &str) -> &'static str {
    lines(TURN)
        .into_iter()
        .find(|l| l.contains(needle))
        .unwrap_or_else(|| panic!("fixture has no line containing {needle:?}"))
}

fn recorded_noisy(needle: &str) -> &'static str {
    lines(NOISY)
        .into_iter()
        .find(|l| l.contains(needle))
        .unwrap_or_else(|| panic!("fixture has no line containing {needle:?}"))
}

fn router() -> CodexRouter {
    CodexRouter::new()
}

/// Points `HOME`, `USERPROFILE` and `PIE_HOME` at one temp dir.
///
/// Env is process-global while tests run on threads, so every test that touches
/// it takes the same lock (`src/testing.rs` is crate-private and unavailable
/// from an integration test).
#[derive(Debug)]
struct HomeGuard {
    _guard: MutexGuard<'static, ()>,
    saved: Vec<(&'static str, Option<String>)>,
}

impl HomeGuard {
    fn new(home: &Path) -> Self {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        let guard = LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        let keys = ["HOME", "USERPROFILE", "PIE_HOME"];
        let saved: Vec<(&'static str, Option<String>)> =
            keys.iter().map(|k| (*k, std::env::var(k).ok())).collect();

        let value = home.to_string_lossy().to_string();
        for key in keys {
            std::env::set_var(key, &value);
        }

        Self {
            _guard: guard,
            saved,
        }
    }
}

impl Drop for HomeGuard {
    fn drop(&mut self) {
        for (key, value) in &self.saved {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

/// Write the fixture catalog where codex itself keeps its cache.
fn write_codex_cache(home: &Path) {
    let dir = home.join(".codex");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("models_cache.json"), MODELS).unwrap();
}

/// A token-shaped secret, built at runtime so no credential-shaped literal
/// exists in this file (the static scan in `tests/no_secrets.rs` forbids one).
fn token_shaped() -> String {
    "A".repeat(96)
}

// ---------------------------------------------------------------------------
// Event mapping (ACCEPTANCE §5, bullets 1–6)
// ---------------------------------------------------------------------------

#[test]
fn thread_started_yields_init_with_the_conversation_id() {
    let mut r = router();
    let event = r.on_line(recorded("thread.started")).unwrap();

    match event {
        StreamEvent::Init {
            conversation_id,
            cwd,
            tools,
        } => {
            assert_eq!(
                conversation_id.as_deref(),
                Some("01a11e67-3c28-7cb0-bb6e-9753954cf6ce")
            );
            assert_eq!(cwd, None, "codex reports no cwd here");
            assert!(tools.is_empty());
        }
        other => panic!("thread.started must become Init, got {other:?}"),
    }

    // The id must survive into the outcome, or resume cannot work.
    r.on_line(recorded("agent_message"));
    r.on_line(recorded("turn.completed"));
    let outcome = r.outcome(Some(0), "exit 0", "").unwrap();
    assert_eq!(
        outcome.conversation_id.as_deref(),
        Some("01a11e67-3c28-7cb0-bb6e-9753954cf6ce")
    );
}

#[test]
fn turn_started_is_not_user_visible() {
    assert_eq!(
        router().on_line(recorded("turn.started")),
        None,
        "turn.started carries no user-visible state"
    );
}

#[test]
fn a_completed_agent_message_is_a_text_delta() {
    assert_eq!(
        router().on_line(recorded("agent_message")),
        Some(StreamEvent::TextDelta("OK".to_string()))
    );
}

#[test]
fn the_assistant_message_alias_is_also_answer_text() {
    let line = r#"{"type":"item.completed","item":{"id":"item_1","type":"assistant_message","text":"OK"}}"#;
    assert_eq!(
        router().on_line(line),
        Some(StreamEvent::TextDelta("OK".to_string()))
    );
}

#[test]
fn non_agent_item_types_are_never_text_deltas() {
    let mut r = router();
    let mut tool_steps = Vec::new();
    let mut others = Vec::new();

    let item_lines: Vec<&str> = lines(NOISY)
        .into_iter()
        .filter(|l| l.contains("\"item") && !l.contains("agent_message") && !l.contains("turn."))
        .collect();
    assert_eq!(
        item_lines.len(),
        4,
        "fixture must exercise four item events"
    );

    for line in item_lines {
        match r.on_line(line) {
            Some(StreamEvent::ToolStep { name, state, .. }) => tool_steps.push((name, state)),
            Some(StreamEvent::Other { kind, .. }) => others.push(kind),
            Some(StreamEvent::TextDelta(text)) => {
                panic!("a non-agent item became answer text: {text:?} in {line}")
            }
            other => panic!("unexpected event {other:?} for {line}"),
        }
    }

    assert_eq!(
        tool_steps,
        vec![
            ("command_execution".to_string(), ToolState::Active),
            ("command_execution".to_string(), ToolState::Active),
            ("command_execution".to_string(), ToolState::Done),
        ],
        "item.started/item.updated are active, item.completed is done"
    );
    assert_eq!(others, vec!["reasoning".to_string()]);
    assert_eq!(
        r.text(),
        "",
        "no item in this set may contribute answer text"
    );
}

#[test]
fn a_completed_item_with_a_non_zero_exit_code_is_a_failed_tool_step() {
    // The exit code is checked before the status string: an item can report
    // `status: "completed"` while its command exited non-zero, and the exit code
    // is the truth. This test previously asserted `Done` here, which contradicted
    // its own name — the QC sweep caught the mismatch.
    let line = r#"{"type":"item.completed","item":{"id":"item_1","type":"command_execution","command":"false","exit_code":1,"status":"completed"}}"#;
    match router().on_line(line).unwrap() {
        StreamEvent::ToolStep { state, .. } => assert_eq!(state, ToolState::Error),
        other => panic!("expected a tool step, got {other:?}"),
    }

    let no_status = r#"{"type":"item.completed","item":{"id":"item_1","type":"command_execution","command":"false","exit_code":1}}"#;
    match router().on_line(no_status).unwrap() {
        StreamEvent::ToolStep { state, .. } => assert_eq!(state, ToolState::Error),
        other => panic!("expected a tool step, got {other:?}"),
    }
}

#[test]
fn turn_completed_yields_usage_and_done() {
    let mut r = router();
    r.on_line(recorded("thread.started"));
    r.on_line(recorded("turn.started"));
    r.on_line(recorded("agent_message"));

    // One stdout line yields one event, so the terminal line is `Done` and the
    // token accounting travels in the outcome it produces.
    let terminal = r.on_line(recorded("turn.completed")).unwrap();
    assert_eq!(
        terminal,
        StreamEvent::Done {
            text: "OK".to_string(),
            status: "completed".to_string(),
        }
    );

    let outcome = r.outcome(Some(0), "exit 0", "").unwrap();
    assert_eq!(outcome.text, "OK");
    assert_eq!(outcome.status, "completed");
    assert_eq!(outcome.usage.input_tokens, 13_768);
    assert_eq!(outcome.usage.output_tokens, 5);
}

/// The renames are the requirement: codex's `cached_input_tokens` is pie's
/// `cache_read_tokens`, and `reasoning_output_tokens` is `thinking_tokens`.
#[test]
fn usage_field_names_are_renamed_on_mapping() {
    let mut r = router();
    // An agent message too: `outcome` now refuses a `turn.completed` with no
    // answer text, and this test is about field renames, not about that rule.
    r.on_line(
        r#"{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"OK"}}"#,
    );
    r.on_line(
        r#"{"type":"turn.completed","usage":{"input_tokens":13768,"cached_input_tokens":11008,"cache_write_input_tokens":256,"output_tokens":5,"reasoning_output_tokens":19}}"#,
    );
    let usage = r.outcome(Some(0), "exit 0", "").unwrap().usage;

    assert_eq!(usage.input_tokens, 13_768);
    assert_eq!(usage.cache_read_tokens, 11_008, "cached_input_tokens");
    assert_eq!(usage.cache_write_tokens, 256, "cache_write_input_tokens");
    assert_eq!(usage.output_tokens, 5);
    assert_eq!(usage.thinking_tokens, 19, "reasoning_output_tokens");
}

#[test]
fn unknown_event_types_are_forwarded_as_other() {
    match router().on_line(recorded_noisy("future.event")).unwrap() {
        StreamEvent::Other { kind, raw } => {
            assert_eq!(kind, "future.event");
            assert_eq!(raw["payload"]["added_by"], "a newer codex");
        }
        other => panic!("an unknown event must be forwarded, got {other:?}"),
    }
}

#[test]
fn a_non_json_line_is_unparsed_and_a_blank_line_is_skipped() {
    let mut r = router();
    assert_eq!(r.on_line("   "), None, "blank lines are skipped");

    match r
        .on_line("codex: warning: something wrote a non-JSON line")
        .unwrap()
    {
        StreamEvent::Unparsed(raw) => assert!(raw.contains("non-JSON")),
        other => panic!("a non-JSON line must be Unparsed, got {other:?}"),
    }

    // A truncated line is a parse failure too, never a panic (I6).
    assert!(matches!(
        r.on_line(r#"{"type":"turn.com"#).unwrap(),
        StreamEvent::Unparsed(_)
    ));
}

/// Invariant I5: a bad stdout line must never abort a run that could otherwise
/// succeed.
#[test]
fn garbage_and_unknown_events_around_a_valid_turn_still_succeed() {
    let mut r = router();
    let mut unparsed = 0;
    let mut forwarded = Vec::new();

    for line in lines(NOISY) {
        match r.on_line(line) {
            Some(StreamEvent::Unparsed(_)) => unparsed += 1,
            Some(StreamEvent::Other { kind, .. }) if kind != "turn.failed" => {
                forwarded.push(kind);
            }
            _ => {}
        }
    }

    assert_eq!(unparsed, 1, "the banner line is the only unparsable one");
    assert!(
        forwarded.contains(&"reasoning".to_string())
            && forwarded.contains(&"future.event".to_string()),
        "drift must be forwarded, saw {forwarded:?}"
    );

    let outcome = r.outcome(Some(0), "exit 0", "").unwrap();
    assert_eq!(outcome.text, "OK");
    assert_eq!(outcome.usage.input_tokens, 13_768);
}

// ---------------------------------------------------------------------------
// Outcome (DESIGN §6 rule 7)
// ---------------------------------------------------------------------------

#[test]
fn turn_failed_is_a_backend_error() {
    let mut r = router();
    r.on_line(recorded("thread.started"));
    let line = r#"{"type":"turn.failed","error":{"message":"stream error: connection reset"}}"#;
    r.on_line(line);

    let err = r.outcome(Some(0), "exit 0", "").unwrap_err();
    assert_eq!(err.exit_code(), 1);
    match err {
        PieError::Backend {
            backend,
            status,
            detail,
        } => {
            assert_eq!(backend, "codex");
            assert_eq!(status, "failed");
            assert!(detail.contains("connection reset"), "got {detail}");
        }
        other => panic!("turn.failed must be a Backend error, got {other:?}"),
    }
}

#[test]
fn an_error_event_is_a_backend_error() {
    let mut r = router();
    r.on_line(r#"{"type":"error","message":"model is unavailable for this plan"}"#);

    let err = r.outcome(Some(0), "exit 0", "").unwrap_err();
    match err {
        PieError::Backend { status, detail, .. } => {
            assert_eq!(status, "error");
            assert!(detail.contains("model is unavailable"));
        }
        other => panic!("an error event must be a Backend error, got {other:?}"),
    }
}

#[test]
fn a_non_zero_exit_without_a_terminal_event_reports_the_stderr_tail() {
    let mut r = router();
    r.on_line(recorded("thread.started"));

    let err = r
        .outcome(Some(2), "exit 2", "codex: could not reach the model\n")
        .unwrap_err();
    assert_eq!(err.exit_code(), 1);
    match err {
        PieError::Backend { status, detail, .. } => {
            assert_eq!(status, "exit 2");
            assert!(detail.contains("could not reach the model"), "got {detail}");
        }
        other => panic!("a non-zero exit must be a Backend error, got {other:?}"),
    }
}

/// Success is decided by the terminal event, not by the exit status.
#[test]
fn a_healthy_terminal_event_wins_over_a_noisy_stderr_and_a_bad_exit() {
    let mut r = router();
    for line in lines(TURN) {
        r.on_line(line);
    }

    let outcome = r
        .outcome(
            Some(7),
            "exit 7",
            "warning: retrying the websocket\nwarning: 503\n",
        )
        .unwrap();
    assert_eq!(outcome.text, "OK");
    assert_eq!(outcome.usage.output_tokens, 5);
}

/// A stream that contradicts itself must not discard a delivered answer.
///
/// This test previously asserted the turn *fails* when a `turn.failed` follows a
/// healthy `turn.completed`. The QC sweep argued the opposite, and for a
/// delegation tool the argument is stronger: codex emits `turn.failed` *instead
/// of* `turn.completed` when a turn fails, so both appearing means the stream is
/// contradictory, with a plausible cause being a close after delivery. Discarding
/// a complete answer over a trailing noise event is worse than returning it and
/// noting the anomaly, which the router now does with a warning.
#[test]
fn a_failure_after_the_terminal_event_does_not_erase_the_answer() {
    let mut r = router();
    for line in lines(TURN) {
        r.on_line(line);
    }
    r.on_line(r#"{"type":"turn.failed","error":{"message":"aborted after completion"}}"#);

    let outcome = r
        .outcome(Some(0), "exit 0", "")
        .expect("a trailing failure must not discard a delivered answer");
    assert_eq!(outcome.text, "OK");
    assert_eq!(outcome.status, "completed");
}

#[test]
fn exit_zero_without_a_terminal_event_is_a_protocol_error() {
    let mut r = router();
    r.on_line(recorded("thread.started"));
    r.on_line(recorded("agent_message"));

    let err = r.outcome(Some(0), "exit 0", "").unwrap_err();
    assert_eq!(err.exit_code(), 4, "a stream that breaks its own contract");
    match err {
        PieError::Protocol { backend, detail } => {
            assert_eq!(backend, "codex");
            assert!(detail.contains("turn.completed"), "got {detail}");
        }
        other => panic!("expected a Protocol error, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Secret hygiene (I3)
// ---------------------------------------------------------------------------

/// The token must never reach an error message, whatever the path.
#[test]
fn a_token_shaped_stderr_never_reaches_the_error_message() {
    let token = token_shaped();
    let prefixed = format!("sk-{}", "b".repeat(48));
    let stderr = format!("codex: auth failed: token={token} Authorization: Bearer {prefixed}\n");

    let mut r = router();
    r.on_line(recorded("thread.started"));
    let err = r.outcome(Some(1), "exit 1", &stderr).unwrap_err();

    let rendered = err.to_string();
    assert!(
        !rendered.contains(&token),
        "the stderr tail leaked: {rendered}"
    );
    assert!(
        !rendered.contains(&prefixed),
        "an sk- prefixed secret leaked: {rendered}"
    );
    assert!(
        rendered.contains("«redacted»"),
        "nothing was redacted: {rendered}"
    );
}

#[test]
fn a_token_shaped_failure_event_never_reaches_the_error_message() {
    let token = token_shaped();
    let line =
        format!(r#"{{"type":"turn.failed","error":{{"message":"stream error: Bearer {token}"}}}}"#);

    let mut r = router();
    r.on_line(&line);
    let err = r.outcome(Some(0), "exit 0", "").unwrap_err();

    let rendered = err.to_string();
    assert!(
        !rendered.contains(&token),
        "the event leaked a token: {rendered}"
    );
    assert!(rendered.contains("«redacted»"), "got {rendered}");
}

/// The quota probe's failure path must name the status and nothing else.
#[cfg(feature = "http")]
#[test]
fn http_failures_name_only_the_status_and_a_redacted_body() {
    let token = token_shaped();
    let body = format!(r#"{{"error":"upstream said Bearer {token}"}}"#);

    let server_error = describe_http_failure(500, &body);
    assert!(!server_error.to_string().contains(&token));
    assert!(server_error.to_string().contains("500"));
    assert_eq!(server_error.exit_code(), 1);
    assert!(matches!(server_error, PieError::Backend { .. }));

    let rejected = describe_http_failure(401, "unauthorized");
    assert_eq!(
        rejected.exit_code(),
        3,
        "a rejected credential is an auth error"
    );
    assert!(matches!(rejected, PieError::AuthRequired { .. }));
    assert!(rejected.to_string().contains("401"));
}

// ---------------------------------------------------------------------------
// Auth gate (I9)
// ---------------------------------------------------------------------------

/// The value actually stored by codex on this machine, read from
/// `~/.codex/auth.json` during M2 (`"auth_mode": "chatgpt"`). Only the mode
/// string is asserted — never a token.
#[test]
fn the_auth_mode_observed_on_this_machine_is_a_subscription() {
    let classified = classify_codex_mode("chatgpt");
    assert!(
        classified.is_subscription(),
        "auth_mode=chatgpt must classify as a subscription, got {classified:?}"
    );
}

#[tokio::test]
async fn an_api_key_only_credential_is_refused_with_no_subscription() {
    let dir = tempfile::tempdir().unwrap();
    let _home = HomeGuard::new(dir.path());

    let codex_dir = dir.path().join(".codex");
    std::fs::create_dir_all(&codex_dir).unwrap();
    std::fs::write(
        codex_dir.join("auth.json"),
        r#"{"auth_mode":"apikey","tokens":{"access_token":"stub"}}"#,
    )
    .unwrap();

    // A binary that cannot resolve, so if the gate ever regressed the test would
    // fail as BinaryMissing instead of spawning a real codex.
    let mut cfg = Config::default();
    cfg.codex.bin = "definitely-not-a-real-codex-binary-xyz".into();
    cfg.codex.use_node_shim = false;

    let mut sink = |_event: StreamEvent| {};
    let err = Codex
        .stream(&cfg, &ChatRequest::new("hi"), &mut sink)
        .await
        .unwrap_err();

    assert!(
        matches!(err, PieError::NoSubscription { .. }),
        "an API-key-only store must be refused, got {err:?}"
    );
    assert_eq!(err.exit_code(), 3);
}

// ---------------------------------------------------------------------------
// Model catalog
// ---------------------------------------------------------------------------

#[test]
fn the_catalog_maps_slugs_efforts_and_the_default_effort() {
    let value: serde_json::Value = serde_json::from_str(MODELS).unwrap();
    let models = shape_models(&value);
    assert_eq!(models.len(), 3, "the slug-less entry is dropped");

    let luna = models.iter().find(|m| m.id == "gpt-6-luna").unwrap();
    assert_eq!(luna.display, "GPT-6-Luna");
    assert_eq!(luna.efforts, ["low", "medium", "high", "xhigh", "max"]);
    assert_eq!(luna.default_effort.as_deref(), Some("medium"));

    let dream = models.iter().find(|m| m.id == "memory4-dream").unwrap();
    assert_eq!(
        dream.efforts.last().map(String::as_str),
        Some("ultra"),
        "every reported effort is kept, in order"
    );
    assert_eq!(dream.default_effort.as_deref(), Some("low"));

    let gpt55 = models.iter().find(|m| m.id == "gpt-5.5").unwrap();
    assert_eq!(
        gpt55.efforts.len(),
        4,
        "per-model effort sets are not merged"
    );
}

#[test]
fn a_catalog_with_no_usable_models_is_a_miss_not_an_empty_list() {
    let value: serde_json::Value = serde_json::from_str(r#"{"models":[]}"#).unwrap();
    assert!(shape_models(&value).is_empty());

    let broken: serde_json::Value =
        serde_json::from_str(r#"{"models":[{"display_name":"x"}]}"#).unwrap();
    assert!(shape_models(&broken).is_empty());
}

#[test]
fn the_catalog_falls_back_when_codex_has_written_no_cache() {
    let dir = tempfile::tempdir().unwrap();
    let _home = HomeGuard::new(dir.path());

    assert!(!dir.path().join(".codex").join("models_cache.json").exists());
    let models = model_catalog(&Config::default());

    assert_eq!(models.len(), fallback_models().len());
    assert!(models.iter().any(|m| m.id == "gpt-6-luna"));
    assert!(models.iter().all(|m| !m.efforts.is_empty()));
    assert!(
        !pie::paths::codex_model_cache().exists(),
        "the fallback is deliberately not cached, so a later real cache is picked up at once"
    );
}

#[test]
fn codex_own_cache_is_shaped_and_cached_in_pie_home() {
    let dir = tempfile::tempdir().unwrap();
    let _home = HomeGuard::new(dir.path());
    write_codex_cache(dir.path());

    let models = model_catalog(&Config::default());
    assert_eq!(models.len(), 3);
    assert!(models.iter().any(|m| m.id == "gpt-6-luna"));

    let cache = pie::paths::codex_model_cache();
    assert!(
        cache.is_file(),
        "the shaped catalog must be cached at {cache:?}"
    );
    let text = std::fs::read_to_string(&cache).unwrap();
    assert!(text.contains("gpt-6-luna"));

    // A corrupt codex cache is a miss, not an error.
    std::fs::write(
        dir.path().join(".codex").join("models_cache.json"),
        "{ not json",
    )
    .unwrap();
    std::fs::remove_file(&cache).unwrap();
    assert_eq!(
        model_catalog(&Config::default()).len(),
        fallback_models().len()
    );
}

/// A fresh pie cache must short-circuit reading codex's own state — that is what
/// makes `pie models` cheap, and it is asserted by pointing codex's store at
/// content the answer must not come from.
#[test]
fn a_fresh_pie_cache_is_served_without_reading_codex_state() {
    let dir = tempfile::tempdir().unwrap();
    let _home = HomeGuard::new(dir.path());

    let sentinel = vec![Model {
        id: "sentinel-model".to_string(),
        display: "Sentinel".to_string(),
        efforts: vec!["low".to_string()],
        default_effort: Some("low".to_string()),
    }];
    pie::cache::write_now(&pie::paths::codex_model_cache(), &sentinel).unwrap();
    write_codex_cache(dir.path());

    assert_eq!(
        model_catalog(&Config::default()),
        sentinel,
        "a fresh cache must win over codex's own file"
    );
}

#[test]
fn a_stale_pie_cache_is_refreshed_from_codex_state() {
    let dir = tempfile::tempdir().unwrap();
    let _home = HomeGuard::new(dir.path());

    let stale = vec![Model::plain("stale-model", "Stale")];
    let entry = pie::cache::CacheEntry::new(stale, 1);
    pie::cache::write(&pie::paths::codex_model_cache(), &entry).unwrap();
    write_codex_cache(dir.path());

    let models = model_catalog(&Config::default());
    assert!(models.iter().any(|m| m.id == "gpt-6-luna"));
    assert!(!models.iter().any(|m| m.id == "stale-model"));
}

// ---------------------------------------------------------------------------
// Quota probe
// ---------------------------------------------------------------------------

#[test]
fn the_usage_endpoint_is_the_documented_one() {
    assert_eq!(USAGE_URL, "https://chatgpt.com/backend-api/wham/usage");
}

#[cfg(feature = "http")]
#[test]
fn quota_windows_map_to_buckets() {
    let value: serde_json::Value = serde_json::from_str(
        r#"{"rate_limit":{"allowed":true,"limit_reached":false,
            "primary_window":{"used_percent":12,"limit_window_seconds":18000,"reset_at":1794102772},
            "secondary_window":{"used_percent":40,"limit_window_seconds":604800}},
            "credits":{"has_credits":false,"unlimited":false},
            "rate_limit_reset_credits":{"available_count":2}}"#,
    )
    .unwrap();

    let buckets = buckets_from_usage(&value);
    assert_eq!(buckets.len(), 2);

    assert_eq!(buckets[0].label, "5h");
    assert!((buckets[0].remaining_fraction.unwrap() - 0.88).abs() < 1e-9);
    assert_eq!(
        buckets[0].reset_at.as_deref(),
        Some("2026-11-08T01:52:52Z"),
        "reset_at is epoch seconds"
    );
    assert_eq!(buckets[0].used, None, "the endpoint reports percentages");
    assert_eq!(buckets[0].limit, None);

    assert_eq!(buckets[1].label, "weekly");
    assert!((buckets[1].remaining_fraction.unwrap() - 0.60).abs() < 1e-9);
    assert_eq!(buckets[1].reset_at, None);

    let notes = quota_notes(&value);
    assert!(
        notes.iter().any(|n| n.contains("no usage credits")),
        "{notes:?}"
    );
    assert!(
        notes.iter().any(|n| n.contains("reset credit")),
        "{notes:?}"
    );
}

/// The response actually observed on this machine: one 30-day window and a null
/// secondary window. The label follows the real duration — a plan with a 5h and
/// a weekly window gets `5h` and `weekly`, this one does not get mislabelled.
#[cfg(feature = "http")]
#[test]
fn a_window_is_labelled_by_its_real_duration() {
    let value: serde_json::Value = serde_json::from_str(
        r#"{"rate_limit":{"allowed":true,"limit_reached":false,
            "primary_window":{"used_percent":12,"limit_window_seconds":2592000,
                              "reset_after_seconds":2298291,"reset_at":1794102772},
            "secondary_window":null}}"#,
    )
    .unwrap();

    let buckets = buckets_from_usage(&value);
    assert_eq!(
        buckets.len(),
        1,
        "a null window is absent, not an empty bucket"
    );
    assert_eq!(buckets[0].label, "30d");

    // Codex's own rollout records the same window as `window_minutes`.
    let rollout: serde_json::Value = serde_json::from_str(
        r#"{"rate_limit":{"primary":{"used_percent":12.0,"window_minutes":43200,"resets_at":1794102772}}}"#,
    )
    .unwrap();
    assert!(
        buckets_from_usage(&rollout).is_empty(),
        "only the endpoint's own spelling is mapped"
    );
}

#[cfg(feature = "http")]
#[test]
fn an_empty_or_unknown_payload_yields_no_buckets_and_no_notes() {
    for body in [
        "{}",
        r#"{"rate_limit":null}"#,
        r#"{"rate_limit":{"primary_window":null}}"#,
    ] {
        let value: serde_json::Value = serde_json::from_str(body).unwrap();
        assert!(buckets_from_usage(&value).is_empty(), "{body}");
        assert!(quota_notes(&value).is_empty(), "{body}");
    }
}

#[cfg(feature = "http")]
#[test]
fn epoch_seconds_render_as_utc_timestamps() {
    assert_eq!(iso8601_utc(0), "1970-01-01T00:00:00Z");
    assert_eq!(iso8601_utc(604_800), "1970-01-08T00:00:00Z");
    assert_eq!(iso8601_utc(1_700_000_000), "2023-11-14T22:13:20Z");
    assert_eq!(iso8601_utc(1_794_102_772), "2026-11-08T01:52:52Z");
}

/// The private endpoint is opt-in, and `off` — **the default** — must be
/// provably network-free: this test has no credential on disk at all, so any
/// path that read a token or sent a request would fail before returning.
///
/// The assertion on the *default* rather than on an explicitly-set `off` is the
/// point. A probe that defaults to on still calls a vendor endpoint nobody asked
/// for, and the switch being reachable proves nothing about what a fresh install
/// does (user decision, 2026-10-12).
#[tokio::test]
async fn the_default_probe_is_off_and_makes_no_credential_or_network_call() {
    let dir = tempfile::tempdir().unwrap();
    let _home = HomeGuard::new(dir.path());

    assert_eq!(
        CodexQuotaProbe::default(),
        CodexQuotaProbe::Off,
        "a fresh install must never call a private vendor endpoint"
    );
    assert!(
        !dir.path().join(".codex").join("auth.json").exists(),
        "no credential exists for this test"
    );
    assert!(
        require_subscription().is_err(),
        "with no credential the gate refuses, so the http path could not run"
    );

    // The default config, untouched. If the default ever flips back to `http`,
    // this stops being the disabled report and the assertions below fail.
    let cfg = Config::default();
    assert_eq!(cfg.codex.quota_probe, CodexQuotaProbe::Off);

    let report = Codex.usage(&cfg).await.unwrap();
    assert_eq!(report, quota_disabled_report());
    assert!(report.buckets.is_empty());
    assert_eq!(report.source, "disabled by config");
    assert!(
        report.notes.iter().any(|n| n.contains("off")),
        "{:?}",
        report.notes
    );

    // Neither read nor wrote anything: the whole state dir is still empty.
    let written: Vec<String> = std::fs::read_dir(dir.path())
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    assert!(
        written.is_empty(),
        "the disabled probe must not touch the credential store, the network, or disk: {written:?}"
    );
}

/// …and opting in still reaches the endpoint, so the switch is real rather than
/// a flag with no effect in one direction.
///
/// The credential is absent, so the *failure* is the evidence: with `http`,
/// `usage` gets as far as demanding a subscription and fails there. With `off`
/// it returns the disabled report without ever looking. That difference is what
/// `the_default_probe_is_off_and_makes_no_credential_or_network_call` cannot
/// show on its own, which is exactly why it is asserted separately.
#[cfg(feature = "http")]
#[tokio::test]
async fn opting_in_reaches_the_credential_step_instead_of_returning_disabled() {
    let dir = tempfile::tempdir().unwrap();
    let _home = HomeGuard::new(dir.path());

    let mut cfg = Config::default();
    cfg.codex.quota_probe = CodexQuotaProbe::Http;

    let error = Codex
        .usage(&cfg)
        .await
        .expect_err("no credential on disk, so the probe cannot succeed");
    assert!(
        !error.to_string().contains("disabled"),
        "with `http` the disabled report must not be what comes back: {error}"
    );
    assert_eq!(
        error.exit_code(),
        3,
        "a missing subscription is exit 3 (I9), which is how we know the probe got \
         past the switch and stopped at the auth gate: {error}"
    );
}
