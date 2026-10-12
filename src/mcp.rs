//! `pie mcp ocr` — the OCR capability hosted over MCP, for the delegate CLIs.
//!
//! In delegate mode `agy` and `codex` run their **own** tools, so a pie-native
//! tool never reaches them (`RISKS.md` §11). MCP is the only route that serves all
//! three backends: the user points each CLI at this server through that CLI's own
//! MCP config, and every model then gets `ocr` uniformly. `PLAN.md` §10 is the
//! design source; `DESIGN.md` §11 fixes the shape.
//!
//! Four properties matter more than the plumbing:
//!
//! * **Exactly one tool.** An advertised tool raises the vendor's per-turn input
//!   tokens on *every* turn, including turns that never call it, so a second tool
//!   — even a health probe — would tax the whole session. `ocr` is the entire
//!   catalog, and `tests/mcp.rs` asserts it by count rather than by membership.
//! * **stdout carries protocol frames and nothing else.** A stray log line on
//!   stdout is not cosmetic noise: it is a corrupt stream that the client cannot
//!   parse. Diagnostics go to stderr through `tracing`, exactly as `pie -p`'s
//!   answer channel does (`DESIGN.md` §6).
//! * **A failure is stated, never an empty success.** An unreachable itsmini comes
//!   back as an MCP tool **error result** (`isError`), carrying the same message
//!   `pie ocr` prints — the rule `ACCEPTANCE.md` §8 applies to stage D, kept here
//!   because this server is a second door onto the same capability.
//! * **No vendor CLI is spawned and no session row is written.** The work happens
//!   on itsmini; this server forwards, so it spends no model quota.
//!
//! Hand-rolled on `serde_json` rather than an MCP SDK: the stdio half of the
//! protocol is one JSON object per line, and a dependency for that would be a
//! liability rather than a saving.
//!
//! Two protocol details are deliberate rather than accidental. This server does
//! **not** require an `initialize` first, and it answers `ping`: a stall there is
//! the kind of thing a vendor treats as a dead server and drops, while the
//! request-handling rule above (a malformed frame is answered and the loop
//! continues) is the one that keeps a client alive.

use std::path::PathBuf;

use serde_json::{json, Map, Value};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

use crate::error::{truncate, PieError, Result};
use crate::ocr::{self, Request as OcrRequest};
use crate::process;
use crate::{NAME, VERSION};

/// Name of the one tool this server advertises, and the only capability `pie mcp`
/// hosts. The MCP tool name and the `pie` subcommand name are the same word on
/// purpose: one capability, one name, in both places a user can type it.
pub const TOOL_OCR: &str = "ocr";

/// The protocol revision pie answers with when the client names none it knows.
///
/// Deliberately the newest revision pie has been written against rather than a
/// guess at what a client speaks: the client states its own version in
/// `initialize`, and [`SUPPORTED_PROTOCOL_VERSIONS`] is what decides whether it is
/// echoed back.
pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// Every revision this server will echo.
///
/// An echo is not a claim of full conformance — the surface here is four methods
/// and one tool, none of which changed shape across these revisions — it is the
/// answer to "may we proceed as though we agree", which is why the client's
/// version is repeated back whenever it appears in this list.
pub const SUPPORTED_PROTOCOL_VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];

/// The tool description, bounded because every advertised byte is billed on every
/// turn of the session (see the module docs). It says what the tool does and the
/// one non-obvious fact about it — that repeats are free — and stops.
pub const OCR_TOOL_DESCRIPTION: &str =
    "Convert an image or a scanned PDF to text through itsmini. Cached by content \
     hash, so the same file is converted once.";

/// The longest description this server will advertise.
///
/// A cap rather than a style note: the whole point of the opt-in (`DESIGN.md` §11)
/// is that an advertised tool costs tokens on every turn, so the schema is kept
/// minimal on purpose and the limit is asserted rather than remembered.
pub const MAX_TOOL_DESCRIPTION_CHARS: usize = 200;

/// A frame larger than this is refused unparsed.
///
/// MCP requests are small — the largest thing that legitimately arrives here is a
/// file path — so a megabyte is already far past anything a real client sends, and
/// the bound is what stops a malformed stream from being read into memory without
/// limit.
const MAX_FRAME_BYTES: usize = 1024 * 1024;

/// Bound on an error message handed back through a tool result.
///
/// The message can quote a transport failure, whose text is not ours, so it is
/// redacted and bounded before it reaches the client (`RISKS.md` §10).
const MAX_TOOL_ERROR_CHARS: usize = 600;

/// JSON-RPC 2.0 error codes. Only the five this server can produce.
const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

/// The one protocol revision string JSON-RPC requires on every frame.
const JSONRPC: &str = "2.0";

/// Run the server named by `capability` over this process's own stdin/stdout.
///
/// The capability is validated rather than ignored: `pie mcp ocr` is the only
/// server pie hosts, and a name that is not `ocr` is a typo the user should see
/// now rather than a server that silently does nothing.
pub async fn serve(capability: &str) -> Result<()> {
    if capability != TOOL_OCR {
        return Err(PieError::Config(format!(
            "unknown MCP capability `{capability}` — `pie mcp ocr` is the only server pie hosts"
        )));
    }

    tracing::debug!(
        tool = TOOL_OCR,
        protocol = PROTOCOL_VERSION,
        "pie mcp ocr: serving JSON-RPC on stdin/stdout"
    );

    // stderr, always: stdout is the frame channel from here on.
    let mut reader = tokio::io::BufReader::new(tokio::io::stdin());
    let mut writer = tokio::io::stdout();
    Server::new().run(&mut reader, &mut writer).await
}

/// The JSON-RPC server.
///
/// Stateless: every method here answers from the request plus what itsmini says,
/// so there is nothing to carry between frames. It exists as a type rather than as
/// free functions so the dispatch table has an owner and the ocr call has one place
/// to live.
#[derive(Debug, Default, Clone, Copy)]
pub struct Server;

impl Server {
    /// A server. Nothing to configure — the itsmini base URL and the cache
    /// directory are read by `ocr` at call time, so a long-lived server picks up an
    /// itsmini that moved without being restarted.
    pub const fn new() -> Self {
        Self
    }

    /// Read frames until the client closes the pipe, answering each one.
    ///
    /// Exits `Ok` at end of input: a client that closes stdin has finished, and
    /// that is not a failure. A frame that cannot be parsed is answered with a
    /// JSON-RPC error and the loop continues — one bad line must never end a
    /// session that could otherwise keep serving.
    pub async fn run<R, W>(&self, reader: &mut R, writer: &mut W) -> Result<()>
    where
        R: AsyncBufRead + Unpin + ?Sized,
        W: AsyncWrite + Unpin + ?Sized,
    {
        let mut buffer: Vec<u8> = Vec::new();

        loop {
            buffer.clear();
            let read = reader.read_until(b'\n', &mut buffer).await?;
            if read == 0 {
                return Ok(());
            }

            if buffer.len() > MAX_FRAME_BYTES {
                let response = error_response(
                    Value::Null,
                    PARSE_ERROR,
                    &format!("frame larger than {MAX_FRAME_BYTES} bytes — refusing to parse it"),
                );
                self.write(writer, &response).await?;
                continue;
            }

            // Read as bytes and decode explicitly: `read_line` turns a non-UTF-8
            // frame into an I/O error, which would end the session over one bad
            // byte. A frame that cannot be decoded is a parse error like any other.
            let response = match std::str::from_utf8(&buffer) {
                Err(_) => error_response(
                    Value::Null,
                    PARSE_ERROR,
                    "frame is not valid UTF-8, so it cannot be a JSON-RPC request",
                ),
                Ok(text) => {
                    // Blank lines are not frames: a client is free to write one,
                    // and there is nothing to answer.
                    let text = text.trim();
                    if text.is_empty() {
                        continue;
                    }
                    match Request::parse(text) {
                        Ok(request) => match self.dispatch(&request).await {
                            Some(response) => response,
                            None => continue,
                        },
                        Err(response) => response,
                    }
                }
            };

            self.write(writer, &response).await?;
        }
    }

    /// Answer one parsed request, or nothing at all for a notification.
    async fn dispatch(&self, request: &Request) -> Option<Value> {
        // A frame with no `id` is a notification, and JSON-RPC forbids replying to
        // one. The `?` is doing the work: a frame without an id ends the call here,
        // before the method is even examined, so an unknown method that arrives as
        // a notification is silent too — a client that sent a notification must
        // not receive a frame it is not waiting for.
        request.id.as_ref()?;

        match request.action() {
            Action::Ignore => None,
            Action::Reply(response) => Some(response),
            Action::Invoke { arguments } => {
                Some(self.call_tool(request.reply_id(), &arguments).await)
            }
        }
    }

    /// Invoke the advertised tool and shape the answer as an MCP tool result.
    ///
    /// A conversion failure is a **result with `isError`**, not a JSON-RPC error:
    /// the protocol distinguishes "your request was malformed" from "the tool ran
    /// and failed", and a caller that cannot tell those apart has to treat every
    /// tool failure as a broken connection. The text is the same stated error
    /// `pie ocr` prints, so the fix — start itsmini, or point
    /// `PIE_ITSMINI_URL` at it — is in the message either way.
    async fn call_tool(&self, id: Value, arguments: &ToolArguments) -> Value {
        let outcome = ocr::convert(OcrRequest {
            file: &arguments.file,
            lang: arguments.lang.as_deref(),
            output: arguments.output.as_deref(),
            // Not exposed over MCP, deliberately: a cache the *caller* can bypass
            // is a cache that costs a conversion every time a model asks twice.
            refresh: false,
            // `None` on both: the server reads itsmini's address from the
            // environment at call time and uses pie's own cache directory, so what
            // the user configured is what runs.
            base_url: None,
            cache_dir: None,
        })
        .await;

        match outcome {
            Ok(result) => {
                tracing::debug!(
                    pages = result.pages,
                    cached = result.cached,
                    chars = result.text.chars().count(),
                    "pie mcp ocr: converted"
                );
                // The text is the whole answer and lands in the first content item
                // unadorned, so a caller can take `content[0].text` and write it to
                // a file. The page count and the cache flag go to the log instead
                // of into the text: they would be indistinguishable from the
                // document's own words.
                ok_response(
                    id,
                    json!({
                        "content": [{"type": "text", "text": result.text}],
                    }),
                )
            }
            Err(error) => {
                let message = truncate(&process::redact(&error.to_string()), MAX_TOOL_ERROR_CHARS);
                tracing::debug!(error = %message, "pie mcp ocr: conversion failed");
                ok_response(
                    id,
                    json!({
                        "content": [{"type": "text", "text": message}],
                        "isError": true,
                    }),
                )
            }
        }
    }

    /// One frame, one line, flushed.
    ///
    /// **The only place anything is written to stdout.** Every other diagnostic on
    /// this path goes to stderr through `tracing`; a `println!` anywhere under
    /// `serve` would put a non-frame on the wire and break the client's parser.
    async fn write<W>(&self, writer: &mut W, response: &Value) -> Result<()>
    where
        W: AsyncWrite + Unpin + ?Sized,
    {
        let frame = serde_json::to_string(response)?;
        writer.write_all(frame.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        // Flushed per frame, never buffered until exit: a client that has sent a
        // request is blocked waiting for this line, and a server whose answer is
        // still in its buffer is indistinguishable from a hung one.
        writer.flush().await?;
        Ok(())
    }
}

/// One parsed JSON-RPC request.
#[derive(Debug, Clone)]
struct Request {
    /// `None` for a notification. `Some(Value::Null)` is a request that named a
    /// null id — still a request, and still owed a reply, because a client that
    /// wrote an `id` is waiting for it.
    id: Option<Value>,
    /// The method name.
    method: String,
    /// Method parameters, `{}` when absent.
    params: Value,
}

impl Request {
    /// Parse one frame, or produce the response that says why it could not be.
    ///
    /// Returns `Err(response)` rather than a Rust error because the reply depends
    /// on what was parseable: a frame whose `id` survived is answered against that
    /// id, and one where it did not is answered with a null id, as JSON-RPC
    /// requires.
    fn parse(frame: &str) -> std::result::Result<Self, Value> {
        let value: Value = serde_json::from_str(frame).map_err(|error| {
            error_response(
                Value::Null,
                PARSE_ERROR,
                &format!("not a JSON-RPC frame: {error}"),
            )
        })?;
        Self::from_value(value)
    }

    /// Validate a decoded frame.
    fn from_value(value: Value) -> std::result::Result<Self, Value> {
        let Value::Object(mut object) = value else {
            return Err(error_response(
                Value::Null,
                INVALID_REQUEST,
                "a JSON-RPC frame must be an object",
            ));
        };

        let id = match object.remove("id") {
            None => None,
            Some(Value::Null) => Some(Value::Null),
            Some(found @ (Value::Number(_) | Value::String(_))) => Some(found),
            Some(_) => {
                return Err(error_response(
                    Value::Null,
                    INVALID_REQUEST,
                    "`id` must be a string or a number",
                ))
            }
        };

        // An `id` that survived is the one to answer against, so every rejection
        // from here on names it rather than going out as a null-id frame.
        let refuse = |message: &str| {
            error_response(id.clone().unwrap_or_default(), INVALID_REQUEST, message)
        };

        // Tolerant by design: a missing `jsonrpc` field is accepted, because the
        // version is not needed to answer the frame and refusing it would end a
        // session over a formality. A *wrong* version is refused, because that is a
        // client disagreeing about the protocol.
        match object.remove("jsonrpc") {
            None => {}
            Some(Value::String(version)) if version == JSONRPC => {}
            Some(_) => return Err(refuse("`jsonrpc` must be `\"2.0\"`")),
        }

        let method = match object.remove("method") {
            Some(Value::String(method)) if !method.trim().is_empty() => method,
            _ => return Err(refuse("`method` must be a non-empty string")),
        };

        let params = match object.remove("params") {
            None | Some(Value::Null) => Value::Object(Map::new()),
            Some(params @ Value::Object(_)) => params,
            Some(_) => return Err(refuse("`params` must be an object")),
        };

        Ok(Self { id, method, params })
    }

    /// The id to answer against; `null` when the frame is a request with no id.
    fn reply_id(&self) -> Value {
        self.id.clone().unwrap_or_default()
    }

    /// What this request asks for, without performing it.
    ///
    /// Split from the I/O so every decision the server makes is reachable from a
    /// unit test: only [`Action::Invoke`] touches the network.
    fn action(&self) -> Action {
        match self.method.as_str() {
            "initialize" => Action::Reply(ok_response(
                self.reply_id(),
                initialize_result(&self.params),
            )),
            "tools/list" => Action::Reply(ok_response(self.reply_id(), tools_list_result())),
            "tools/call" => self.tool_call_action(),
            // A ping is a liveness probe, not a tool and not a capability: it is
            // answered so a client's health check does not read a live server as a
            // dead one. `listChanged` stays absent — the catalog never changes.
            "ping" => Action::Reply(ok_response(self.reply_id(), json!({}))),
            // Protocol notifications are honoured by being ignored. No reply is
            // sent, and none is expected.
            _ if self.method.starts_with("notifications/") => Action::Ignore,
            other => Action::Reply(error_response(
                self.reply_id(),
                METHOD_NOT_FOUND,
                &format!("unknown method `{other}` — this server implements initialize, tools/list, tools/call and ping"),
            )),
        }
    }

    /// Validate a `tools/call` far enough to know which tool and with what.
    fn tool_call_action(&self) -> Action {
        let id = self.reply_id();

        let name = match self.params.get("name") {
            Some(Value::String(name)) => name.clone(),
            _ => {
                return Action::Reply(error_response(
                    id,
                    INVALID_PARAMS,
                    "`tools/call` requires a `name`",
                ))
            }
        };

        if name != TOOL_OCR {
            // Naming the tool that was asked for matters: a model that hallucinated
            // a tool name reads this error and can correct itself, while a bare
            // "invalid params" gives it nothing to act on.
            return Action::Reply(error_response(
                id,
                INVALID_PARAMS,
                &format!(
                    "unknown tool `{name}` — this server advertises exactly one tool: `{TOOL_OCR}`"
                ),
            ));
        }

        let arguments = self.params.get("arguments").cloned().unwrap_or_default();
        match ToolArguments::parse(&arguments) {
            Ok(arguments) => Action::Invoke { arguments },
            Err(problem) => Action::Reply(error_response(id, INVALID_PARAMS, &problem)),
        }
    }
}

/// What a request asks the server to do.
#[derive(Debug, Clone)]
enum Action {
    /// Nothing, and no reply — a notification.
    Ignore,
    /// A complete response, produced without I/O.
    Reply(Value),
    /// Invoke the advertised tool. The only variant that does any work.
    Invoke { arguments: ToolArguments },
}

/// The `tools/call` arguments for `ocr`, validated.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ToolArguments {
    /// The image or scanned PDF to convert.
    file: PathBuf,
    /// Language hint, `None` when absent or blank.
    lang: Option<String>,
    /// Output format, `None` when absent or blank — `ocr` defaults it.
    output: Option<String>,
}

impl ToolArguments {
    /// Validate the arguments object.
    ///
    /// Strict about *shape* and about unknown keys: a mistyped `fileName` silently
    /// ignored would look to the caller like a tool that read the wrong file, and
    /// an error naming the key is the one answer that fixes it. Blank optional
    /// strings are treated as absent, matching what `pie ocr` does with
    /// `--lang ""`.
    fn parse(arguments: &Value) -> std::result::Result<Self, String> {
        let Value::Object(object) = arguments else {
            return Err("`arguments` must be an object".to_string());
        };

        let mut file: Option<String> = None;
        let mut lang: Option<String> = None;
        let mut output: Option<String> = None;

        for (key, value) in object {
            let slot = match key.as_str() {
                "file" => &mut file,
                "lang" => &mut lang,
                "output" => &mut output,
                other => {
                    return Err(format!(
                    "unknown argument `{other}` — `{TOOL_OCR}` takes file (required), lang, output"
                ))
                }
            };
            let Value::String(text) = value else {
                return Err(format!("`{key}` must be a string"));
            };
            *slot = Some(text.clone());
        }

        let file = file.filter(|path| !path.trim().is_empty()).ok_or_else(|| {
            format!("`{TOOL_OCR}` requires `file`: the image or scanned PDF to convert")
        })?;

        Ok(Self {
            file: PathBuf::from(file.trim()),
            lang: lang.filter(|value| !value.trim().is_empty()),
            output: output.filter(|value| !value.trim().is_empty()),
        })
    }
}

/// A successful JSON-RPC response.
fn ok_response(id: Value, result: Value) -> Value {
    json!({"jsonrpc": JSONRPC, "id": id, "result": result})
}

/// A JSON-RPC error response.
fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": JSONRPC,
        "id": id,
        "error": {"code": code, "message": message},
    })
}

/// The `initialize` result: version, capabilities and server identity.
fn initialize_result(params: &Value) -> Value {
    let requested = params.get("protocolVersion").and_then(Value::as_str);

    // Echo a version we speak, because agreeing is what lets the client proceed;
    // answer with our own otherwise, so a client that knows a version we support
    // and a client that knows none both get something they can act on.
    let version = requested
        .filter(|version| SUPPORTED_PROTOCOL_VERSIONS.contains(version))
        .unwrap_or(PROTOCOL_VERSION);

    json!({
        "protocolVersion": version,
        "capabilities": {"tools": {}},
        "serverInfo": {"name": NAME, "version": VERSION},
    })
}

/// The `tools/list` result: **exactly one** tool, `ocr`.
fn tools_list_result() -> Value {
    json!({"tools": [ocr_tool()]})
}

/// The `ocr` tool definition — the whole catalog.
///
/// Minimal on purpose: a required `file`, the two optional parameters `pie ocr`
/// already accepts, and a description under
/// [`MAX_TOOL_DESCRIPTION_CHARS`]. Every field here is re-sent to the model on
/// every turn of the session, which is the cost this server exists to make
/// opt-in, so nothing decorative goes in — no examples, no `annotations`, no
/// second tool.
fn ocr_tool() -> Value {
    json!({
        "name": TOOL_OCR,
        "description": OCR_TOOL_DESCRIPTION,
        "inputSchema": {
            "type": "object",
            "properties": {
                "file": {
                    "type": "string",
                    "description": "Path to the image or scanned PDF to convert.",
                },
                "lang": {
                    "type": "string",
                    "description": "Language hint, e.g. `en` or `ch` (optional).",
                },
                "output": {
                    "type": "string",
                    "description": "Output format itsmini should produce, default `text` (optional).",
                },
            },
            "required": ["file"],
            "additionalProperties": false,
        },
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    /// Parse a frame straight to its action, as the request loop would.
    fn action_of(frame: &str) -> Action {
        Request::parse(frame).expect("the frame parses").action()
    }

    /// Pull the response out of an action that must be a reply.
    fn reply(frame: &str) -> Value {
        match action_of(frame) {
            Action::Reply(response) => response,
            other => panic!("expected a reply, got {other:?}"),
        }
    }

    fn call(arguments: Value) -> String {
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {"name": TOOL_OCR, "arguments": arguments},
        })
        .to_string()
    }

    #[test]
    fn the_catalog_holds_exactly_one_tool_and_it_is_ocr() {
        let result = reply(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#);
        let tools = result["result"]["tools"].as_array().unwrap();

        // The count, not membership: a second tool is the failure this asserts.
        assert_eq!(tools.len(), 1, "one tool, ever: {tools:?}");
        assert_eq!(tools[0]["name"], TOOL_OCR);
    }

    #[test]
    fn the_advertised_schema_is_minimal_and_says_what_is_required() {
        let tool = reply(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#);
        let tool = &tool["result"]["tools"][0];

        assert_eq!(tool["inputSchema"]["required"], json!(["file"]));
        assert_eq!(tool["inputSchema"]["properties"]["file"]["type"], "string");

        let properties = tool["inputSchema"]["properties"].as_object().unwrap();
        assert_eq!(properties.len(), 3, "{properties:?}");
        assert!(properties.contains_key("lang") && properties.contains_key("output"));

        let description = tool["description"].as_str().unwrap();
        assert!(!description.trim().is_empty());
        assert!(
            description.chars().count() <= MAX_TOOL_DESCRIPTION_CHARS,
            "every advertised character is billed on every turn: {description}"
        );
        assert!(OCR_TOOL_DESCRIPTION.chars().count() <= MAX_TOOL_DESCRIPTION_CHARS);
    }

    #[test]
    fn a_protocol_version_we_speak_is_echoed_back() {
        let frame = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {"protocolVersion": "2024-11-05"},
        })
        .to_string();
        let response = reply(&frame);

        assert_eq!(response["result"]["protocolVersion"], "2024-11-05");
        assert_eq!(response["result"]["serverInfo"]["name"], NAME);
        assert!(!response["result"]["serverInfo"]["version"]
            .as_str()
            .unwrap()
            .is_empty());
        assert!(
            response["result"]["capabilities"]["tools"].is_object(),
            "the one capability we have must be declared: {response}"
        );
    }

    #[test]
    fn a_version_we_do_not_speak_gets_ours_rather_than_silence() {
        let frame = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {"protocolVersion": "1999-01-01"},
        })
        .to_string();

        assert_eq!(reply(&frame)["result"]["protocolVersion"], PROTOCOL_VERSION);
        assert!(SUPPORTED_PROTOCOL_VERSIONS.contains(&PROTOCOL_VERSION));
    }

    #[test]
    fn an_unknown_method_is_an_error_naming_it() {
        let response = reply(r#"{"jsonrpc":"2.0","id":7,"method":"resources/list"}"#);

        assert_eq!(response["id"], 7);
        assert_eq!(response["error"]["code"], METHOD_NOT_FOUND);
        assert!(response["error"]["message"]
            .as_str()
            .unwrap()
            .contains("resources/list"));
        assert!(response.get("result").is_none());
    }

    #[test]
    fn an_unknown_tool_is_an_error_naming_the_tool() {
        let frame = json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "tools/call",
            "params": {"name": "rm", "arguments": {"file": "x"}},
        })
        .to_string();
        let response = reply(&frame);

        assert_eq!(response["error"]["code"], INVALID_PARAMS);
        let message = response["error"]["message"].as_str().unwrap();
        assert!(message.contains("rm"), "{message}");
        assert!(message.contains(TOOL_OCR), "{message}");
    }

    #[test]
    fn a_call_without_a_usable_file_is_an_error_rather_than_a_conversion() {
        for arguments in [json!({}), json!({"file": ""}), json!({"file": 3})] {
            match action_of(&call(arguments.clone())) {
                Action::Reply(response) => {
                    assert_eq!(response["error"]["code"], INVALID_PARAMS, "{arguments}");
                }
                other => panic!("{arguments} must not reach itsmini: {other:?}"),
            }
        }
    }

    #[test]
    fn a_tool_call_validates_its_arguments() {
        match action_of(&call(json!({"file": " scan.png ", "lang": "ch"}))) {
            Action::Invoke { arguments } => {
                assert_eq!(arguments.file, PathBuf::from("scan.png"));
                assert_eq!(arguments.lang.as_deref(), Some("ch"));
                assert_eq!(arguments.output, None);
            }
            other => panic!("expected an invocation, got {other:?}"),
        }

        match ToolArguments::parse(&json!({"file": "x", "file_name": "y"})) {
            Err(problem) => assert!(problem.contains("file_name"), "{problem}"),
            Ok(arguments) => panic!("a mistyped key must not be ignored: {arguments:?}"),
        }
    }

    #[test]
    fn a_notification_has_no_id_and_therefore_no_reply() {
        let request =
            Request::parse(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).unwrap();

        assert!(request.id.is_none());
        assert!(matches!(request.action(), Action::Ignore));
    }

    #[test]
    fn the_frame_guards_reject_what_cannot_be_a_request() {
        let parse_error = Request::parse("{not json").unwrap_err();
        assert_eq!(parse_error["error"]["code"], PARSE_ERROR);
        assert!(parse_error["id"].is_null(), "a parse error has a null id");

        let invalid = Request::parse(r#"{"jsonrpc":"2.0"}"#).unwrap_err();
        assert_eq!(invalid["error"]["code"], INVALID_REQUEST);

        // An id that survived the frame is the id the error is answered against.
        let invalid =
            Request::parse(r#"{"jsonrpc":"3.0","id":9,"method":"tools/list"}"#).unwrap_err();
        assert_eq!(invalid["id"], 9);
    }

    #[test]
    fn a_missing_jsonrpc_field_is_tolerated_but_a_wrong_one_is_not() {
        assert!(Request::parse(r#"{"id":1,"method":"tools/list"}"#).is_ok());
        assert!(Request::parse(r#"{"jsonrpc":"1.0","id":1,"method":"tools/list"}"#).is_err());
    }
}
