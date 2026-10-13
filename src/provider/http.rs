//! M9's [`Transport`] over an OpenAI-compatible HTTP endpoint
//! (`planning/DEFERRED-BACKENDS.md`, `planning/ACCEPTANCE.md` §18 stage B).
//!
//! [`crate::tool_loop`] speaks to the model through a [`Transport`]: some frames go in,
//! `StreamEvent`s come out, and the loop neither knows nor cares what is on the other side.
//! [`SdkTransport`](crate::tool_loop::SdkTransport) puts a vendor CLI there; this puts a
//! `POST` and an SSE body there. That is the whole point of M9's seam — `RISKS.md` §8 says
//! the `tool_calls` path is what delegate-mode CLIs never exercise, and it is one loop with
//! two consumers rather than two implementations.
//!
//! ## A child process is a conversation; HTTP is not
//!
//! This is the difference worth understanding before reading the code. A child keeps the
//! turn in its own memory: pie writes *one* result frame and the process remembers everything
//! that came before. An HTTP endpoint is **stateless**, so every request re-POSTs the whole
//! conversation built so far — the user prompt, the `assistant` message that requested the
//! tool calls, and the `tool` messages answering them. [`HttpTransport::messages`] is
//! therefore the real state of the turn, and each request body is a projection of it. A
//! client that assumed the server remembered the previous request would be wrong in exactly
//! that way.
//!
//! That statelessness also decides **when** a request is sent, which is not where a child
//! process would suggest. A `tool` frame is recorded by [`HttpTransport::send`] and the
//! request is issued by the next [`HttpTransport::recv`], because one assistant message can
//! request two tools in parallel and the loop answers them one at a time: posting per frame
//! would send a conversation whose second call was still unanswered, leaving the endpoint to
//! be the first thing that noticed. Nothing is delayed by waiting — only `recv` can consume
//! the answer.
//!
//! ## The client is injected, and never a live one in a test
//!
//! [`HttpClient`] is a trait for the same reason `watch::news::Http` is
//! (`src/watch/news.rs`): a test that reaches a live endpoint fails when a provider reorders
//! its JSON, which teaches nothing about pie. Every test supplies a scripted client, so the
//! whole of this module is exercised offline — which is what gate G3d exists to protect.
//!
//! ## The key lives in one local, and nowhere else
//!
//! `RISKS.md` §10 and invariant I3: the key is read from the environment **at call time**,
//! placed in one `Authorization` header, and dropped. It is not a field of any struct here,
//! so no `Debug`, no log line and no error can print it — and [`HttpRequest`] has a **manual
//! `Debug`** that prints header *names*, because a derived one would print `Bearer <key>` the
//! first time somebody logged a request. `DEFERRED-BACKENDS.md`'s non-negotiable is that
//! reading the key is what makes these backends different, not that reading it is safe.

use std::collections::VecDeque;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};

use crate::error::{truncate, PieError, Result};
use crate::process;
use crate::provider::sse::SseDecoder;
use crate::provider::StreamEvent;
use crate::tool_loop::Transport;

/// How much of a failed response's body is read for the diagnostic.
///
/// Bounded because the body belongs to a server that just refused the request, and "it
/// returned 40 MB of HTML" must not become pie's memory problem. Enough for any JSON error
/// object a provider sends.
const MAX_ERROR_BODY: usize = 8 * 1024;

// ---------------------------------------------------------------------------
// The injected transport
// ---------------------------------------------------------------------------

/// One request, ready to send.
///
/// No `#[derive(Debug)]`: this type holds the credential in its headers, so `Debug` is
/// written by hand below and prints header **names** only. A derived one would print
/// `Authorization: Bearer <key>` into whatever log or error formatted it — the exact leak
/// invariant I3 forbids, introduced by a derive nobody would look at twice.
pub struct HttpRequest {
    /// Absolute URL of the endpoint.
    pub url: String,
    /// The JSON body (the whole conversation, plus the stream flags).
    pub body: Value,
    /// Headers in the order they are sent, including `Authorization`.
    pub headers: Vec<(String, String)>,
}

impl HttpRequest {
    /// One header's value, by case-insensitive name.
    ///
    /// Exists for the client implementations, not for callers: everything outside this
    /// module only ever needs to hand the whole request to [`HttpClient::post`].
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

impl fmt::Debug for HttpRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names: Vec<&str> = self.headers.iter().map(|(name, _)| name.as_str()).collect();
        f.debug_struct("HttpRequest")
            .field("url", &self.url)
            .field("headers", &names)
            // A byte count rather than the body: the body is the user's own conversation,
            // and it is large, and neither belongs in a log line.
            .field("body_bytes", &self.body.to_string().len())
            .finish()
    }
}

/// The body of a response, as it arrives.
///
/// A trait rather than a `String` because SSE is the whole point: the caller has to see
/// frames *as they stream*, and a chunk count is the only place a stalled connection and a
/// slow model differ.
pub trait ResponseStream: Send + Sync {
    /// The next chunk of the body, or `None` when the body ended.
    ///
    /// A transport failure — a reset connection, a read timeout — is a **stated error**.
    /// Returning `None` for it would turn a broken stream into a complete-looking empty
    /// answer, and that is the one outcome the SSE decoder's `finish` exists to refuse.
    fn next_chunk(&mut self) -> Result<Option<Vec<u8>>>;
}

/// A response: its status, and the stream of its body.
pub struct HttpResponse {
    /// HTTP status.
    pub status: u16,
    /// The body.
    pub stream: Box<dyn ResponseStream>,
}

impl fmt::Debug for HttpResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "HttpResponse {{ status: {} }}", self.status)
    }
}

/// The injectable HTTP transport.
///
/// Implementations: the scripted one in `tests/http_transport.rs`, and
/// [`live_client`]'s `reqwest`-backed one. The trait is **synchronous** while the real client
/// is async, which is deliberate — [`Transport`] is synchronous (the loop's whole design
/// assumes a blocking read it can bound), and the bridge belongs in the one implementation
/// that needs it, exactly as `commands/watch.rs`'s `NetworkHttp` bridges it.
pub trait HttpClient: Send + Sync {
    /// Send one request and return its response.
    ///
    /// A non-2xx is **not** this method's error to raise: it returns the response, status and
    /// all, and [`HttpTransport`] turns a failure into the stated error that names the status
    /// and a redacted body. One consequence is worth a test and so it is stated here: a
    /// client cannot report "it failed" by handing back an empty stream, because
    /// `HttpTransport` would then report the decoder's truncation error instead — the
    /// failure would be misattributed rather than hidden.
    fn post(&self, request: &HttpRequest) -> Result<HttpResponse>;

    /// Send one GET with no body, and return its response.
    ///
    /// Added by M10's follow-up. Until then the trait was POST-only *by construction* — its
    /// one caller streams a completion — and two things had no route through it:
    /// `GET /models` for a catalog, and OpenRouter's `GET /key` for the only quota-like
    /// figure either API-key vendor has. Both are honest stubs in `provider/api.rs` that
    /// this method unblocks.
    ///
    /// Same contract as [`HttpClient::post`]: a non-2xx is **returned**, not raised, so one
    /// place decides how a failure is described. And the same rule about the credential — the
    /// caller reads the key and passes it in as a header, so no client ever holds or logs it.
    ///
    /// **No default implementation on purpose.** A default that errored would let a client
    /// silently lack the capability, and the one place that would show up is a stub returning
    /// the wrong reason for the wrong cause. Requiring it makes every client state what it
    /// does.
    fn get(&self, url: &str, headers: &[(String, String)]) -> Result<HttpResponse>;
}

/// Everything one endpoint needs, minus the credential.
///
/// Deliberately vendor-neutral, including the two things that are not: the URL and the extra
/// body fields. `DEFERRED-BACKENDS.md` calls DeepSeek "identical apart from the two
/// constants", so the vendor vocabulary (which field asks for usage accounting, which
/// headers ask for attribution) belongs to the backend that knows its own endpoint, not to
/// this shared module.
#[derive(Debug, Clone)]
pub struct HttpSpec {
    /// Backend id, for errors.
    pub backend: String,
    /// Absolute URL of the chat-completions endpoint.
    pub url: String,
    /// Model id sent in the body.
    pub model: String,
    /// **Name** of the environment variable holding the key. The value is read per request
    /// and never stored, so a spec can be logged safely.
    pub key_env: &'static str,
    /// The user's prompt: the first user message.
    pub prompt: String,
    /// The persona, sent as a `system` message when the endpoint has one.
    ///
    /// Unlike the CLI backends there is no folding decision to make here: the
    /// OpenAI-compatible shape has a real `system` role, so the projection goes in whole and
    /// unmodified (`DESIGN.md` §10 — the bytes never change, only the mechanism).
    pub system: Option<String>,
    /// Extra headers, e.g. OpenRouter's attribution headers.
    pub headers: Vec<(String, String)>,
    /// Extra body fields.
    ///
    /// This is where a provider's own spelling of "send me the accounting" goes —
    /// `stream_options: {"include_usage": true}` on OpenAI-compatible endpoints,
    /// `usage: {"include": true}` on OpenRouter. It is not hardcoded here because a field a
    /// shared module guesses at is a field that 400s on the endpoint that spells it
    /// differently, and the accounting it buys is the whole M10 cost decision.
    pub body_extras: Vec<(String, Value)>,
}

impl HttpSpec {
    /// A spec with only the required parts set.
    pub fn new(
        backend: impl Into<String>,
        url: impl Into<String>,
        model: impl Into<String>,
        key_env: &'static str,
        prompt: impl Into<String>,
    ) -> Self {
        Self {
            backend: backend.into(),
            url: url.into(),
            model: model.into(),
            key_env,
            prompt: prompt.into(),
            system: None,
            headers: Vec::new(),
            body_extras: Vec::new(),
        }
    }
}

/// Read a whole (non-streaming) body, with a hard size cap.
///
/// For the two calls that are not a streamed completion: a model catalog and a
/// credit figure. Both come from a remote even pie does not control, and `cache::read`
/// already had to learn this lesson (B22) — an unbounded read of somebody else's file is an
/// allocation you did not choose. The cap is generous because a catalog is legitimately
/// large and a credit figure is not, and one cap for both is one thing to reason about.
pub fn read_body(response: &mut HttpResponse, cap_bytes: usize) -> Result<String> {
    let mut out: Vec<u8> = Vec::new();
    while let Some(chunk) = response.stream.next_chunk()? {
        if out.len().saturating_add(chunk.len()) > cap_bytes {
            return Err(PieError::Backend {
                backend: "http".to_string(),
                status: "response too large".to_string(),
                detail: format!(
                    "the body exceeded the {cap_bytes}-byte cap, so it was not read in full"
                ),
            });
        }
        out.extend_from_slice(&chunk);
    }

    // Lossy rather than strict: a catalog entry with one bad byte should cost that byte, not
    // the whole request. The same choice `drain_tail` makes for stderr, and the reason is the
    // same — the alternative discards a good answer because a vendor printed something odd.
    Ok(String::from_utf8_lossy(&out).into_owned())
}

/// Send a GET and read its body, turning a non-2xx into a stated error.
///
/// The error names the status and a **redacted** body, exactly as the streaming path does —
/// one place decides how an HTTP failure is described, or the two would eventually disagree
/// about whether a body is echoed. (It is redacted, and never a header, so the credential
/// cannot be read back out of a failure.)
pub fn get_json(
    client: &dyn HttpClient,
    backend: &str,
    url: &str,
    headers: &[(String, String)],
) -> Result<Value> {
    let mut response = client.get(url, headers)?;
    let status = response.status;
    let body = read_body(&mut response, MAX_JSON_BODY_BYTES)?;

    if !(200..300).contains(&status) {
        // Both the backend **and** the URL, because two vendors share this helper: a failure
        // that named neither would leave a user with both configured unable to tell which one
        // refused — and `http` alone, the first version, told them nothing at all.
        return Err(PieError::Backend {
            backend: backend.to_string(),
            status: format!("HTTP {status}"),
            detail: format!("{url}: {}", truncate(&process::redact(&body), 600)),
        });
    }

    serde_json::from_str(&body).map_err(|e| PieError::Backend {
        backend: backend.to_string(),
        status: "non-JSON response".to_string(),
        detail: format!("{url}: {e}"),
    })
}

/// Generous enough for a model catalog, finite enough to be a bound.
const MAX_JSON_BODY_BYTES: usize = 8 * 1024 * 1024;

/// M9's [`Transport`], over one HTTP endpoint.
pub struct HttpTransport {
    client: Arc<dyn HttpClient>,
    spec: HttpSpec,
    /// The conversation, which is the state a stateless endpoint cannot keep.
    messages: Vec<Value>,
    /// The open response, when one is being read.
    stream: Option<Box<dyn ResponseStream>>,
    /// Bytes in, events out.
    decoder: SseDecoder,
    /// Events parsed but not yet returned; a chunk can complete several frames.
    pending: VecDeque<StreamEvent>,
    /// `tool_calls` of the current assistant turn, waiting for the `tool_result` that
    /// answers one so they can be echoed back whole. The endpoint needs the assistant
    /// message that *requested* a call before it will accept the answer to it.
    held_calls: Vec<Value>,
    /// The answer text of the current assistant turn, taken from its `Done` event.
    turn_text: String,
    /// Every call id this transport has issued. A frame answering an id nobody asked about
    /// is refused here: the endpoint would answer a mismatched `tool_call_id` with a 400
    /// that reads like a provider bug, when it is pie's.
    issued: Vec<String>,
    /// Whether the first request has been sent.
    started: bool,
    /// A tool result was recorded and the conversation has not been sent since.
    pending_request: bool,
    /// Requests sent, so a caller (and a test) can see that each turn of the loop is a
    /// fresh POST of the whole conversation.
    requests: usize,
}

impl fmt::Debug for HttpTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Hand-written: the derived one would need `Debug` on the client trait object, and
        // more to the point a transport is the one object in a turn that has held the key.
        f.debug_struct("HttpTransport")
            .field("backend", &self.spec.backend)
            .field("requests", &self.requests)
            .field("messages", &self.messages.len())
            .finish_non_exhaustive()
    }
}

impl HttpTransport {
    /// A transport over `client` for one endpoint.
    ///
    /// Nothing is sent here: a constructor that performs I/O is a constructor a test cannot
    /// build, and `start` is the one explicit place a turn begins.
    pub fn new(client: Arc<dyn HttpClient>, spec: HttpSpec) -> Self {
        Self {
            decoder: SseDecoder::new(spec.backend.clone()),
            client,
            spec,
            messages: Vec::new(),
            stream: None,
            pending: VecDeque::new(),
            held_calls: Vec::new(),
            turn_text: String::new(),
            issued: Vec::new(),
            started: false,
            pending_request: false,
            requests: 0,
        }
    }

    /// Open the turn: the conversation starts with the persona (when there is one) and the
    /// user's prompt, then the first `POST`.
    pub fn start(&mut self) -> Result<()> {
        if self.started {
            return Err(PieError::protocol(
                self.spec.backend.as_str(),
                "the turn was already started; a second `start` would open a second \
                 conversation and drop the first one's history",
            ));
        }

        if let Some(system) = self.spec.system.as_deref() {
            if !system.trim().is_empty() {
                self.messages
                    .push(json!({ "role": "system", "content": system }));
            }
        }
        self.messages
            .push(json!({ "role": "user", "content": self.spec.prompt }));

        // Marked before the request, not after: the conversation exists from here, so a
        // failed first request must not leave a transport that claims no turn has begun.
        self.started = true;
        self.post()
    }

    /// How many requests this transport has sent.
    pub fn requests(&self) -> usize {
        self.requests
    }

    /// The conversation as it would be sent right now.
    pub fn messages(&self) -> &[Value] {
        &self.messages
    }

    /// The key, read from the environment **now**.
    ///
    /// At call time on purpose (`DEFERRED-BACKENDS.md`, "Non-negotiables"): a key captured
    /// at construction could outlive a rotated one, and a key stored on the struct is a key
    /// one `{:?}` away from a log file. A missing key is a **stated** error naming the
    /// variable — never a fallback to a different backend, because being answered by a
    /// subscription nobody asked for is the failure `resolve_target` exists to prevent.
    fn key(&self) -> Result<String> {
        match std::env::var(self.spec.key_env) {
            Ok(value) if !value.trim().is_empty() => Ok(value.trim().to_string()),
            _ => Err(PieError::AuthRequired {
                backend: self.spec.backend.clone(),
                hint: format!(
                    "set {} — this backend reads its key from the environment and nowhere \
                     else (not from config.toml, and never from a vendor CLI's own login)",
                    self.spec.key_env
                ),
            }),
        }
    }

    /// Build the body from the conversation so far.
    fn request_body(&self) -> Value {
        let mut body = json!({
            "model": self.spec.model,
            "messages": self.messages,
            // Streaming is not an optimisation here: without it there are no `delta`
            // frames, so no `tool_calls` to answer and no loop at all.
            "stream": true,
        });

        if let (Some(object), extras) = (body.as_object_mut(), &self.spec.body_extras) {
            for (key, value) in extras {
                object.insert(key.clone(), value.clone());
            }
        }

        body
    }

    /// Send the current conversation as a fresh POST and open its response.
    fn post(&mut self) -> Result<()> {
        // The one place the key exists as a value, and it goes straight into a header.
        let key = self.key()?;

        let mut headers = vec![
            ("authorization".to_string(), format!("Bearer {key}")),
            ("content-type".to_string(), "application/json".to_string()),
            ("accept".to_string(), "text/event-stream".to_string()),
        ];
        headers.extend(self.spec.headers.iter().cloned());

        let request = HttpRequest {
            url: self.spec.url.clone(),
            body: self.request_body(),
            headers,
        };

        let response = self.client.post(&request)?;
        self.requests += 1;

        if !(200..300).contains(&response.status) {
            let HttpResponse { status, mut stream } = response;
            let body = read_failure_body(stream.as_mut())?;
            return Err(http_failure(
                self.spec.backend.as_str(),
                self.spec.key_env,
                status,
                &body,
            ));
        }

        // A new response starts a new stream, and the decoder is per-response state that
        // must not carry the previous body's terminator into this one.
        self.decoder = SseDecoder::new(self.spec.backend.clone());
        self.stream = Some(response.stream);
        self.pending_request = false;
        Ok(())
    }

    /// Take the events a chunk produced, holding the state the loop's frames need.
    fn absorb(&mut self, events: Vec<StreamEvent>) {
        for event in events {
            match &event {
                StreamEvent::ToolUse { id, name, input } => self.hold_call(id, name, input),
                // `Done` carries the whole answer, so the assistant message echoed back
                // with its `tool_calls` can carry the text that accompanied the calls
                // instead of a `null` a provider may reject.
                StreamEvent::Done { text, .. } => self.turn_text = text.clone(),
                _ => {}
            }
            self.pending.push_back(event);
        }
    }

    /// Remember a call so its `tool_result` can be echoed back in the right shape.
    fn hold_call(&mut self, id: &str, name: &str, input: &Value) {
        self.issued.push(id.to_string());
        self.held_calls.push(json!({
            "id": id,
            "type": "function",
            "function": {
                "name": name,
                // The wire wants the arguments as a **string**, and `null` is not what a
                // no-argument call looks like there — the schema documents an empty object,
                // so a call the provider sent as `""` goes back as `{}`.
                "arguments": if input.is_null() { "{}".to_string() } else { input.to_string() },
            },
        }));
    }
}

impl Transport for HttpTransport {
    /// Answer one tool call: append the `tool` message, then POST the whole conversation.
    ///
    /// This is where the statelessness shows. The child-process transport writes one frame
    /// into a pipe; this rebuilds the entire conversation into a request body, because the
    /// endpoint remembers nothing between requests. It is also why the loop cannot tell the
    /// two apart: both accept a neutral frame and neither is asked what it did with it.
    fn send(&mut self, frame: Value) -> Result<()> {
        if !self.started {
            return Err(PieError::protocol(
                self.spec.backend.as_str(),
                "a tool result arrived before the turn's first request; there is no \
                 conversation to append it to",
            ));
        }

        let id = frame
            .get("tool_use_id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if id.trim().is_empty() {
            return Err(PieError::protocol(
                self.spec.backend.as_str(),
                "a tool result frame carried no `tool_use_id`; the endpoint matches on it, so \
                 a frame without one answers nothing",
            ));
        }
        if !self.issued.iter().any(|issued| issued == id) {
            return Err(PieError::protocol(
                self.spec.backend.as_str(),
                format!(
                    "a tool result answers `{}`, which is not a call this turn made — the \
                     endpoint would reject the mismatched `tool_call_id`, and the failure \
                     would look like a provider bug rather than pie's",
                    truncate(&process::redact(id), 120)
                ),
            ));
        }

        // The endpoint will not accept an answer to a call unless the message that
        // *requested* it is in the conversation, so the assistant turn goes in first.
        if !self.held_calls.is_empty() {
            let content = if self.turn_text.trim().is_empty() {
                Value::Null
            } else {
                Value::String(std::mem::take(&mut self.turn_text))
            };
            self.messages.push(json!({
                "role": "assistant",
                "content": content,
                "tool_calls": std::mem::take(&mut self.held_calls),
            }));
        }

        let output = frame
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or_default();
        // The OpenAI-compatible shape has no error flag on a tool message, unlike claude's
        // SDK. A failed tool therefore has to say so in its content, or the model reads a
        // failed `read` as a successful one that returned nothing (`src/tool_loop.rs`
        // sends the flag; the wire here cannot).
        let is_error = frame
            .get("is_error")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let content = if is_error {
            format!("[tool_error] {output}")
        } else {
            output.to_string()
        };

        self.messages.push(json!({
            "role": "tool",
            "tool_call_id": id,
            "content": content,
        }));

        // The request itself is issued by the next `recv`, not here — see the module doc:
        // two parallel calls arrive as two events, and a POST per event would send the
        // conversation with the second call unanswered.
        self.pending_request = true;
        Ok(())
    }

    /// The next event, reading more of the body as needed.
    ///
    /// End of body is **not** the end of the answer: it is the point where the decoder gets
    /// to decide whether the stream ended or was cut, so `finish` is called here rather than
    /// treating a closed socket as a completed turn.
    fn recv(&mut self) -> Result<Option<StreamEvent>> {
        loop {
            if let Some(event) = self.pending.pop_front() {
                return Ok(Some(event));
            }

            if let Some(stream) = self.stream.as_mut() {
                match stream.next_chunk()? {
                    Some(bytes) => {
                        let events = self.decoder.feed(&bytes)?;
                        self.absorb(events);
                    }
                    None => {
                        let events = self.decoder.finish()?;
                        self.stream = None;
                        self.absorb(events);
                    }
                }
                continue;
            }

            if self.pending_request {
                self.post()?;
                continue;
            }

            if !self.started {
                return Err(PieError::protocol(
                    self.spec.backend.as_str(),
                    "an event was requested before any request was made; an empty turn here \
                     would report a model that said nothing rather than a turn that never ran",
                ));
            }
            return Ok(None);
        }
    }
}

// ---------------------------------------------------------------------------
// Failures
// ---------------------------------------------------------------------------

/// Read a failed response's body, bounded, for the diagnostic.
fn read_failure_body(stream: &mut dyn ResponseStream) -> Result<String> {
    let mut body: Vec<u8> = Vec::new();
    while body.len() < MAX_ERROR_BODY {
        match stream.next_chunk()? {
            Some(chunk) => body.extend_from_slice(&chunk),
            None => break,
        }
    }
    body.truncate(MAX_ERROR_BODY);
    Ok(String::from_utf8_lossy(&body).into_owned())
}

/// Turn a non-2xx response into an error naming the status and a redacted body.
///
/// The shape `codex.rs`'s `describe_http_failure` established: **the status, and a redacted
/// body** — never a header, and so never the key. The body goes through
/// [`process::redact`] as well, because a provider that echoes the offending `Authorization`
/// header in its 401 body would otherwise put the credential into pie's error output, which
/// is exactly the leak I3 forbids and precisely how it would happen.
fn http_failure(backend: &str, key_env: &str, status: u16, body: &str) -> PieError {
    let detail = format!("HTTP {status}: {}", snippet(body));

    if status == 401 || status == 403 {
        return PieError::AuthRequired {
            backend: backend.to_string(),
            hint: format!(
                "the {key_env} credential was rejected ({detail}). The key is read from the \
                 environment only; check it, and that the account may use this model"
            ),
        };
    }

    PieError::Backend {
        backend: backend.to_string(),
        status: format!("HTTP {status}"),
        detail,
    }
}

/// A bounded, redacted rendering of a response body.
fn snippet(body: &str) -> String {
    let redacted = process::redact(body);
    let trimmed = redacted.trim();
    if trimmed.is_empty() {
        return "(empty body)".to_string();
    }
    truncate(trimmed, 600)
}

// ---------------------------------------------------------------------------
// The real client
// ---------------------------------------------------------------------------

/// A client backed by `reqwest`, with the runtime bridge the loop needs.
///
/// `Transport` is synchronous (`src/tool_loop.rs`), so an async client is bridged with
/// `block_in_place` + `Handle::current().block_on` — the same one `commands/watch.rs` uses
/// for `watch::news::Http`, and it needs the multi-threaded runtime `main.rs` starts.
///
/// The timeout is a **read** timeout rather than a total one: a legitimate answer may stream
/// for minutes, so a wall-clock cap would cut off exactly the long turns this backend exists
/// to serve, while a stalled socket still has to fail rather than hang the turn.
#[cfg(feature = "http")]
pub fn live_client(read_timeout: Duration) -> Result<Arc<dyn HttpClient>> {
    Ok(Arc::new(ReqwestClient {
        client: reqwest::Client::builder()
            .connect_timeout(read_timeout)
            .read_timeout(read_timeout)
            .build()
            .map_err(|error| PieError::Backend {
                backend: "http".to_string(),
                status: "client build failed".to_string(),
                detail: truncate(&process::redact(&error.to_string()), 300),
            })?,
    }))
}

/// Without TLS there is no client, and the honest answer is a stated error.
///
/// `--no-default-features` drops `reqwest` (gate G6), so this build must still compile and
/// must still tell the truth: a config error naming the missing feature rather than a
/// transport that silently answers nothing. Same shape as
/// `commands/watch.rs`'s `NetworkHttp` under `not(feature = "http")`.
#[cfg(not(feature = "http"))]
pub fn live_client(read_timeout: Duration) -> Result<Arc<dyn HttpClient>> {
    let _ = read_timeout;
    Ok(Arc::new(NoTransport))
}

/// The no-TLS stand-in.
#[cfg(not(feature = "http"))]
#[derive(Debug)]
struct NoTransport;

#[cfg(not(feature = "http"))]
impl HttpClient for NoTransport {
    fn post(&self, request: &HttpRequest) -> Result<HttpResponse> {
        Err(self.no_tls(&request.url))
    }

    fn get(&self, url: &str, _headers: &[(String, String)]) -> Result<HttpResponse> {
        Err(self.no_tls(url))
    }
}

#[cfg(not(feature = "http"))]
impl NoTransport {
    /// The one thing a no-TLS build can say, said the same way for both verbs.
    ///
    /// Shared rather than repeated: two copies of this message would eventually disagree
    /// about which feature is missing, and the message is the whole value of the stub.
    fn no_tls(&self, url: &str) -> PieError {
        PieError::Config(format!(
            "this build has no TLS transport (`http` was disabled), so {url} cannot be reached — \
             this needs a build with the default features"
        ))
    }
}

/// The `reqwest`-backed client.
#[cfg(feature = "http")]
#[derive(Debug)]
struct ReqwestClient {
    client: reqwest::Client,
}

#[cfg(feature = "http")]
impl HttpClient for ReqwestClient {
    fn post(&self, request: &HttpRequest) -> Result<HttpResponse> {
        let client = &self.client;
        let url = &request.url;
        let body = &request.body;
        let headers = &request.headers;

        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async move {
                let mut builder = client.post(url);
                for (name, value) in headers {
                    builder = builder.header(name.as_str(), value.as_str());
                }

                let response = builder.json(body).send().await?;
                Ok(HttpResponse {
                    status: response.status().as_u16(),
                    stream: Box::new(ReqwestStream { response }),
                })
            })
        })
    }

    fn get(&self, url: &str, headers: &[(String, String)]) -> Result<HttpResponse> {
        let client = &self.client;

        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async move {
                let mut builder = client.get(url);
                // Headers only — a GET carries no body, so there is nothing else to attach
                // and nothing that could carry the conversation by accident.
                for (name, value) in headers {
                    builder = builder.header(name.as_str(), value.as_str());
                }

                let response = builder.send().await?;
                Ok(HttpResponse {
                    status: response.status().as_u16(),
                    stream: Box::new(ReqwestStream { response }),
                })
            })
        })
    }
}

/// A `reqwest` response body, read one chunk at a time.
#[cfg(feature = "http")]
#[derive(Debug)]
struct ReqwestStream {
    response: reqwest::Response,
}

#[cfg(feature = "http")]
impl ResponseStream for ReqwestStream {
    fn next_chunk(&mut self) -> Result<Option<Vec<u8>>> {
        // `Response::chunk` is the streaming read that needs no `futures-util` — see the
        // note in `Cargo.toml` about why that dependency is deliberately absent.
        let chunk = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(self.response.chunk())
        })?;
        Ok(chunk.map(|bytes| bytes.to_vec()))
    }
}
