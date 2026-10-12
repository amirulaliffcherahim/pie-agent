//! M9's `Transport` over HTTP (`ACCEPTANCE.md` §18 stage B).
//!
//! **Gate:** `cargo test --test http_transport`.
//!
//! Every test drives a **scripted client**. Nothing here reaches the network and nothing
//! needs an API key: the key is a literal this file plants in the environment for the
//! duration of one test, and the whole point of several tests is to prove that literal never
//! reaches any output pie produces.
//!
//! The four claims that carry the stage, each with the test that fails if it is false:
//! one full loop iteration is answered over a scripted stream
//! (`a_scripted_client_drives_one_full_loop_iteration_answering_a_tool_use`); a non-2xx is a
//! stated error naming the status and not the key
//! (`a_non_2xx_is_an_error_that_names_the_status_and_not_the_key`); a failed request is an
//! error rather than an empty stream
//! (`a_request_that_fails_is_an_error_rather_than_an_empty_stream`); and the key never reaches
//! any output (`the_key_never_reaches_an_error_a_log_line_or_a_formatted_value`).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::VecDeque;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde_json::{json, Value};

use pie::error::PieError;
use pie::provider::http::{
    HttpClient, HttpRequest, HttpResponse, HttpSpec, HttpTransport, ResponseStream,
};
use pie::provider::sse::cost_usd_micros;
use pie::provider::StreamEvent;
use pie::tool_loop::{run_loop, tool_result_frame, ToolCall, ToolResult, ToolRunner, Transport};

/// The key these tests plant.
///
/// Deliberately **not** credential-shaped: it is short (under the 32-character heuristic) and
/// carries no uppercase letter, so `tests/no_secrets.rs`'s scanner — which looks for long
/// opaque runs containing a digit, an uppercase and a lowercase letter — is not tripped by a
/// fixture. It is still distinctive enough that finding it anywhere is a leak.
const KEY: &str = "not-a-real-key-m10-stage-b";

/// `std::env` is process-global and the harness runs tests in parallel, so every mutation
/// takes this lock (the rule in `tests/README.md`). Each test also uses its **own** variable
/// name, so no test can be confused by another's value.
fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

// ---------------------------------------------------------------------------
// The scripted client
// ---------------------------------------------------------------------------

/// One scripted response.
#[derive(Debug, Clone)]
struct Script {
    status: u16,
    chunks: Vec<Vec<u8>>,
    /// Raised instead of the next chunk, so a broken read can be scripted.
    failure: Option<String>,
}

impl Script {
    /// A 200 carrying the whole body in one chunk.
    fn ok(body: &str) -> Self {
        Self {
            status: 200,
            chunks: vec![body.as_bytes().to_vec()],
            failure: None,
        }
    }

    /// The same, delivered in `size`-byte reads, as a socket does.
    fn chunked(body: &str, size: usize) -> Self {
        Self {
            status: 200,
            chunks: body
                .as_bytes()
                .chunks(size)
                .map(|chunk| chunk.to_vec())
                .collect(),
            failure: None,
        }
    }

    /// A failure response.
    fn status(status: u16, body: &str) -> Self {
        Self {
            status,
            chunks: vec![body.as_bytes().to_vec()],
            failure: None,
        }
    }

    /// A body whose second read fails, as a reset connection does.
    fn breaks_after(body: &str, failure: &str) -> Self {
        Self {
            status: 200,
            chunks: vec![body.as_bytes().to_vec()],
            failure: Some(failure.to_string()),
        }
    }
}

/// What the transport sent, captured so a test can assert on the wire shape.
///
/// The `Debug` is written by hand for the same reason `HttpRequest`'s is: a derived one would
/// print `Authorization: Bearer <key>`, and a panic message is output too.
#[derive(Clone)]
struct Recorded {
    url: String,
    body: Value,
    headers: Vec<(String, String)>,
}

impl fmt::Debug for Recorded {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names: Vec<&str> = self.headers.iter().map(|(name, _)| name.as_str()).collect();
        f.debug_struct("Recorded")
            .field("url", &self.url)
            .field("headers", &names)
            .finish_non_exhaustive()
    }
}

impl Recorded {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    fn messages(&self) -> Vec<Value> {
        self.body["messages"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    }

    fn roles(&self) -> Vec<String> {
        self.messages()
            .iter()
            .map(|message| message["role"].as_str().unwrap_or_default().to_string())
            .collect()
    }
}

/// A client that answers from a script and records what it was asked.
#[derive(Debug, Default)]
struct Fixture {
    scripts: Mutex<VecDeque<Script>>,
    requests: Mutex<Vec<Recorded>>,
    /// Returned instead of a response, to script a transport failure.
    failure: Mutex<Option<String>>,
}

impl Fixture {
    fn new(scripts: Vec<Script>) -> Arc<Self> {
        Arc::new(Self {
            scripts: Mutex::new(scripts.into()),
            requests: Mutex::new(Vec::new()),
            failure: Mutex::new(None),
        })
    }

    fn failing(reason: &str) -> Arc<Self> {
        let fixture = Self::new(Vec::new());
        *fixture.failure.lock().unwrap() = Some(reason.to_string());
        fixture
    }

    fn requests(&self) -> Vec<Recorded> {
        self.requests.lock().unwrap().clone()
    }
}

impl HttpClient for Fixture {
    fn post(&self, request: &HttpRequest) -> Result<HttpResponse, PieError> {
        self.requests.lock().unwrap().push(Recorded {
            url: request.url.clone(),
            body: request.body.clone(),
            headers: request.headers.clone(),
        });

        if let Some(reason) = self.failure.lock().unwrap().take() {
            return Err(PieError::NotReachable(reason));
        }

        let script = self
            .scripts
            .lock()
            .unwrap()
            .pop_front()
            .expect("the test scripted a response for every request it allows");
        Ok(HttpResponse {
            status: script.status,
            stream: Box::new(Chunks {
                chunks: script.chunks.into(),
                failure: script.failure,
            }),
        })
    }
}

/// The scripted body of a response.
#[derive(Debug)]
struct Chunks {
    chunks: VecDeque<Vec<u8>>,
    failure: Option<String>,
}

impl ResponseStream for Chunks {
    fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, PieError> {
        if let Some(chunk) = self.chunks.pop_front() {
            return Ok(Some(chunk));
        }
        match self.failure.take() {
            Some(reason) => Err(PieError::NotReachable(reason)),
            None => Ok(None),
        }
    }
}

// ---------------------------------------------------------------------------
// The scripted stream's vocabulary
// ---------------------------------------------------------------------------

/// One SSE frame from a JSON value, so a fixture cannot be mis-braced by hand.
fn frame(value: Value) -> String {
    format!("data: {value}\n\n")
}

/// A whole body: the frames, then the terminator.
fn body(frames: Vec<Value>) -> String {
    let mut out: String = frames.into_iter().map(frame).collect();
    out.push_str("data: [DONE]\n\n");
    out
}

/// A chunk that opens a tool call: the id and the name arrive here, the arguments in pieces.
fn tool_call_delta(index: u64, id: &str, name: &str, arguments: &str) -> Value {
    json!({
        "choices": [{
            "index": 0,
            "delta": { "tool_calls": [{
                "index": index,
                "id": id,
                "type": "function",
                "function": { "name": name, "arguments": arguments },
            }] },
        }],
    })
}

/// A chunk that continues a tool call and ends the choice.
fn tool_call_tail(index: u64, arguments: &str, usage_cost: Option<f64>) -> Value {
    let mut chunk = json!({
        "choices": [{
            "index": 0,
            "delta": { "tool_calls": [{
                "index": index,
                "function": { "arguments": arguments },
            }] },
            "finish_reason": "tool_calls",
        }],
    });

    if let Some(cost) = usage_cost {
        chunk["usage"] = json!({
            "prompt_tokens": 12,
            "completion_tokens": 3,
            "total_tokens": 15,
            "cost": cost,
        });
    }

    chunk
}

/// A chunk of answer text.
fn text_delta(text: &str) -> Value {
    json!({ "choices": [{ "index": 0, "delta": { "content": text } }] })
}

/// A chunk that ends the choice with `stop`.
fn stop() -> Value {
    json!({ "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }] })
}

/// The answer the second turn of a script gives.
fn second_turn(text: &str) -> String {
    body(vec![text_delta(text), stop()])
}

/// Answers every call with its own id, as `run_loop` requires.
#[derive(Debug)]
struct EchoRunner;

impl ToolRunner for EchoRunner {
    fn run(&self, call: &ToolCall) -> ToolResult {
        ToolResult {
            id: call.id.clone(),
            output: "file contents of a.txt".to_string(),
            is_error: false,
        }
    }
}

/// A spec pointed at a URL that cannot be reached, with the key variable named by the test.
fn spec(key_env: &'static str) -> HttpSpec {
    let mut spec = HttpSpec::new(
        "openrouter",
        "https://example.invalid/api/v1/chat/completions",
        "test-model",
        key_env,
        "read the file",
    );
    spec.headers
        .push(("x-title".to_string(), "pie".to_string()));
    spec
}

/// The fixture as the client the transport takes.
///
/// `HttpTransport` holds `Arc<dyn HttpClient>`, and a test needs the *concrete* `Arc<Fixture>`
/// to read back what was sent — so the coercion happens here rather than at every call site.
fn client(fixture: &Arc<Fixture>) -> Arc<dyn HttpClient> {
    fixture.clone()
}

/// Build a transport over a fixture, with the key present in the environment.
fn http_transport(fixture: Arc<Fixture>, key_env: &'static str) -> HttpTransport {
    std::env::set_var(key_env, KEY);
    HttpTransport::new(fixture, spec(key_env))
}

// ---------------------------------------------------------------------------
// One full loop iteration
// ---------------------------------------------------------------------------

#[test]
fn a_scripted_client_drives_one_full_loop_iteration_answering_a_tool_use() {
    let _env = env_lock();

    let fixture = Fixture::new(vec![
        // Turn one: a tool call whose arguments straddle two frames, plus accounting with a
        // real cost (this class of backend reports one, unlike the subscription CLIs).
        Script::chunked(
            &body(vec![
                tool_call_delta(0, "call_a", "read", "{\"path\":"),
                tool_call_tail(0, "\"a.txt\"}", Some(0.00005)),
            ]),
            7,
        ),
        Script::ok(&second_turn("all done")),
    ]);

    let mut transport = http_transport(Arc::clone(&fixture), "PIE_M10_KEY_T1");
    transport.start().expect("the turn opens");

    let mut seen: Vec<StreamEvent> = Vec::new();
    let outcome = run_loop(&mut transport, &EchoRunner, 4, &mut |event| {
        seen.push(event);
    })
    .expect("the loop finishes against a scripted client");

    assert_eq!(outcome.iterations, 1, "one call, one result");
    assert!(!outcome.capped);
    assert_eq!(outcome.text, "all done");

    // The call reached the loop with its arguments parsed, not as a fragment.
    let call = seen
        .iter()
        .find_map(|event| match event {
            StreamEvent::ToolUse { id, name, input } => {
                Some((id.clone(), name.clone(), input.clone()))
            }
            _ => None,
        })
        .expect("the stream asked for a tool");
    assert_eq!(call.0, "call_a");
    assert_eq!(call.2["path"], "a.txt");

    // The cost the endpoint reported reached the caller through the loop's own sink.
    assert_eq!(seen.iter().find_map(cost_usd_micros), Some(50));

    let requests = fixture.requests();
    assert_eq!(requests.len(), 2, "one POST per turn, not per event");

    // The statelessness, asserted on the wire: the second request carries the whole
    // conversation — the prompt, the assistant turn that asked for the call, and the answer.
    assert_eq!(requests[0].roles(), vec!["user"]);
    assert_eq!(
        requests[1].roles(),
        vec!["user", "assistant", "tool"],
        "{:?}",
        requests[1].messages()
    );

    let assistant = &requests[1].messages()[1];
    assert_eq!(assistant["tool_calls"][0]["id"], "call_a");
    assert_eq!(
        assistant["tool_calls"][0]["function"]["arguments"], "{\"path\":\"a.txt\"}",
        "the arguments go back to the wire as a string, which is what it accepts"
    );

    let answer = &requests[1].messages()[2];
    assert_eq!(answer["tool_call_id"], "call_a");
    assert_eq!(answer["content"], "file contents of a.txt");

    assert_eq!(requests[0].body["stream"], true);
    assert_eq!(requests[0].body["model"], "test-model");
    assert_eq!(requests[0].body["messages"][0]["content"], "read the file");

    // Positive control for the leak tests: the key really does reach the wire, so a scan
    // that finds nothing later is a scan of something that was there.
    assert_eq!(
        requests[0].header("authorization"),
        Some(format!("Bearer {KEY}").as_str())
    );
}

#[test]
fn two_parallel_tool_calls_are_answered_in_one_request() {
    let _env = env_lock();

    // One assistant message asking for two tools. The loop answers them one at a time, so a
    // POST per answered frame would send the conversation with the second call still
    // unanswered — and the endpoint would be the first thing to notice.
    let mut both = tool_call_delta(0, "call_a", "read", "{\"path\":\"a.txt\"}");
    both["choices"][0]["delta"]["tool_calls"]
        .as_array_mut()
        .unwrap()
        .push(json!({
            "index": 1,
            "id": "call_b",
            "type": "function",
            "function": { "name": "read", "arguments": "{\"path\":\"b.txt\"}" },
        }));
    both["choices"][0]["finish_reason"] = json!("tool_calls");

    let fixture = Fixture::new(vec![
        Script::ok(&body(vec![both])),
        Script::ok(&second_turn("done")),
    ]);

    let mut transport = http_transport(Arc::clone(&fixture), "PIE_M10_KEY_T2");
    transport.start().unwrap();

    let outcome = run_loop(&mut transport, &EchoRunner, 4, &mut |_| {}).unwrap();
    assert_eq!(outcome.iterations, 2);

    let requests = fixture.requests();
    assert_eq!(
        requests.len(),
        2,
        "one request per *turn*, with both results in the second: {}",
        requests.len()
    );

    let messages = requests[1].messages();
    let answered: Vec<&str> = messages
        .iter()
        .filter(|message| message["role"] == "tool")
        .map(|message| message["tool_call_id"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(answered, vec!["call_a", "call_b"]);

    let assistant_turns = messages
        .iter()
        .filter(|message| message["role"] == "assistant")
        .count();
    assert_eq!(assistant_turns, 1, "the asking turn is echoed exactly once");
}

#[test]
fn an_errored_tool_result_is_marked_in_the_content_because_the_wire_has_no_error_flag() {
    let _env = env_lock();

    let fixture = Fixture::new(vec![
        Script::ok(&body(vec![
            tool_call_delta(0, "call_a", "read", "{}"),
            stop(),
        ])),
        Script::ok(&second_turn("ok")),
    ]);
    let mut transport = http_transport(Arc::clone(&fixture), "PIE_M10_KEY_T3");
    transport.start().unwrap();
    assert!(matches!(
        transport.recv().unwrap(),
        Some(StreamEvent::ToolUse { .. })
    ));

    transport
        .send(tool_result_frame(&ToolResult {
            id: "call_a".to_string(),
            output: "no such file".to_string(),
            is_error: true,
        }))
        .unwrap();

    // The transport records the result at `send` and posts on a later `recv`, once the
    // events the previous response already delivered have been returned to the loop.
    for _ in 0..8 {
        if fixture.requests().len() == 2 {
            break;
        }
        match transport.recv().unwrap() {
            Some(_) => {}
            None => panic!("the stream ended before the answered call was posted"),
        }
    }
    assert_eq!(
        fixture.requests().len(),
        2,
        "the answered call is posted as a fresh request"
    );

    let requests = fixture.requests();
    let content = requests[1].messages()[2]["content"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        content.starts_with("[tool_error] "),
        "a failed tool the model cannot see reads as a successful empty one: {content}"
    );
    assert!(content.contains("no such file"), "{content}");
}

#[test]
fn the_persona_is_sent_as_a_system_message_rather_than_folded_into_the_prompt() {
    let _env = env_lock();

    let fixture = Fixture::new(vec![Script::ok(&second_turn("ok"))]);
    std::env::set_var("PIE_M10_KEY_T4", KEY);

    let mut with_persona = spec("PIE_M10_KEY_T4");
    with_persona.system = Some("PERSONA BYTES".to_string());
    let mut transport = HttpTransport::new(client(&fixture), with_persona);
    transport.start().unwrap();

    let messages = fixture.requests()[0].messages();
    assert_eq!(messages[0]["role"], "system");
    assert_eq!(messages[0]["content"], "PERSONA BYTES");
    assert_eq!(messages[1]["role"], "user");
    assert_eq!(
        messages[1]["content"], "read the file",
        "the prompt is not modified by the projection: the shape has a real system role, so \
         there is nothing to fold"
    );
}

// ---------------------------------------------------------------------------
// Failures
// ---------------------------------------------------------------------------

#[test]
fn a_non_2xx_is_an_error_that_names_the_status_and_not_the_key() {
    let _env = env_lock();

    let fixture = Fixture::new(vec![Script::status(
        401,
        "{\"error\":{\"message\":\"no auth credentials found\"}}",
    )]);
    let mut transport = http_transport(fixture, "PIE_M10_KEY_T5");

    let error = transport.start().unwrap_err();
    let rendered = format!("{error}");
    assert!(rendered.contains("401"), "{rendered}");
    assert!(rendered.contains("no auth credentials found"), "{rendered}");
    assert!(
        rendered.contains("PIE_M10_KEY_T5"),
        "names the variable: {rendered}"
    );
    assert!(
        !rendered.contains(KEY),
        "the key is in the error: {rendered}"
    );
    assert_eq!(
        error.exit_code(),
        3,
        "a rejected credential is an auth failure"
    );

    let fixture = Fixture::new(vec![Script::status(500, "{\"error\":\"boom\"}")]);
    let mut transport = http_transport(fixture, "PIE_M10_KEY_T6");
    let error = transport.start().unwrap_err();
    let rendered = format!("{error}");
    assert!(rendered.contains("HTTP 500"), "{rendered}");
    assert!(!rendered.contains(KEY), "{rendered}");
    assert!(matches!(error, PieError::Backend { .. }), "{error:?}");
}

#[test]
fn a_request_that_fails_is_an_error_rather_than_an_empty_stream() {
    let _env = env_lock();

    // A transport that never answers.
    let mut transport = http_transport(Fixture::failing("connection refused"), "PIE_M10_KEY_T7");
    let error = transport.start().unwrap_err();
    assert!(error.to_string().contains("connection refused"), "{error}");

    // A body whose read breaks half way through: the loop must see an error, not a turn
    // that produced no answer.
    let fixture = Fixture::new(vec![Script::breaks_after(
        &frame(text_delta("half")),
        "connection reset",
    )]);
    let mut transport = http_transport(fixture, "PIE_M10_KEY_T8");
    transport.start().unwrap();

    let mut seen = Vec::new();
    let error = run_loop(&mut transport, &EchoRunner, 4, &mut |event| {
        seen.push(event)
    })
    .expect_err("a reset connection is not a completed turn");
    assert!(error.to_string().contains("connection reset"), "{error}");
    assert!(
        seen.iter()
            .any(|event| matches!(event, StreamEvent::TextDelta(text) if text == "half")),
        "what arrived before the failure is still delivered: {seen:?}"
    );

    // A 200 with an empty body. This is the case the brief names: silence must not be
    // reported as an answer, and `Ok(None)` would be exactly that.
    let fixture = Fixture::new(vec![Script::ok("")]);
    let mut transport = http_transport(fixture, "PIE_M10_KEY_T9");
    transport.start().unwrap();

    let error = run_loop(&mut transport, &EchoRunner, 4, &mut |_| {})
        .expect_err("an empty body is not an empty answer");
    assert!(
        error.to_string().contains("ended with no `[DONE]`"),
        "{error}"
    );
}

#[test]
fn a_missing_key_is_a_stated_error_naming_the_variable_and_never_a_fallback() {
    let _env = env_lock();

    let fixture = Fixture::new(vec![Script::ok(&second_turn("ok"))]);
    let mut missing = spec("PIE_M10_KEY_MISSING");
    missing.prompt = "read the file".to_string();
    let mut transport = HttpTransport::new(client(&fixture), missing);

    let error = transport.start().unwrap_err();
    assert!(matches!(error, PieError::AuthRequired { .. }), "{error:?}");
    assert!(
        error.to_string().contains("PIE_M10_KEY_MISSING"),
        "the error names the variable to set: {error}"
    );
    assert_eq!(error.exit_code(), 3);
    assert!(
        fixture.requests().is_empty(),
        "a turn with no key sends nothing at all — it does not fall back to another backend"
    );
}

#[test]
fn the_key_is_read_at_call_time_rather_than_when_the_transport_is_built() {
    let _env = env_lock();
    let key_env = "PIE_M10_KEY_T10";
    std::env::remove_var(key_env);

    let fixture = Fixture::new(vec![
        Script::ok(&second_turn("ok")),
        Script::ok(&second_turn("ok")),
    ]);

    // Built with no key in the environment. Nothing is read here, which is what lets the
    // same transport start once the key appears — a key captured in the constructor would
    // already be absent and this turn could never run.
    let mut transport = HttpTransport::new(client(&fixture), spec(key_env));

    // A turn with no key at all is a stated error rather than a fallback to another backend.
    let error = transport.start().unwrap_err();
    assert!(matches!(error, PieError::AuthRequired { .. }), "{error:?}");
    assert!(
        error.to_string().contains(key_env),
        "the error names the variable to set: {error}"
    );

    std::env::set_var(key_env, KEY);
    let mut transport = HttpTransport::new(client(&fixture), spec(key_env));
    transport
        .start()
        .expect("the key is read when the request is made, not when the transport is built");
    assert_eq!(fixture.requests().len(), 1);

    std::env::remove_var(key_env);
}

// ---------------------------------------------------------------------------
// The protocol's own errors
// ---------------------------------------------------------------------------

#[test]
fn an_event_requested_before_the_turn_starts_is_a_stated_error() {
    let _env = env_lock();

    let fixture = Fixture::new(vec![Script::ok(&second_turn("ok"))]);
    let mut transport = http_transport(fixture, "PIE_M10_KEY_T11");

    let error = transport.recv().unwrap_err();
    assert!(error.to_string().contains("before any request"), "{error}");
    assert_eq!(error.exit_code(), 4);
}

#[test]
fn a_result_for_a_call_this_turn_never_made_is_refused_rather_than_sent() {
    let _env = env_lock();

    let fixture = Fixture::new(vec![Script::ok(&body(vec![
        tool_call_delta(0, "call_a", "read", "{}"),
        stop(),
    ]))]);
    let mut transport = http_transport(Arc::clone(&fixture), "PIE_M10_KEY_T12");
    transport.start().unwrap();
    assert!(matches!(
        transport.recv().unwrap(),
        Some(StreamEvent::ToolUse { .. })
    ));

    let error = transport
        .send(tool_result_frame(&ToolResult {
            id: "call_zzz".to_string(),
            output: "nope".to_string(),
            is_error: false,
        }))
        .unwrap_err();
    assert!(error.to_string().contains("call_zzz"), "{error}");
    assert_eq!(
        fixture.requests().len(),
        1,
        "the mismatched result is refused before it can reach the endpoint"
    );
}

#[test]
fn a_transport_failure_in_the_middle_of_a_body_does_not_end_the_turn_cleanly() {
    let _env = env_lock();

    let fixture = Fixture::new(vec![Script::breaks_after(
        &body(vec![tool_call_delta(0, "call_a", "read", "{}"), stop()]),
        "read timed out",
    )]);
    let mut transport = http_transport(fixture, "PIE_M10_KEY_T13");
    transport.start().unwrap();

    // The events the body did deliver are still returned, and the failure surfaces after
    // them rather than being swallowed by the end of the stream.
    let first = transport.recv().unwrap();
    assert!(
        matches!(first, Some(StreamEvent::ToolUse { .. })),
        "{first:?}"
    );

    let error = loop {
        match transport.recv() {
            Ok(Some(_)) => continue,
            Ok(None) => panic!("the turn ended cleanly although its body was cut"),
            Err(error) => break error,
        }
    };
    assert!(error.to_string().contains("read timed out"), "{error}");
}

// ---------------------------------------------------------------------------
// The key reaches no output at all
// ---------------------------------------------------------------------------

#[test]
fn the_key_never_reaches_an_error_a_log_line_or_a_formatted_value() {
    let _env = env_lock();

    let mut produced: Vec<(&str, String)> = Vec::new();

    // A rejected key.
    let fixture = Fixture::new(vec![Script::status(403, "{\"error\":\"forbidden\"}")]);
    let mut rejected = http_transport(fixture, "PIE_M10_KEY_T14");
    let error = rejected.start().unwrap_err();
    produced.push(("a 403 body", format!("{error}")));
    produced.push(("a 403 debug", format!("{error:?}")));

    // A good turn, so every value a successful turn produces is covered too.
    let fixture = Fixture::new(vec![
        Script::ok(&body(vec![
            tool_call_delta(0, "call_a", "read", "{}"),
            stop(),
        ])),
        Script::ok(&second_turn("done")),
    ]);
    std::env::set_var("PIE_M10_KEY_T15", KEY);
    let mut transport = HttpTransport::new(client(&fixture), spec("PIE_M10_KEY_T15"));
    let spec_debug = format!("{:?}", spec("PIE_M10_KEY_T15"));
    transport.start().unwrap();
    produced.push(("a spec", spec_debug));

    let mut seen = Vec::new();
    let outcome = run_loop(&mut transport, &EchoRunner, 4, &mut |event| {
        seen.push(event)
    })
    .unwrap();
    produced.push(("a transport", format!("{transport:?}")));
    produced.push(("the answer", outcome.text));
    for event in &seen {
        produced.push(("an event", format!("{event:?}")));
    }
    for request in fixture.requests() {
        produced.push(("a recorded request", format!("{request:?}")));
        produced.push(("a request body", request.body.to_string()));
        for (name, value) in &request.headers {
            // Every header except the one the key is legitimately in: the assertion is that
            // the key reaches no *output*, and the authorization header is the request.
            if name.eq_ignore_ascii_case("authorization") {
                continue;
            }
            produced.push(("a header", value.clone()));
        }
    }

    // The production `Debug` impls, which are the ones a future log line would call.
    let request = HttpRequest {
        url: "https://example.invalid".to_string(),
        body: json!({ "messages": [] }),
        headers: vec![("authorization".to_string(), format!("Bearer {KEY}"))],
    };
    produced.push(("HttpRequest::Debug", format!("{request:?}")));

    for (what, text) in &produced {
        assert!(
            !text.contains(KEY),
            "{what} carried the key:\n{text}\n\n- and the key is present in the request, so \
             this is a leak rather than a test that never planted one"
        );
    }

    // The control, again: the key *was* used, so a green scan above is a statement about a
    // value that reached the request.
    let used = fixture
        .requests()
        .iter()
        .any(|recorded| recorded.header("authorization") == Some(format!("Bearer {KEY}").as_str()));
    assert!(
        used,
        "no request carried the key, so nothing above was tested"
    );
}

#[test]
fn the_live_client_is_absent_without_the_http_feature_and_is_only_a_stub_otherwise() {
    // The production client is never constructed against a real endpoint in a test; this
    // asserts only that the entry point exists and reports its own failure honestly. With
    // `http` it builds a `reqwest` client; without it, a stated config error. The gate this
    // protects is G6 (`--no-default-features`), which compiles the other arm.
    let client = pie::provider::http::live_client(Duration::from_secs(5));
    assert!(
        client.is_ok(),
        "the default build has a TLS transport: {}",
        match &client {
            Ok(_) => "ok".to_string(),
            Err(error) => error.to_string(),
        }
    );

    #[cfg(not(feature = "http"))]
    {
        let client = client.unwrap();
        let error = client
            .post(&HttpRequest {
                url: "https://example.invalid".to_string(),
                body: json!({}),
                headers: Vec::new(),
            })
            .unwrap_err();
        assert!(error.to_string().contains("http"), "{error}");
    }
}
