//! The SSE wire for the API-key backends (`ACCEPTANCE.md` §18 stage A).
//!
//! **Gate:** `cargo test --test sse`.
//!
//! Every test here drives the decoder with bytes this file wrote. Nothing reaches the
//! network, so the whole stage is machine-independent — the property gate G3d exists to
//! protect — and a provider reordering its JSON cannot turn a green suite red for a reason
//! that teaches nothing about pie.
//!
//! Two claims carry the stage and each has a test that fails if it is false:
//! `every_chunk_boundary_produces_the_same_events` (incrementality — a frame lost when a
//! read splits it) and `a_stream_that_never_terminates_is_an_error_rather_than_an_empty_answer`
//! (a cut connection must not read as a complete answer).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use pie::provider::sse::{
    cost_usd_micros, SseDecoder, COST_EVENT_KIND, DONE_MARKER, MAX_FRAME_BYTES,
};
use pie::provider::{StreamEvent, Usage};

/// A body that exercises everything one turn can contain: text, a tool call split into
/// fragments, accounting with a cost, and the terminator.
const FULL_TURN: &str = concat!(
    "data: {\"id\":\"gen-1\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"}}]}\n\n",
    "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Look\"}}]}\n\n",
    "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_a\",\"type\":\"function\",\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"pa\"}}]}}]}\n\n",
    "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"th\\\":\\\"a.txt\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n",
    "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":4,\"total_tokens\":14,\"cost\":0.000321}}\n\n",
    "data: [DONE]\n\n"
);

/// Decode a body in **fixed-size** chunks rather than one call, so frame boundaries land
/// inside chunks rather than at their edges: the network does not align frames to reads, and
/// a decoder that only works when it does is the bug this stage exists to catch.
fn decode(backend: &str, body: &str) -> Vec<StreamEvent> {
    let mut decoder = SseDecoder::new(backend);
    let mut events = Vec::new();
    for chunk in body.as_bytes().chunks(5) {
        events.extend(
            decoder
                .feed(chunk)
                .expect("the body written here is decodable"),
        );
    }
    events.extend(
        decoder
            .finish()
            .expect("a terminated body finishes cleanly"),
    );
    events
}

/// The answer text, as a consumer of the deltas sees it.
fn text(events: &[StreamEvent]) -> String {
    let mut out = String::new();
    for event in events {
        if let StreamEvent::TextDelta(chunk) = event {
            out.push_str(chunk);
        }
    }
    out
}

/// The first tool call, in the shape the loop consumes.
fn first_call(events: &[StreamEvent]) -> Option<(String, String, serde_json::Value)> {
    events.iter().find_map(|event| match event {
        StreamEvent::ToolUse { id, name, input } => Some((id.clone(), name.clone(), input.clone())),
        _ => None,
    })
}

fn first_usage(events: &[StreamEvent]) -> Option<Usage> {
    events.iter().find_map(|event| match event {
        StreamEvent::Usage(usage) => Some(usage.clone()),
        _ => None,
    })
}

// ---------------------------------------------------------------------------
// Incrementality
// ---------------------------------------------------------------------------

/// The claim the whole stage rests on: a frame boundary lands wherever the network puts it,
/// so **every** split of the same bytes must produce the same events.
///
/// A decoder that dispatches early, or drops a partial tail, passes a single-read test and
/// fails this one at the split that crosses a `data:` line — which is why it is written as a
/// sweep over every boundary rather than a couple of hand-picked ones.
#[test]
fn every_chunk_boundary_produces_the_same_events() {
    let reference = decode("openrouter", FULL_TURN);
    let bytes = FULL_TURN.as_bytes();

    for split in 1..bytes.len() {
        let mut decoder = SseDecoder::new("openrouter");
        let mut events = decoder.feed(&bytes[..split]).unwrap();
        events.extend(decoder.feed(&bytes[split..]).unwrap());
        events.extend(decoder.finish().unwrap());

        assert_eq!(
            events, reference,
            "a frame was lost or reordered when the body was split at byte {split}"
        );
    }

    // And the same body one byte at a time, which is the worst case a socket can produce.
    let mut decoder = SseDecoder::new("openrouter");
    let mut events = Vec::new();
    for byte in bytes {
        events.extend(decoder.feed(&[*byte]).unwrap());
    }
    events.extend(decoder.finish().unwrap());
    assert_eq!(events, reference);
}

#[test]
fn a_frame_split_across_chunks_is_not_lost() {
    let mut decoder = SseDecoder::new("openrouter");
    let frame = b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n";

    let mut events = Vec::new();
    for byte in frame.iter() {
        events.extend(decoder.feed(&[*byte]).unwrap());
    }
    assert_eq!(text(&events), "hi");
}

#[test]
fn a_multi_byte_character_split_across_chunks_does_not_panic() {
    let body = "data: {\"choices\":[{\"delta\":{\"content\":\"\u{e9}\"}}]}\n\n";
    let bytes = body.as_bytes();
    // One byte into the two-byte character: the split lands inside a UTF-8 sequence, which
    // is why framing is done on bytes and decoding only on complete lines.
    let split = body.find('\u{e9}').unwrap() + 1;

    let mut decoder = SseDecoder::new("openrouter");
    let mut events = decoder.feed(&bytes[..split]).unwrap();
    events.extend(decoder.feed(&bytes[split..]).unwrap());
    assert_eq!(text(&events), "\u{e9}");
}

// ---------------------------------------------------------------------------
// The frames an SSE stream is allowed to contain
// ---------------------------------------------------------------------------

#[test]
fn a_frame_that_is_not_data_is_ignored_rather_than_fatal() {
    let body = ": keepalive\nevent: message\nid: 4\nretry: 100\ndata: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n";
    let events = decode("openrouter", body);
    assert_eq!(text(&events), "hi");
}

#[test]
fn a_payload_split_across_several_data_lines_is_joined() {
    // The SSE spec joins a frame's `data:` lines with a newline, which is what lets a
    // payload be delivered in pieces. JSON tolerates the newline between tokens.
    let body = "data: {\"choices\":[{\"delta\":\ndata: {\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n";
    let events = decode("openrouter", body);
    assert_eq!(text(&events), "hi", "{events:?}");
}

#[test]
fn carriage_returns_do_not_become_part_of_the_payload() {
    let body = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\r\n\r\ndata: [DONE]\r\n\r\n";
    let events = decode("openrouter", body);
    assert_eq!(text(&events), "hi");
    assert!(events
        .iter()
        .any(|event| matches!(event, StreamEvent::Done { .. })));
}

#[test]
fn a_payload_that_is_not_json_does_not_abort_the_stream() {
    // Invariant I5, at the frame level: one frame we cannot read must not discard the
    // frames we can. The unreadable one is forwarded so nothing disappears silently.
    let body = "data: <html>nope</html>\n\ndata: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n";
    let events = decode("openrouter", body);
    assert!(
        matches!(events.first(), Some(StreamEvent::Unparsed(raw)) if raw.contains("nope")),
        "{events:?}"
    );
    assert_eq!(text(&events), "hi");
}

#[test]
fn a_body_that_closes_without_the_final_blank_line_still_delivers_its_last_frame() {
    // The separator is what normally ends a frame, and a server that closes the body right
    // after the last one sent a complete frame: the payload parses, so nothing is missing.
    let mut decoder = SseDecoder::new("openrouter");
    let mut events = decoder
        .feed(b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"stop\"}]}")
        .unwrap();
    assert!(
        events.is_empty(),
        "nothing is dispatched before the frame's end is known: {events:?}"
    );
    events.extend(decoder.finish().unwrap());
    assert_eq!(text(&events), "hi");
    assert!(
        matches!(events.last(), Some(StreamEvent::Done { status, .. }) if status == "stop"),
        "{events:?}"
    );
}

#[test]
fn the_done_marker_ends_the_stream_and_later_bytes_cannot_resurrect_it() {
    let mut decoder = SseDecoder::new("openrouter");
    let events = decoder.feed(b"data: [DONE]\n\n").unwrap();
    assert!(
        matches!(events.as_slice(), [StreamEvent::Done { .. }]),
        "{events:?}"
    );
    assert!(decoder.terminated());
    assert!(
        decoder
            .feed(b"data: {\"choices\":[]}\n\n")
            .unwrap()
            .is_empty(),
        "a frame after the terminator belongs to no turn"
    );
    assert!(decoder.finish().unwrap().is_empty());
}

#[test]
fn the_terminator_is_recognised_with_the_whitespace_providers_put_around_it() {
    for body in ["data: [DONE]\n\n", "data:[DONE]\n\n", "data:  [DONE]\n\n"] {
        let events = decode("openrouter", body);
        assert!(
            matches!(events.as_slice(), [StreamEvent::Done { .. }]),
            "{body:?} produced {events:?}"
        );
        assert_eq!(DONE_MARKER, "[DONE]");
    }
}

// ---------------------------------------------------------------------------
// The three cases the brief asks to be decided, and what was decided
// ---------------------------------------------------------------------------

#[test]
fn a_stream_that_never_terminates_is_an_error_rather_than_an_empty_answer() {
    let mut decoder = SseDecoder::new("openrouter");
    let events = decoder
        .feed(b"data: {\"choices\":[{\"delta\":{\"content\":\"half\"}}]}\n\n")
        .unwrap();
    assert_eq!(text(&events), "half");

    let error = decoder.finish().unwrap_err();
    assert!(
        error.to_string().contains("ended with no `[DONE]`"),
        "{error}"
    );
    assert_eq!(
        error.exit_code(),
        4,
        "a protocol error has its own exit code"
    );
}

#[test]
fn a_body_cut_mid_frame_is_an_error_naming_the_truncation() {
    let mut decoder = SseDecoder::new("openrouter");
    decoder
        .feed(b"data: {\"choices\":[{\"delta\":{\"cont")
        .unwrap();

    let error = decoder.finish().unwrap_err();
    assert!(error.to_string().contains("mid-frame"), "{error}");
    // The evidence is redacted and bounded before it reaches a message (I3).
    assert!(
        error.to_string().contains("cont"),
        "names what arrived: {error}"
    );
}

#[test]
fn finish_is_idempotent_and_only_ever_completes_a_turn_once() {
    let mut decoder = SseDecoder::new("openrouter");
    let events = decoder
        .feed(b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"stop\"}]}\n\n")
        .unwrap();
    assert_eq!(text(&events), "hi");

    let first = decoder.finish().unwrap();
    assert_eq!(first.len(), 1);
    assert!(
        decoder.finish().unwrap().is_empty(),
        "a second finish must not emit a second Done"
    );
}

#[test]
fn a_stream_with_no_finish_reason_still_terminates_on_done_and_says_status_unknown() {
    // The status is the vendor's own word or `unknown` — never a value pie invented.
    let events = decode(
        "openrouter",
        "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n",
    );
    assert!(
        matches!(events.last(), Some(StreamEvent::Done { status, text: answer }) if status == "unknown" && answer == "hi"),
        "{events:?}"
    );
}

#[test]
fn a_mid_stream_error_frame_is_a_stated_error_rather_than_a_truncated_answer() {
    // A 200 that then fails. This is not the "bad line" I5 tolerates: the provider is
    // telling us the turn failed, and the prefix already delivered would read as the answer.
    let mut decoder = SseDecoder::new("openrouter");
    let error = decoder
        .feed(b"data: {\"error\":{\"code\":402,\"message\":\"insufficient credits\"}}\n\n")
        .unwrap_err();
    assert!(error.to_string().contains("HTTP 402"), "{error}");
    assert!(
        error.to_string().contains("insufficient credits"),
        "{error}"
    );
}

#[test]
fn an_oversized_frame_is_refused_rather_than_buffered() {
    let mut decoder = SseDecoder::new("openrouter");
    decoder.feed(b"data: ").unwrap();
    let error = decoder.feed(&vec![b'x'; MAX_FRAME_BYTES]).unwrap_err();
    assert!(error.to_string().contains("grew past"), "{error}");
}

// ---------------------------------------------------------------------------
// The content, the calls and the accounting
// ---------------------------------------------------------------------------

#[test]
fn text_content_arrives_as_deltas_in_order() {
    let body = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"Hello \"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"world\"}}]}\n\n",
        "data: [DONE]\n\n"
    );
    let events = decode("deepseek", body);
    let deltas: Vec<String> = events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::TextDelta(chunk) => Some(chunk.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(deltas, vec!["Hello ".to_string(), "world".to_string()]);
}

#[test]
fn content_sent_as_text_parts_is_read_rather_than_dropped() {
    // Newer OpenAI-compatible shapes send `content` as a list of parts. Ignoring the shape
    // would drop the entire answer on a provider that adopted it.
    let body = "data: {\"choices\":[{\"delta\":{\"content\":[{\"type\":\"text\",\"text\":\"hi\"}]}}]}\n\ndata: [DONE]\n\n";
    let events = decode("deepseek", body);
    assert_eq!(text(&events), "hi");
}

#[test]
fn arguments_split_across_frames_are_parsed_once_complete() {
    let events = decode("deepseek", FULL_TURN);
    let (id, name, input) = first_call(&events).expect("the turn asks for a tool");
    assert_eq!(id, "call_a", "the id arrives only on the first fragment");
    assert_eq!(name, "read", "and so does the name");
    assert_eq!(
        input["path"], "a.txt",
        "the fragments join into one JSON object"
    );
}

#[test]
fn a_tool_call_is_emitted_only_once_its_arguments_are_complete() {
    // The order matters: a runner handed the call earlier would run it on a fragment of its
    // arguments, which for a `bash` tool is a different command rather than a smaller one.
    let events = decode("deepseek", FULL_TURN);
    let usage_at = events
        .iter()
        .position(|event| matches!(event, StreamEvent::Usage(_)))
        .expect("a usage event");
    let call_at = events
        .iter()
        .position(|event| matches!(event, StreamEvent::ToolUse { .. }))
        .expect("a tool call");

    assert!(
        call_at < usage_at,
        "the terminal frame flushes the calls: {events:?}"
    );
    assert!(
        matches!(events.last(), Some(StreamEvent::Done { .. })),
        "and `Done` stays last: {events:?}"
    );
}

#[test]
fn tool_calls_are_emitted_in_the_order_the_provider_indexed_them() {
    let body = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":1,\"id\":\"b\",\"function\":{\"name\":\"second\",\"arguments\":\"{}\"}},{\"index\":0,\"id\":\"a\",\"function\":{\"name\":\"first\",\"arguments\":\"{}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n";
    let events = decode("deepseek", body);

    let ids: Vec<String> = events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::ToolUse { id, .. } => Some(id.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(ids, vec!["a".to_string(), "b".to_string()]);
}

#[test]
fn a_call_with_no_arguments_is_a_null_input_rather_than_a_parse_failure() {
    let body = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"ping\",\"arguments\":\"\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n";
    let events = decode("deepseek", body);
    let (_, name, input) = first_call(&events).expect("a call");
    assert_eq!(name, "ping");
    assert_eq!(input, serde_json::Value::Null);
}

#[test]
fn arguments_that_are_not_json_are_a_stated_error_rather_than_a_null_tool_call() {
    let body = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"bash\",\"arguments\":\"not json\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n";
    let mut decoder = SseDecoder::new("deepseek");
    let error = decoder.feed(body.as_bytes()).unwrap_err();
    assert!(error.to_string().contains("not JSON"), "{error}");
}

#[test]
fn a_call_with_no_id_is_a_stated_error_rather_than_an_unanswerable_frame() {
    let body = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"name\":\"read\",\"arguments\":\"{}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n";
    let mut decoder = SseDecoder::new("deepseek");
    let error = decoder.feed(body.as_bytes()).unwrap_err();
    assert!(error.to_string().contains("no id"), "{error}");
}

#[test]
fn reasoning_tokens_are_moved_out_of_the_completion_count_rather_than_double_counted() {
    let body = concat!(
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{",
        "\"prompt_tokens\":10,\"completion_tokens\":20,\"total_tokens\":30,",
        "\"completion_tokens_details\":{\"reasoning_tokens\":7},",
        "\"prompt_cache_hit_tokens\":4}}\n\n",
        "data: [DONE]\n\n"
    );
    let usage = first_usage(&decode("deepseek", body)).expect("a usage event");

    assert_eq!(usage.input_tokens, 10);
    assert_eq!(
        usage.output_tokens, 13,
        "completion_tokens includes reasoning"
    );
    assert_eq!(usage.thinking_tokens, 7);
    assert_eq!(usage.cache_read_tokens, 4);
    assert_eq!(
        usage.computed_total(),
        30,
        "the parts sum to the reported total"
    );
}

#[test]
fn the_openai_spelling_of_the_cached_token_count_is_read_too() {
    let body = "data: {\"usage\":{\"prompt_tokens\":8,\"completion_tokens\":1,\"prompt_tokens_details\":{\"cached_tokens\":5}}}\n\ndata: [DONE]\n\n";
    let usage = first_usage(&decode("openrouter", body)).expect("a usage event");
    assert_eq!(usage.cache_read_tokens, 5);
    assert_eq!(
        usage.cache_write_tokens, 0,
        "no OpenAI-compatible endpoint reports cache writes, and zero means `not reported`"
    );
}

#[test]
fn an_empty_usage_object_emits_nothing_rather_than_asserting_a_free_turn() {
    let body =
        "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}],\"usage\":{}}\n\ndata: [DONE]\n\n";
    let events = decode("openrouter", body);
    assert!(
        first_usage(&events).is_none(),
        "a zero usage is indistinguishable from `not reported`, so it must not be emitted: {events:?}"
    );
}

#[test]
fn a_reported_cost_is_mapped_and_one_that_is_not_reported_is_absent() {
    let with_cost = "data: {\"usage\":{\"prompt_tokens\":1,\"cost\":0.000123}}\n\ndata: [DONE]\n\n";
    let events = decode("openrouter", with_cost);
    assert_eq!(events.iter().find_map(cost_usd_micros), Some(123));
    assert!(
        events.iter().any(
            |event| matches!(event, StreamEvent::Other { kind, .. } if kind == COST_EVENT_KIND)
        ),
        "the cost rides in the event kind `--json` already forwards: {events:?}"
    );

    // DeepSeek reports tokens and no money. Absent, never zero: M4 removed the cost column
    // because a permanent `$0.00` claims cost is measured when it is not.
    let without = "data: {\"usage\":{\"prompt_tokens\":1}}\n\ndata: [DONE]\n\n";
    let events = decode("deepseek", without);
    assert_eq!(events.iter().find_map(cost_usd_micros), None);
    assert!(
        !events.iter().any(
            |event| matches!(event, StreamEvent::Other { kind, .. } if kind == COST_EVENT_KIND)
        ),
        "{events:?}"
    );
}

#[test]
fn a_cost_that_is_not_money_is_treated_as_not_reported() {
    for cost in ["-1", "1e400"] {
        let body = format!(
            "data: {{\"usage\":{{\"prompt_tokens\":1,\"cost\":{cost}}}}}\n\ndata: [DONE]\n\n"
        );
        let events = decode("openrouter", &body);
        assert_eq!(
            events.iter().find_map(cost_usd_micros),
            None,
            "cost {cost} is not a figure to render"
        );
    }
}

#[test]
fn the_answer_carried_by_the_terminal_event_is_bounded() {
    // `Done` carries a second copy of the answer for a consumer that does not accumulate
    // deltas. An unbounded copy of a stream whose length the provider chooses has no reader,
    // so it stops growing.
    let mut body = String::new();
    for _ in 0..3_000 {
        body.push_str("data: {\"choices\":[{\"delta\":{\"content\":\"");
        body.push_str(&"x".repeat(100));
        body.push_str("\"}}]}\n\n");
    }
    body.push_str("data: [DONE]\n\n");

    let events = decode("openrouter", &body);
    let Some(StreamEvent::Done { text: answer, .. }) = events.last() else {
        panic!("no terminal event: {events:?}");
    };
    assert!(answer.len() < 300_000, "the copy grew to {}", answer.len());
    assert_eq!(
        text(&events).len(),
        300_000,
        "while every delta still reaches the caller"
    );
}
