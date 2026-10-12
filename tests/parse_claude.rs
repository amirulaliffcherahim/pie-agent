//! Offline tests for the claude backend's `stream-json` envelope (M9 stage A).
//!
//! # Where these lines come from, and what they are not
//!
//! **Every line below is constructed from `knowledge/claude.md`, not recorded.** That
//! document states its envelope is *expected* rather than captured — there is no Claude
//! subscription on this machine (checked 2026-10-12, `~/.claude` holds no credential),
//! so no live `claude -p … --output-format stream-json` turn could have produced a
//! fixture. A fixture written from a specification is **not** evidence the specification
//! is right, and `planning/ACCEPTANCE.md` §17 says so in the same words: these tests are
//! evidence about pie's protocol handling, never about claude.
//!
//! Nothing here spawns a process at all: `ClaudeRouter` is pure parsing, which is why
//! `EventRouter` is a trait. The milestone's verdict stays `offline-verified`.
//!
//! Covered, per `planning/ACCEPTANCE.md` §17 stage A: every event kind of the envelope,
//! a single assistant message carrying several blocks **in order**, a `tool_use` that
//! carries its id and nested input unchanged, a `tool_result` in a `user` message that is
//! `Other` and not a second `TextDelta`, the usage mapping by claude's own field names,
//! garbage + an unknown type + a truncated final line around a valid `result` (invariant
//! I5), a multi-byte character at an awkward offset, and the `-p` argv that must not
//! change.

use std::path::PathBuf;

use pie::config::Config;
use pie::error::PieError;
use pie::provider::claude::{
    build_args, build_loop_args, is_success_status, ClaudeRouter, SUCCESS_SUBTYPES, UNKNOWN_STATUS,
    UNNAMED_TOOL, UNTERMINATED_STATUS,
};
use pie::provider::driver::EventRouter;
use pie::provider::{ChatRequest, StreamEvent, Usage};

// -------------------------------------------------------- the envelope ------
//
// Constructed from `knowledge/claude.md` §"Output — stream-json". See the module note:
// none of these is a recording.

/// `{"type":"system","subtype":"init",…}` — the turn start.
///
/// Carries `session_id` (claude's conversation id), `cwd`, `tools`, and extra keys a
/// newer claude may add (`model`, `permissionMode`, `apiKeySource`), because a parser
/// that only tolerates the fields it was written against is the fragility `RISKS.md` §3
/// names.
const INIT_LINE: &str = r#"{"type":"system","subtype":"init","cwd":"/work/pie","session_id":"5f0d3c2b-9a71-4d68-8e11-1c2a7b6d9f04","tools":["Bash","Read","Edit"],"model":"claude-sonnet-4-6","permissionMode":"default","apiKeySource":"none","uuid":"e1c0"}"#;

/// **One assistant message, three blocks, mixed kinds — and the `tool_use` is in the
/// middle.**
///
/// The middle position is deliberate. A router that collected the blocks by type, or
/// that took only the first block, would still pass a one-block test; with the call
/// between two text blocks, both defects change the emitted order.
const ASSISTANT_MIXED_LINE: &str = r#"{"type":"assistant","session_id":"5f0d3c2b-9a71-4d68-8e11-1c2a7b6d9f04","message":{"role":"assistant","model":"claude-sonnet-4-6","content":[{"type":"text","text":"Let me look"},{"type":"tool_use","id":"toolu_01ABC4XyZ","name":"Read","input":{"file_path":"/work/pie/src/main.rs","limit":40,"options":{"encoding":"utf-8","follow":true}}},{"type":"text","text":" at the file."}]}}"#;

/// The same message with the text blocks removed, for the `tool_result` test.
const ASSISTANT_TOOL_ONLY_LINE: &str = r#"{"type":"assistant","session_id":"5f0d3c2b-9a71-4d68-8e11-1c2a7b6d9f04","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_01ABC4XyZ","name":"Read","input":{"file_path":"/work/pie/src/main.rs"}}]}}"#;

/// `{"type":"user","message":{"content":[{"type":"tool_result",…}]}}` — the CLI echoing
/// a tool result back. pie must forward this and must **not** read it as answer text.
const USER_TOOL_RESULT_LINE: &str = r#"{"type":"user","session_id":"5f0d3c2b-9a71-4d68-8e11-1c2a7b6d9f04","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_01ABC4XyZ","content":"fn main() { println!(\"hi\"); }","is_error":false}]}}"#;

/// `{"type":"result",…}` — the terminal event, with everything claude reports.
///
/// The usage block uses claude's own names, and carries `service_tier`/`server_tool_use`
/// as noise: a field we do not model must not disturb the ones we do.
const RESULT_LINE: &str = r#"{"type":"result","subtype":"success","is_error":false,"duration_ms":4210,"duration_api_ms":3900,"num_turns":3,"result":"The file is six lines long.","session_id":"5f0d3c2b-9a71-4d68-8e11-1c2a7b6d9f04","total_cost_usd":0.0,"usage":{"input_tokens":1200,"cache_creation_input_tokens":300,"cache_read_input_tokens":900,"output_tokens":64,"service_tier":"standard","server_tool_use":{"web_search_requests":0}}}"#;

// ------------------------------------------------------------- helpers -----

fn strings(args: &[std::ffi::OsString]) -> Vec<String> {
    args.iter()
        .map(|a| a.to_string_lossy().to_string())
        .collect()
}

fn kinds(events: &[StreamEvent]) -> Vec<&'static str> {
    events.iter().map(StreamEvent::kind).collect()
}

fn texts(events: &[StreamEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::TextDelta(text) => Some(text.clone()),
            _ => None,
        })
        .collect()
}

/// Feed lines through the router exactly as `driver::drive` does — one `on_line` per
/// stdout line — then drain whatever a multi-block message left queued.
///
/// The drain is not decoration: `EventRouter::on_line` returns at most one event, so a
/// multi-block message lags (see
/// `a_multi_block_message_lags_the_single_event_trait_and_the_drain_recovers_it`). A
/// consumer that never drains loses the tail, which for a `tool_use` is a hang.
fn replay(lines: &[&str]) -> (ClaudeRouter, Vec<StreamEvent>) {
    let mut router = ClaudeRouter::new();
    let mut events: Vec<StreamEvent> = Vec::new();
    for line in lines {
        if let Some(event) = router.on_line(line) {
            events.push(event);
        }
    }
    events.extend(router.drain_pending());
    (router, events)
}

/// A request in a fixed working directory, so the argv assertions compare bytes and not
/// this machine's `cwd`.
fn request(prompt: &str) -> ChatRequest {
    let mut req = ChatRequest::new(prompt);
    req.cwd = PathBuf::from("C:/work");
    req
}

// ------------------------------------------------------- every event kind --

#[test]
fn the_init_line_becomes_an_init_event() {
    let (_, events) = replay(&[INIT_LINE]);

    match &events[0] {
        StreamEvent::Init {
            conversation_id,
            cwd,
            tools,
        } => {
            assert_eq!(
                conversation_id.as_deref(),
                Some("5f0d3c2b-9a71-4d68-8e11-1c2a7b6d9f04"),
                "claude carries the conversation id as `session_id`"
            );
            assert_eq!(cwd.as_deref(), Some(std::path::Path::new("/work/pie")));
            assert_eq!(
                tools,
                &vec!["Bash".to_string(), "Read".to_string(), "Edit".to_string()]
            );
        }
        other => panic!("wrong first event: {other:?}"),
    }
    assert_eq!(kinds(&events), vec!["init"], "one line, one event");
}

#[test]
fn a_system_line_with_another_subtype_is_forwarded_not_taken_for_a_turn_start() {
    // `system` is not only `init`: claude also reports `compact_boundary`, hook
    // notifications, and subtypes a newer version adds. Inventing an `Init` (with a
    // conversation id and a tool list) for one of those would fabricate a turn start.
    let compact = r#"{"type":"system","subtype":"compact_boundary","session_id":"5f0d","compact_metadata":{"trigger":"manual"}}"#;
    let (_, events) = replay(&[compact]);

    match &events[0] {
        StreamEvent::Other { kind, raw } => {
            assert_eq!(kind, "system");
            assert_eq!(raw["subtype"], "compact_boundary");
        }
        other => panic!("a `system` line that is not `init` must not become Init: {other:?}"),
    }
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, StreamEvent::Init { .. })),
        "only `subtype: init` starts a turn"
    );
}

#[test]
fn every_block_of_one_assistant_message_is_emitted_in_order() {
    let (router, events) = replay(&[ASSISTANT_MIXED_LINE]);

    // The requirement, exactly: the blocks in the CLI's order, not grouped by kind.
    // A router that took only the first text block would emit `["text"]`; one that
    // collected by type would emit `["text", "text", "tool_use"]`.
    assert_eq!(kinds(&events), vec!["text", "tool_use", "text"]);
    assert_eq!(texts(&events), vec!["Let me look", " at the file."]);
    assert_eq!(
        router.text(),
        "Let me look at the file.",
        "the accumulated answer is every text block, in order"
    );
}

#[test]
fn a_tool_use_carries_its_id_and_nested_input_through_unchanged() {
    let (_, events) = replay(&[ASSISTANT_MIXED_LINE]);

    let (id, name, input) = events
        .iter()
        .find_map(|event| match event {
            StreamEvent::ToolUse { id, name, input } => {
                Some((id.clone(), name.clone(), input.clone()))
            }
            _ => None,
        })
        .expect("the message asks pie to run a tool, so it must surface as ToolUse");

    // The id is what the matching `tool_result` must echo, and the input is what a
    // runner needs — neither may be reworded on the way through.
    assert_eq!(id, "toolu_01ABC4XyZ");
    assert_eq!(name, "Read");
    assert_eq!(
        input,
        serde_json::json!({
            "file_path": "/work/pie/src/main.rs",
            "limit": 40,
            "options": { "encoding": "utf-8", "follow": true }
        }),
        "nested JSON must survive verbatim, not be flattened or re-quoted"
    );
}

#[test]
fn a_tool_use_without_an_id_still_surfaces_rather_than_being_dropped() {
    // A call pie cannot name is a call it cannot answer. Dropping it turns that into a
    // hang — the model waits for a `tool_result` nobody will ever send — while surfacing
    // it fails loudly in the loop. Visible beats silent.
    let line =
        r#"{"type":"assistant","message":{"content":[{"type":"tool_use","input":{"cmd":"ls"}}]}}"#;
    let (_, events) = replay(&[line]);

    match &events[0] {
        StreamEvent::ToolUse { id, name, input } => {
            assert_eq!(id, "", "there was no id to carry, and none is invented");
            assert_eq!(name, UNNAMED_TOOL);
            assert_eq!(input, &serde_json::json!({"cmd": "ls"}));
        }
        other => panic!("a malformed call must stay visible, not become {other:?}"),
    }
}

#[test]
fn a_block_type_we_do_not_model_is_forwarded_not_swallowed() {
    // `thinking` is the common case in real claude streams. Forwarding it is what makes
    // a *future* `tool_use` spelling visible instead of vanishing.
    let line = r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"hmm"},{"type":"text","text":"done"}]}}"#;
    let (_, events) = replay(&[line]);

    assert_eq!(kinds(&events), vec!["other", "text"]);
    match &events[0] {
        StreamEvent::Other { kind, raw } => {
            assert_eq!(kind, "thinking");
            assert_eq!(raw["thinking"], "hmm");
        }
        other => panic!("wrong event: {other:?}"),
    }
}

#[test]
fn a_tool_result_in_a_user_message_becomes_other_and_is_never_echoed_as_text() {
    let (router, events) = replay(&[
        INIT_LINE,
        ASSISTANT_TOOL_ONLY_LINE,
        USER_TOOL_RESULT_LINE,
        RESULT_LINE,
    ]);

    let others: Vec<&str> = events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::Other { kind, .. } => Some(kind.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        others,
        vec!["user"],
        "the `user` event is forwarded once, as `Other`"
    );

    // The defect this pins: reading a `tool_result` as answer text would double-count
    // pie's own tool output in the transcript — the answer would contain the file it
    // read, as though the model had said it.
    assert!(
        !texts(&events).iter().any(|text| text.contains("fn main")),
        "a tool_result is not answer text: {:?}",
        texts(&events)
    );
    let outcome = router.outcome(Some(0), "exit 0", "").expect("success");
    assert_eq!(outcome.text, "The file is six lines long.");
}

#[test]
fn the_result_line_becomes_a_done_event_and_a_turn_outcome() {
    let (router, events) = replay(&[INIT_LINE, ASSISTANT_MIXED_LINE, RESULT_LINE]);
    assert!(router.saw_terminal());

    match events.last().expect("the fixture has events") {
        StreamEvent::Done { text, status } => {
            assert_eq!(text, "The file is six lines long.");
            // claude's own word, not agy's `SUCCESS`: the vendor status string travels
            // into the session log, and renaming it would hide which CLI produced it.
            assert_eq!(status, "success");
        }
        other => panic!("expected a terminal Done, got {other:?}"),
    }

    let outcome = router.outcome(Some(0), "exit 0", "").expect("success");
    assert_eq!(outcome.text, "The file is six lines long.");
    assert_eq!(outcome.status, "success");
    assert_eq!(
        outcome.conversation_id.as_deref(),
        Some("5f0d3c2b-9a71-4d68-8e11-1c2a7b6d9f04")
    );
    assert_eq!(
        outcome.usage,
        Usage {
            input_tokens: 1200,
            output_tokens: 64,
            cache_read_tokens: 900,
            cache_write_tokens: 300,
            ..Usage::default()
        }
    );
}

#[test]
fn usage_maps_by_claudes_field_names_and_never_by_another_backends() {
    // `cache_creation_input_tokens` is the **write** side and must land on
    // `cache_write_tokens`. The decoys are codex's and agy's names
    // (`cached_input_tokens`, `reasoning_output_tokens`, `thinking_tokens`): reading
    // another backend's field is how a mapping reports a number the vendor never sent.
    let line = r#"{"type":"result","subtype":"success","result":"ok","usage":{"input_tokens":10,"output_tokens":20,"cache_read_input_tokens":30,"cache_creation_input_tokens":40,"cached_input_tokens":999,"reasoning_output_tokens":999,"thinking_tokens":999}}"#;
    let (router, _) = replay(&[line]);

    let usage = router
        .outcome(Some(0), "exit 0", "")
        .expect("success")
        .usage;
    assert_eq!(usage.input_tokens, 10, "input_tokens → input_tokens");
    assert_eq!(usage.output_tokens, 20, "output_tokens → output_tokens");
    assert_eq!(
        usage.cache_read_tokens, 30,
        "cache_read_input_tokens → cache_read_tokens"
    );
    assert_eq!(
        usage.cache_write_tokens, 40,
        "cache_creation_input_tokens → cache_write_tokens (the write side)"
    );
    assert_eq!(usage.thinking_tokens, 0, "claude reports no thinking count");
    assert_eq!(
        usage.total_tokens, 0,
        "claude reports no total; `computed_total` sums the parts instead"
    );
}

#[test]
fn a_blank_line_and_a_json_line_without_a_type_produce_no_turn_state() {
    let mut router = ClaudeRouter::new();

    assert!(router.on_line("   ").is_none(), "a blank line is skipped");
    assert!(router.on_line("").is_none(), "an empty line is skipped too");

    let event = router
        .on_line(r#"{"phase":"warp","detail":{"n":1}}"#)
        .expect("parsed JSON is never silently dropped");
    match event {
        StreamEvent::Other { kind, raw } => {
            assert_eq!(kind, "unknown", "no `type` field at all");
            assert_eq!(raw["phase"], "warp");
        }
        other => panic!("wrong event: {other:?}"),
    }

    assert_eq!(router.text(), "");
    assert!(!router.saw_terminal());
}

// ------------------------------------------------------- tolerance (I5) ----

#[test]
fn garbage_an_unknown_type_and_a_truncated_final_line_around_a_valid_result_still_succeed() {
    // Invariant I5: a bad stdout line never aborts an otherwise healthy run. A CLI that
    // is killed mid-line, or one that grows an event kind we have never seen, must not
    // throw away the turn.
    let lines = [
        INIT_LINE,
        "Warning: claude is about to do something",
        r#"{"type":"mystery_future_event","payload":{"n":1}}"#,
        ASSISTANT_MIXED_LINE,
        RESULT_LINE,
        r#"{"type":"result","subtype":"succ"#,
    ];
    let (router, events) = replay(&lines);

    assert!(
        events.iter().any(
            |event| matches!(event, StreamEvent::Unparsed(raw) if raw.contains("Warning: claude"))
        ),
        "a non-JSON line is Unparsed, never fatal: {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, StreamEvent::Other { kind, .. } if kind == "mystery_future_event")),
        "an unknown event kind is forwarded: {events:?}"
    );
    assert!(events
        .iter()
        .any(|event| matches!(event, StreamEvent::TextDelta(text) if text == "Let me look")));

    // Exactly one terminal: the first one wins, and a later half-written line cannot
    // rewrite an answer that was already produced.
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, StreamEvent::Done { .. }))
            .count(),
        1,
        "the terminal event is emitted once: {events:?}"
    );

    let outcome = router.outcome(Some(0), "exit 0", "").expect("success");
    assert_eq!(outcome.text, "The file is six lines long.");
    assert_eq!(outcome.usage.input_tokens, 1200);
}

#[test]
fn a_result_that_is_not_the_first_terminal_does_not_rewrite_the_outcome() {
    let second =
        r#"{"type":"result","subtype":"error_during_execution","is_error":true,"result":"boom"}"#;
    let (router, events) = replay(&[RESULT_LINE, second]);

    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, StreamEvent::Done { .. }))
            .count(),
        2,
        "each terminal line surfaces, so a consumer can see the anomaly"
    );
    let outcome = router.outcome(Some(0), "exit 0", "").expect("success");
    assert_eq!(
        outcome.status, "success",
        "the first terminal decided the turn"
    );
    assert_eq!(outcome.text, "The file is six lines long.");
}

// --------------------------------------------------- multi-byte safety -----

#[test]
fn a_multi_byte_prompt_survives_argv_assembly_byte_for_byte() {
    // "A multi-byte character at an awkward offset in a prompt … does not panic": the
    // prompt reaches the child through argv, so the assertion is that the bytes are
    // still there unchanged — a `chars()`/`bytes()` mix-up here truncates a Chinese
    // prompt into mojibake or panics on a byte slice inside a character.
    let prompt = "🙂 日本語のテキスト — café";
    let mut req = request(prompt);
    req.persona = Some("# Persona\n语气: 简洁".into());

    let print = strings(&build_args(&Config::default(), &req).unwrap());
    assert_eq!(print[1], prompt, "the one-shot path hands over the bytes");
    assert!(
        print.contains(&"# Persona\n语气: 简洁".to_string()),
        "and the persona too: {print:?}"
    );

    let loop_args = strings(&build_loop_args(&Config::default(), &req).unwrap());
    assert!(
        loop_args.contains(&"# Persona\n语气: 简洁".to_string()),
        "the loop path takes the same persona: {loop_args:?}"
    );
}

#[test]
fn a_multi_byte_answer_and_error_survive_the_truncation_and_redaction_paths() {
    let answer = "🙂回答です";
    let text_line = format!(
        r#"{{"type":"assistant","message":{{"content":[{{"type":"text","text":"{answer}"}}]}}}}"#
    );
    let (router, events) = replay(&[&text_line, RESULT_LINE]);

    assert_eq!(texts(&events), vec![answer.to_string()]);
    assert_eq!(
        router.text(),
        answer,
        "no lossy slicing on a 4-byte character"
    );

    // A failure whose explanation is multi-byte and long enough to be truncated: this is
    // the path that historically sliced strings by byte offset (`error::truncate`,
    // `process::redact`). A byte-boundary panic here would kill pie on a Chinese error
    // message, which is exactly the input a vendor CLI does produce.
    let long = "错".repeat(700);
    let error_line = format!(
        r#"{{"type":"result","subtype":"error_during_execution","is_error":true,"result":"{long}"}}"#
    );
    let (router, _) = replay(&[&error_line]);

    let error = router
        .outcome(Some(1), "exit 1", "")
        .expect_err("an error terminal is a failure");
    let detail = match &error {
        PieError::Backend { detail, status, .. } => {
            assert_eq!(status, "error_during_execution");
            detail.clone()
        }
        other => panic!("expected PieError::Backend, got {other:?}"),
    };
    assert!(!detail.is_empty());
    assert!(
        detail.chars().count() <= 600 + "…[truncated]".chars().count(),
        "the detail is bounded: {} chars",
        detail.chars().count()
    );
    assert!(
        detail.starts_with('错'),
        "truncation starts at a character boundary, never mid-character"
    );
    assert_eq!(error.exit_code(), 1);
}

// ---------------------------------------------------------------- errors --

#[test]
fn an_error_terminal_is_a_backend_error_with_exit_code_one() {
    let line = r#"{"type":"result","subtype":"error_during_execution","is_error":true,"result":"API Error: 529 overloaded"}"#;
    let (router, _) = replay(&[line]);

    let error = router
        .outcome(Some(1), "exit 1", "")
        .expect_err("`is_error: true` is a failure");

    match &error {
        PieError::Backend {
            backend,
            status,
            detail,
        } => {
            assert_eq!(backend, "claude");
            assert_eq!(status, "error_during_execution");
            assert!(
                detail.contains("529 overloaded"),
                "the payload's own error text is the detail: {detail}"
            );
        }
        other => panic!("expected PieError::Backend, got {other:?}"),
    }
    assert_eq!(error.exit_code(), 1);
}

#[test]
fn an_unrecognised_subtype_fails_loudly_rather_than_claiming_success() {
    // `error_max_turns` is claude running out of its own iteration budget. A parser that
    // only looked for `is_error` would read it as a clean turn and log a truncated answer
    // as a complete one.
    let line = r#"{"type":"result","subtype":"error_max_turns","result":"I was still working"}"#;
    let (router, _) = replay(&[line]);

    match router.outcome(Some(0), "exit 0", "") {
        Err(PieError::Backend { status, .. }) => assert_eq!(status, "error_max_turns"),
        other => panic!("a new subtype must be visible, not silently success: {other:?}"),
    }
    assert!(
        !is_success_status("error_max_turns"),
        "only claude's own success word counts"
    );
    assert_eq!(SUCCESS_SUBTYPES, ["success"]);
}

#[test]
fn a_result_without_a_subtype_still_succeeds_on_a_zero_exit_and_says_the_field_was_absent() {
    let line = r#"{"type":"result","result":"OK","usage":{"output_tokens":5}}"#;
    let (router, _) = replay(&[line]);

    let outcome = router.outcome(Some(0), "exit 0", "").expect("success");
    assert_eq!(outcome.text, "OK");
    assert_eq!(
        outcome.status, UNKNOWN_STATUS,
        "the missing field stays visible"
    );

    // …but the same payload with a failing exit code is not a success.
    assert!(router.outcome(Some(2), "exit 2", "").is_err());
}

#[test]
fn a_result_that_is_an_error_without_a_subtype_is_still_a_failure() {
    let line = r#"{"type":"result","is_error":true,"result":"no subtype, but it failed"}"#;
    let (router, _) = replay(&[line]);

    match router.outcome(Some(0), "exit 0", "") {
        Err(PieError::Backend { status, detail, .. }) => {
            assert_eq!(status, "ERROR");
            assert!(detail.contains("no subtype"), "detail: {detail}");
        }
        other => panic!("`is_error` alone must fail the turn: {other:?}"),
    }
}

// ------------------------------------------------------- truncated stream --

#[test]
fn a_truncated_stream_with_a_non_zero_exit_is_a_protocol_error() {
    let (router, events) = replay(&[INIT_LINE, ASSISTANT_MIXED_LINE]);
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, StreamEvent::Done { .. })),
        "this stream has no terminal event"
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

#[test]
fn a_truncated_stream_with_exit_zero_and_text_is_a_success_that_says_it_was_truncated() {
    let (router, _) = replay(&[INIT_LINE, ASSISTANT_MIXED_LINE]);

    let outcome = router
        .outcome(Some(0), "exit 0", "")
        .expect("partial answer");
    assert_eq!(outcome.text, "Let me look at the file.");
    assert_eq!(
        outcome.status, UNTERMINATED_STATUS,
        "truncation is visible in the session log instead of being laundered into success"
    );
}

#[test]
fn a_truncated_stream_with_no_text_at_all_is_a_protocol_error() {
    let mut router = ClaudeRouter::new();
    assert!(router.on_line(INIT_LINE).is_some());

    let error = router
        .outcome(Some(0), "exit 0", "")
        .expect_err("no answer");
    assert!(matches!(error, PieError::Protocol { .. }), "got {error:?}");
    assert_eq!(error.exit_code(), 4);
}

#[test]
fn a_child_killed_by_a_signal_is_treated_as_a_failure() {
    let (router, _) = replay(&[INIT_LINE, ASSISTANT_MIXED_LINE]);

    let error = router
        .outcome(None, "signal 9", "")
        .expect_err("a killed child produced no terminal event");
    assert!(matches!(error, PieError::Protocol { .. }), "got {error:?}");
}

// ------------------------------------------------- one line, many events --

#[test]
fn a_multi_block_message_lags_the_single_event_trait_and_the_drain_recovers_it() {
    // `EventRouter::on_line` returns **one** event, and a claude assistant message can
    // carry several blocks. This test states the consequence instead of letting a
    // consumer discover it: the trait surface returns the first event and queues the
    // rest, and only a drain (or `on_line_all`) gets them back.
    //
    // The loop must therefore not use `on_line` alone — a `tool_use` left in this queue
    // is a turn that never finishes.
    let mut router = ClaudeRouter::new();

    let first = router
        .on_line(ASSISTANT_MIXED_LINE)
        .expect("the first block surfaces through the trait");
    assert_eq!(first.kind(), "text", "the leading text block comes first");

    let rest = router.drain_pending();
    assert_eq!(
        kinds(&rest),
        vec!["tool_use", "text"],
        "the remainder is held in order, not dropped and not reordered"
    );

    // The faithful surface, for contrast: every event, in order, in one call.
    let mut router = ClaudeRouter::new();
    assert_eq!(
        kinds(&router.on_line_all(ASSISTANT_MIXED_LINE)),
        vec!["text", "tool_use", "text"]
    );
    assert!(
        router.drain_pending().is_empty(),
        "on_line_all leaves nothing behind"
    );
}

#[test]
fn the_single_event_path_still_yields_every_event_of_a_long_stream_in_order() {
    // The control for the test above: over a whole turn, `on_line` + a final drain
    // produces exactly the same sequence as the faithful surface. If the queue were a
    // stack, or if a line's events were appended in the wrong place, this changes.
    let lines = [
        INIT_LINE,
        r#"{"type":"system","subtype":"compact_boundary","session_id":"s"}"#,
        ASSISTANT_MIXED_LINE,
        ASSISTANT_TOOL_ONLY_LINE,
        USER_TOOL_RESULT_LINE,
        RESULT_LINE,
    ];
    let (_, via_trait) = replay(&lines);

    let mut fresh = ClaudeRouter::new();
    let via_all: Vec<StreamEvent> = lines
        .iter()
        .flat_map(|line| fresh.on_line_all(line))
        .collect();

    assert_eq!(kinds(&via_trait), kinds(&via_all));
    assert_eq!(
        kinds(&via_trait),
        vec!["init", "other", "text", "tool_use", "text", "tool_use", "other", "done"]
    );
}

// --------------------------------------------------------------- argv ----

#[test]
fn the_print_mode_argv_is_unchanged_byte_for_byte() {
    // This is the check `planning/ACCEPTANCE.md` §17 asks for by name: the `-p` one-shot
    // path is frozen, because the loop's `--input-format` must not leak into the delegate
    // path — a stream-json input format there would make claude wait for a prompt on a
    // stdin nobody writes to.
    let mut req = request("hi");
    req.model = Some("claude-sonnet-4-6".into());
    req.persona = Some("# Persona\ntone: terse".into());

    assert_eq!(
        strings(&build_args(&Config::default(), &req).unwrap()),
        vec![
            "-p",
            "hi",
            "--output-format",
            "stream-json",
            "--dangerously-skip-permissions",
            "--add-dir",
            "C:/work",
            "--model",
            "claude-sonnet-4-6",
            "--append-system-prompt",
            "# Persona\ntone: terse",
        ],
        "the one-shot argv is a frozen contract (DESIGN.md §9)"
    );
}

#[test]
fn the_loop_argv_carries_both_format_flags_and_the_print_argv_carries_neither() {
    let mut req = request("hi");
    req.model = Some("claude-sonnet-4-6".into());

    let loop_args = strings(&build_loop_args(&Config::default(), &req).unwrap());
    let at = |flag: &str| loop_args.iter().position(|arg| arg == flag);

    let input_at = at("--input-format").expect("the loop needs stream-json input");
    assert_eq!(
        loop_args[input_at + 1],
        "stream-json",
        "SDK mode is what makes a `tool_result` injectable"
    );
    let output_at = at("--output-format").expect("the loop needs the streaming envelope");
    assert_eq!(loop_args[output_at + 1], "stream-json");

    // The pair is the check: a `build_args` that grew these flags would be a delegate
    // path that hangs, and a loop argv missing one of them cannot run a loop.
    let print = strings(&build_args(&Config::default(), &req).unwrap());
    assert!(
        !print.contains(&"--input-format".to_string()),
        "the loop's input format must not leak into the one-shot path: {print:?}"
    );
    assert_eq!(
        print.iter().filter(|arg| *arg == "--output-format").count(),
        1,
        "one output format, the documented one: {print:?}"
    );
}

#[test]
fn the_loop_argv_keeps_the_persona_mechanism_and_carries_no_prompt() {
    let mut req = request("таск");
    req.persona = Some("# Persona\ntone: terse".into());

    let loop_args = strings(&build_loop_args(&Config::default(), &req).unwrap());
    let at = loop_args
        .iter()
        .position(|arg| arg == "--append-system-prompt")
        .expect("claude has a system role, so the loop keeps the flag, not the fold");
    assert_eq!(loop_args[at + 1], "# Persona\ntone: terse");
    assert!(
        !loop_args.iter().any(|arg| arg.starts_with("таск")),
        "the prompt travels in the first stdin frame in SDK mode, not in argv: {loop_args:?}"
    );
    // `TASK_SEPARATOR` is what a backend with no system role folds in. Spelling it out
    // from the constant (rather than deleting the check) is what keeps this honest if the
    // separator is ever reworded.
    assert!(
        !loop_args
            .iter()
            .any(|arg| arg.contains(pie::profile::TASK_SEPARATOR)),
        "nothing is folded for a backend with a system prompt"
    );
    assert_eq!(
        loop_args[0], "-p",
        "print mode is still what makes it non-interactive"
    );
}

#[test]
fn the_tool_loop_capability_follows_the_turn_path_not_the_loop() {
    // `planning/ACCEPTANCE.md` §17 stage C, corrected during implementation. Stage C
    // originally said to flip this once the loop existed — and implementing that is what
    // showed the rule was wrong. A capability answers "can this backend accept an injected
    // `tool_result`?", so it must be true exactly when the backend's `stream` can answer a
    // call. claude's `stream` returns `Unsupported`, so it stays `false`, and the reason is
    // now the turn path rather than the loop.
    //
    // The pair is asserted because either half alone can be satisfied by a lie: the flag
    // must be `false` AND the missing piece must be the turn path.
    let claude = pie::provider::by_id("claude").expect("claude is in the registry");
    assert!(
        !claude.capabilities().tool_loop,
        "claude cannot run a turn yet, so it must not advertise a tool loop"
    );

    // …and a delegate-mode backend still refuses, which is the half that must never
    // change: a registry-wide flip would be a real regression, and a test that only
    // checked claude would not see it.
    for id in ["agy", "codex"] {
        let backend = pie::provider::by_id(id).expect("in the registry");
        assert!(
            !backend.capabilities().tool_loop,
            "`{id}` runs in delegate mode and cannot accept an injected tool_result \
             (RISKS.md section 1)"
        );
    }
}

/// **`drive` must drain what a router owes.** One `assistant` message can carry several
/// content blocks, and `EventRouter::on_line` can only return one of them — so a driver
/// that emitted the primary event and never asked for the rest would lose the tail
/// **silently**. The event most likely to be lost is a `tool_use`, and a lost `tool_use` is
/// a conversation that never finishes.
///
/// This is a *driver-level* test on purpose: the router's own tests exercise
/// `on_line`/`drain_pending` directly, which proves the router is faithful but says nothing
/// about whether anything ever calls the drain. This one spawns a real stand-in through
/// `drive` and asserts the sink saw all three blocks in order.
#[tokio::test]
async fn drive_drains_the_extra_blocks_a_single_assistant_line_carries() {
    use pie::provider::driver::{drive, TurnPlan};
    use std::time::Instant;

    let dir = tempfile::tempdir().expect("a scratch dir");
    let fixture = dir.path().join("mixed.jsonl");
    std::fs::write(&fixture, format!("{ASSISTANT_MIXED_LINE}\n")).expect("write the fixture");

    #[cfg(windows)]
    let script = dir.path().join("stand-in.bat");
    #[cfg(windows)]
    let body = format!("@echo off\r\ntype \"{}\"\r\n", fixture.display());
    #[cfg(not(windows))]
    let script = dir.path().join("stand-in.sh");
    #[cfg(not(windows))]
    let body = format!("#!/bin/sh\ncat \"{}\"\n", fixture.display());
    std::fs::write(&script, body).expect("write the stand-in");
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let plan = TurnPlan {
        program: script,
        args: Vec::new(),
        cwd: dir.path().to_path_buf(),
        scrub: Vec::new(),
        timeout: std::time::Duration::from_secs(30),
    };

    let mut router = ClaudeRouter::new();
    let mut seen: Vec<&'static str> = Vec::new();
    let _ = drive(
        plan,
        &mut router,
        &mut |event| seen.push(event.kind()),
        Instant::now(),
    )
    .await;

    assert_eq!(
        seen,
        vec!["text", "tool_use", "text"],
        "every block must reach the sink, in order - a driver that skipped the drain would \
         report `[\"text\"]` and lose the call"
    );
}
