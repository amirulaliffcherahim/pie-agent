//! Gate G4 for M5 stage E: `pie mcp ocr`, driven over **real stdio**.
//!
//! The stage's acceptance line (`ACCEPTANCE.md` §8) is a handshake: `initialize`,
//! then `tools/list` returning exactly one tool named `ocr`. What is verified here
//! is the binary's behaviour as a client would meet it — the built `pie` is spawned,
//! frames are written to its stdin and read from its stdout, and nothing in the
//! server's implementation is called directly. A unit test of `Server::run` would
//! prove the logic; this file proves the *transport*, which is where an MCP server
//! actually fails.
//!
//! Four things are deliberately asserted rather than assumed:
//!
//! * **Exactly one tool**, by count. Membership would pass with a second tool
//!   bundled alongside, and a second advertised tool is the failure the milestone
//!   exists to prevent — it costs the vendor input tokens on every turn, including
//!   turns that never call anything.
//! * **An itsmini failure is a stated error**, never a crash and never an empty
//!   success: the unreachable case is the only one that is testable while itsmini
//!   is unbuilt (`RISKS.md` §11), so it is tested properly.
//! * **A malformed line does not kill the server.** One bad frame is answered and
//!   the next request is still served.
//! * **stdout carries protocol frames and nothing else** — every line the server
//!   ever wrote is parsed, in every test that reads it.
//!
//! The one success path that *is* reachable is a loopback stand-in for itsmini: the
//! same stub pattern as `tests/ocr.rs`, in a different test binary because the
//! stage has its own gate. No test here touches the real network: the stub binds
//! `127.0.0.1:0`, and the unreachable case is a loopback port with nothing on it.
//!
//! **Not verifiable here**: a real itsmini (it does not exist yet) and a real
//! vendor CLI consuming this server — agy, codex and claude are not spawned and no
//! model turn is taken. What a vendor needs in its own config is documented beside
//! the gate, not claimed by it.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use serde_json::{json, Value};
use tempfile::TempDir;

use pie::mcp::{PROTOCOL_VERSION, SUPPORTED_PROTOCOL_VERSIONS, TOOL_OCR};

/// How long one frame may take before the test calls it a hang.
///
/// Generous on purpose: the failure this must not produce is a flaky test on a
/// loaded machine. The unreachable-itsmini case is refused by the OS immediately, so
/// nothing here waits out `ocr`'s own 120 s deadline.
const FRAME_TIMEOUT: Duration = Duration::from_secs(30);

/// A response in the documented itsmini shape. `{n}` becomes the request's
/// ordinal, so a test can tell a fresh conversion from a cached one.
const RECORDED: &str = r#"{"text":"RECORDED MCP TEXT {n}","pages":1,"confidence":0.97}"#;

/// Stand-in image bytes. The stub never looks at them; `ocr` hashes the file.
const IMAGE: &[u8] = b"\x89PNG\r\n\x1a\nMCP-IMAGE-BYTES";

// ------------------------------------------------------------------ the server --

/// A running `pie mcp ocr`, with its stdout drained on a thread.
///
/// Drained on a thread rather than read inline because a test must be able to wait
/// for a frame *with a deadline*: a server that stopped answering has to fail the
/// test rather than hang the suite. The thread also keeps the pipe empty, so a
/// stderr warning cannot wedge the child.
struct Session {
    child: Child,
    stdin: Option<ChildStdin>,
    frames: Receiver<String>,
    seen: Arc<Mutex<Vec<String>>>,
    stderr: Arc<Mutex<String>>,
    reader: Option<JoinHandle<()>>,
    drain: Option<JoinHandle<()>>,
    home: TempDir,
}

impl Session {
    /// Start the server pointed at `itsmini_url`, with its own `PIE_HOME`.
    ///
    /// The temporary `PIE_HOME` is what keeps the OCR cache out of the developer's
    /// real `~/.pie` and what makes the cache assertion in the stub test
    /// meaningful: an empty cache directory belongs to this test alone.
    fn start(itsmini_url: &str) -> Self {
        let home = tempfile::tempdir().expect("a temporary PIE_HOME");

        let mut child = Command::new(env!("CARGO_BIN_EXE_pie"))
            .args(["mcp", "ocr"])
            .env("PIE_HOME", home.path())
            .env("PIE_ITSMINI_URL", itsmini_url)
            .env("PIE_LOG", "warn")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to spawn `pie mcp ocr`");

        let stdin = child.stdin.take().expect("stdin pipe");
        let stdout = child.stdout.take().expect("stdout pipe");
        let stderr = child.stderr.take().expect("stderr pipe");

        let (sender, frames) = mpsc::channel();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let reader = {
            let seen = Arc::clone(&seen);
            std::thread::spawn(move || {
                for line in BufReader::new(stdout).lines() {
                    // A read error ends the thread; the test then times out on the
                    // frame it was waiting for, which is the failure it wants to
                    // see rather than a panic on a helper thread.
                    let Ok(line) = line else { return };
                    if let Ok(mut log) = seen.lock() {
                        log.push(line.clone());
                    }
                    if sender.send(line).is_err() {
                        return;
                    }
                }
            })
        };

        let stderr_text = Arc::new(Mutex::new(String::new()));
        let drain = {
            let stderr_text = Arc::clone(&stderr_text);
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stderr);
                let mut buffer = Vec::new();
                if reader.read_to_end(&mut buffer).is_ok() {
                    if let Ok(mut log) = stderr_text.lock() {
                        log.push_str(&String::from_utf8_lossy(&buffer));
                    }
                }
            })
        };

        Self {
            child,
            stdin: Some(stdin),
            frames,
            seen,
            stderr: stderr_text,
            reader: Some(reader),
            drain: Some(drain),
            home,
        }
    }

    /// The server's `PIE_HOME`, where the OCR cache lands.
    fn home(&self) -> &Path {
        self.home.path()
    }

    /// Everything the server has written to stderr so far, for a failure message.
    fn stderr_text(&self) -> String {
        self.stderr
            .lock()
            .map(|log| log.clone())
            .unwrap_or_default()
    }

    /// Every line the server has written to stdout, in order.
    fn seen(&self) -> Vec<String> {
        self.seen.lock().map(|log| log.clone()).unwrap_or_default()
    }

    /// Write raw bytes as one frame. Used to send input that is not JSON at all.
    fn send_bytes(&mut self, bytes: &[u8]) {
        let stdin = self.stdin.as_mut().expect("the server's stdin is open");
        stdin.write_all(bytes).expect("write to the server");
        stdin.write_all(b"\n").expect("write the frame terminator");
        stdin.flush().expect("flush the frame");
    }

    /// Write one JSON frame.
    fn send(&mut self, frame: &Value) {
        let line = serde_json::to_vec(frame).expect("serialize a frame");
        self.send_bytes(&line);
    }

    /// The next frame the server writes, or a failure naming what it said on
    /// stderr.
    fn recv_frame(&mut self) -> Value {
        let line = match self.frames.recv_timeout(FRAME_TIMEOUT) {
            Ok(line) => line,
            Err(RecvTimeoutError::Timeout) => panic!(
                "no frame within {FRAME_TIMEOUT:?}. stdout so far: {:?}. stderr: {}",
                self.seen(),
                self.stderr_text()
            ),
            Err(RecvTimeoutError::Disconnected) => {
                panic!(
                    "the server closed stdout. stdout so far: {:?}. stderr: {}",
                    self.seen(),
                    self.stderr_text()
                )
            }
        };
        serde_json::from_str(&line).unwrap_or_else(|error| {
            panic!("stdout carried a line that is not a protocol frame: {line:?} ({error})")
        })
    }

    /// Send a request and return its response, asserting the id round-trips.
    ///
    /// The id check is what makes the rest of these assertions mean anything: a
    /// response that answers a different request, or a stray frame from somewhere
    /// else on stdout, would otherwise pass as the answer to this one.
    fn ask(&mut self, id: Value, method: &str, params: Value) -> Value {
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }));

        let response = self.recv_frame();
        assert_eq!(response["id"], id, "an answer must carry the id it answers");
        assert_eq!(response["jsonrpc"], "2.0");
        response
    }

    /// `initialize`, answering with the same version the server answered with.
    fn handshake(&mut self) -> Value {
        let response = self.ask(
            json!(1),
            "initialize",
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": "tests/mcp.rs", "version": "0"},
            }),
        );
        assert!(response.get("error").is_none(), "{response}");
        response
    }

    /// A `tools/call` for `ocr`.
    fn call_ocr(&mut self, id: Value, file: &Path) -> Value {
        self.ask(
            id,
            "tools/call",
            json!({"name": TOOL_OCR, "arguments": {"file": file.display().to_string()}}),
        )
    }

    /// Write the stand-in image into this session's `PIE_HOME`.
    fn image(&self) -> PathBuf {
        let path = self.home().join("scan.png");
        std::fs::write(&path, IMAGE).expect("write the stand-in image");
        path
    }

    /// Every stdout line must be a JSON object, and every object a JSON-RPC frame.
    ///
    /// This is the assertion behind "stdout carries only protocol frames": a
    /// `println!` anywhere on the server's path would land here as a line that does
    /// not parse, which is exactly how a client would fail.
    fn assert_stdout_is_protocol_only(&self) {
        for line in self.seen() {
            let value: Value = serde_json::from_str(&line)
                .unwrap_or_else(|error| panic!("non-frame line on stdout: {line:?} ({error})"));
            assert_eq!(value["jsonrpc"], "2.0", "not a JSON-RPC frame: {line:?}");
        }
    }

    /// The server is still running, not exited.
    fn assert_still_running(&mut self) {
        let status = self.child.try_wait().expect("query the child");
        assert!(
            status.is_none(),
            "the server exited: {status:?} (stderr: {})",
            self.stderr_text()
        );
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Close stdin first: the server exits cleanly at end of input, which is the
        // path a real client takes. The kill is the backstop for a wedged server.
        drop(self.stdin.take());
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(thread) = self.reader.take() {
            let _ = thread.join();
        }
        if let Some(thread) = self.drain.take() {
            let _ = thread.join();
        }
    }
}

/// A base URL nothing is listening on.
///
/// A just-released loopback port, so the connection is refused at once rather than
/// waiting out `ocr`'s 120 s deadline. `192.0.2.1` would be unroutable in the
/// intended sense and would take the whole timeout to say so.
fn unreachable_base_url() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("reserve a loopback port");
    let addr = listener.local_addr().expect("read the reserved address");
    drop(listener);
    format!("http://{addr}")
}

// ----------------------------------------------------------------- the itsmini --

/// A loopback server that answers the frozen itsmini contract.
///
/// A plain `std` thread rather than `tokio`: the test process blocks on a child
/// process, and a stub sharing that runtime could not answer while the block was in
/// progress. It records what it was asked, which is how the cache assertion below
/// can tell a second call was served without a second request.
struct ItsminiStub {
    base_url: String,
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl ItsminiStub {
    /// Serve `body` on an ephemeral loopback port.
    fn start(body: &'static str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind the itsmini stand-in");
        let addr = listener.local_addr().expect("read the stand-in address");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));

        let thread = {
            let requests = Arc::clone(&requests);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                for connection in listener.incoming() {
                    // The connection `Drop` opens exists only to get us here, so a
                    // set flag means "stop" rather than "answer".
                    if stop.load(Ordering::SeqCst) {
                        return;
                    }
                    match connection {
                        Ok(mut socket) => {
                            if let Some(body) = answer(&mut socket, body, &requests) {
                                let _ = socket.write_all(body.as_bytes());
                                let _ = socket.flush();
                            }
                            let _ = socket.shutdown(Shutdown::Both);
                        }
                        Err(_) => return,
                    }
                }
            })
        };

        Self {
            base_url: format!("http://{addr}"),
            addr,
            requests,
            stop,
            thread: Some(thread),
        }
    }

    /// How many requests the stand-in received.
    fn request_count(&self) -> usize {
        self.requests
            .lock()
            .map(|log| log.len())
            .unwrap_or_default()
    }

    /// The body of request `index`, as the stand-in read it.
    fn body(&self, index: usize) -> String {
        self.requests
            .lock()
            .map(|log| log.get(index).cloned().unwrap_or_default())
            .unwrap_or_default()
    }
}

impl Drop for ItsminiStub {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Unblock the accept loop so the flag can be observed.
        let _ = TcpStream::connect(self.addr);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Read one HTTP request, record its body, and build the response to write.
///
/// `{n}` in `body` becomes the request's ordinal, so the first answer and the
/// second are distinguishable — which is what proves a cached call returned the
/// *stored* text rather than a fresh conversion.
fn answer(socket: &mut TcpStream, body: &str, requests: &Mutex<Vec<String>>) -> Option<String> {
    // A wedged test must fail rather than hang, so a mistake on either side of the
    // socket turns into a timeout instead of a deadlock.
    let _ = socket.set_read_timeout(Some(Duration::from_secs(10)));
    let _ = socket.set_write_timeout(Some(Duration::from_secs(10)));

    let mut reader = BufReader::new(&mut *socket);
    let mut head = String::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            return None;
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        head.push_str(&line);
        if head.len() > 64 * 1024 {
            return None;
        }
    }

    let length: usize = head
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse().ok())
        .unwrap_or(0);

    let mut raw = vec![0u8; length];
    if length > 0 {
        reader.read_exact(&mut raw).ok()?;
    }

    let ordinal = {
        let mut log = requests.lock().ok()?;
        log.push(String::from_utf8_lossy(&raw).into_owned());
        log.len()
    };

    let body = body.replace("{n}", &ordinal.to_string());
    Some(format!(
        "HTTP/1.1 200 OK\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        body.len(),
    ))
}

/// The `text` of a tool result's first content item.
fn content_text(response: &Value) -> String {
    response["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("no text content in {response}"))
        .to_string()
}

// ---------------------------------------------------------------- the handshake --

#[test]
fn initialize_answers_with_our_identity_and_the_version_it_was_asked_for() {
    let mut session = Session::start(&unreachable_base_url());

    let response = session.ask(
        json!(1),
        "initialize",
        json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {"name": "tests/mcp.rs", "version": "0"},
        }),
    );

    assert!(response.get("error").is_none(), "{response}");
    assert_eq!(response["result"]["protocolVersion"], PROTOCOL_VERSION);
    assert_eq!(response["result"]["serverInfo"]["name"], "pie");
    assert!(
        !response["result"]["serverInfo"]["version"]
            .as_str()
            .unwrap_or_default()
            .is_empty(),
        "a client that cannot tell which build answered cannot report a bug: {response}"
    );
    assert!(
        response["result"]["capabilities"]["tools"].is_object(),
        "the tools capability must be declared, or a client may never call tools/list: {response}"
    );

    session.assert_stdout_is_protocol_only();
}

#[test]
fn a_version_we_do_not_speak_is_answered_with_one_we_do() {
    let mut session = Session::start(&unreachable_base_url());

    let response = session.ask(
        json!("init-2"),
        "initialize",
        json!({"protocolVersion": "1999-01-01", "capabilities": {}}),
    );

    let answered = response["result"]["protocolVersion"]
        .as_str()
        .unwrap_or_default();
    assert!(
        SUPPORTED_PROTOCOL_VERSIONS.contains(&answered),
        "the answer must be a version we actually speak, not silence and not a guess: {response}"
    );
}

// --------------------------------------------------------------- the one tool --

#[test]
fn tools_list_advertises_exactly_one_tool_and_it_is_ocr() {
    let mut session = Session::start(&unreachable_base_url());
    session.handshake();

    let response = session.ask(json!(2), "tools/list", json!({}));
    let tools = response["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("`tools` must be an array: {response}"));

    // The count, not membership. A second tool would pass a membership assertion
    // while raising the vendor's per-turn input tokens for the whole session.
    assert_eq!(tools.len(), 1, "exactly one tool, ever: {tools:?}");

    let tool = &tools[0];
    assert_eq!(tool["name"], TOOL_OCR);
    assert_eq!(tool["inputSchema"]["type"], "object");
    assert_eq!(
        tool["inputSchema"]["required"],
        json!(["file"]),
        "one required argument: {tool}"
    );

    let properties = tool["inputSchema"]["properties"]
        .as_object()
        .unwrap_or_else(|| panic!("`properties` must be an object: {tool}"));
    assert_eq!(properties.len(), 3, "{properties:?}");
    for name in ["file", "lang", "output"] {
        assert_eq!(properties[name]["type"], "string", "{name}: {tool}");
    }

    let description = tool["description"].as_str().unwrap_or_default();
    assert!(!description.trim().is_empty(), "a tool with no description");
    assert!(
        description.chars().count() <= 200,
        "an advertised tool is billed on every turn, so its schema stays minimal: {description}"
    );

    session.assert_stdout_is_protocol_only();
}

// ------------------------------------------------------ the failure that matters --

#[test]
fn a_call_against_an_unreachable_itsmini_is_a_stated_error_and_the_server_serves_on() {
    let itsmini = unreachable_base_url();
    let mut session = Session::start(&itsmini);
    session.handshake();
    let file = session.image();

    let response = session.call_ocr(json!(2), &file);

    // A tool *failure*, not a protocol error: the request was well formed, the
    // conversion could not run. MCP distinguishes the two, and collapsing them
    // would make every itsmini outage look like a broken connection.
    assert!(
        response.get("error").is_none(),
        "a conversion failure is a tool error result, not a JSON-RPC error: {response}"
    );
    assert_eq!(
        response["result"]["isError"],
        json!(true),
        "never an empty success: {response}"
    );

    let message = content_text(&response);
    assert!(!message.trim().is_empty(), "an error result must say why");
    assert!(message.contains("itsmini"), "{message}");
    assert!(
        message.contains(&itsmini),
        "the error must name the URL pie tried: {message}"
    );

    // Not a crash: the same server answers the next request.
    session.assert_still_running();
    let listed = session.ask(json!(3), "tools/list", json!({}));
    assert_eq!(listed["result"]["tools"].as_array().map(Vec::len), Some(1));

    session.assert_stdout_is_protocol_only();
}

// ------------------------------------------------------------- the reachable one --

#[test]
fn a_reachable_itsmini_returns_its_text_as_tool_content_and_only_converts_once() {
    let stub = ItsminiStub::start(RECORDED);
    let mut session = Session::start(&stub.base_url);
    session.handshake();
    let file = session.image();

    let first = session.call_ocr(json!(2), &file);
    assert!(first.get("error").is_none(), "{first}");
    // `isError` is absent on success rather than false, which is what the protocol
    // means by it; either spelling is fine, an error flag is not.
    assert!(
        first["result"].get("isError").is_none() || first["result"]["isError"] == json!(false),
        "a conversion that succeeded must not be flagged as an error: {first}"
    );
    assert_eq!(content_text(&first), "RECORDED MCP TEXT 1");
    assert_eq!(first["result"]["content"][0]["type"], json!("text"));

    // The request the server made follows the frozen contract (`PLAN.md` §10).
    assert_eq!(stub.request_count(), 1);
    let body: Value = serde_json::from_str(&stub.body(0)).expect("the request body is JSON");
    assert_eq!(body["file"], json!(file.display().to_string()));
    assert_eq!(body["output"], json!("text"));
    assert!(
        body.get("lang").is_none(),
        "no language hint means no `lang` key: {body}"
    );

    // The same file again: the text is the *cached* one and no second request was
    // made. This is the whole token argument for the capability (`DESIGN.md` §11),
    // and it holds over MCP exactly as it does for `pie ocr`.
    let second = session.call_ocr(json!(3), &file);
    assert_eq!(content_text(&second), "RECORDED MCP TEXT 1");
    assert_eq!(
        stub.request_count(),
        1,
        "a repeat must be served from the content-hash cache, not converted again"
    );

    session.assert_stdout_is_protocol_only();
}

// ------------------------------------------------------------------ the refusals --

#[test]
fn an_unknown_method_is_a_json_rpc_error_naming_it() {
    let mut session = Session::start(&unreachable_base_url());
    session.handshake();

    let response = session.ask(json!(2), "resources/list", json!({}));

    assert_eq!(response["error"]["code"], json!(-32601));
    assert!(response.get("result").is_none(), "{response}");
    assert!(
        response["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("resources/list"),
        "the error must name the method that was not found: {response}"
    );

    // Still serving afterwards.
    session.assert_still_running();
    session.assert_stdout_is_protocol_only();
}

#[test]
fn a_bad_tool_call_is_a_json_rpc_error_rather_than_a_result() {
    let mut session = Session::start(&unreachable_base_url());
    session.handshake();

    // An unknown tool, named so the caller can correct itself.
    let response = session.ask(
        json!(2),
        "tools/call",
        json!({"name": "rm", "arguments": {"file": "x"}}),
    );
    assert_eq!(response["error"]["code"], json!(-32602));
    assert!(
        response["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("rm"),
        "the unknown tool must be named: {response}"
    );
    assert!(response.get("result").is_none(), "{response}");

    // A missing, blank, or non-string `file`: refused before anything is sent
    // anywhere, so a typo cannot become a request to a file the user did not name.
    for arguments in [json!({}), json!({"file": ""}), json!({"file": 7})] {
        let response = session.ask(
            json!(3),
            "tools/call",
            json!({"name": TOOL_OCR, "arguments": arguments}),
        );
        assert_eq!(response["error"]["code"], json!(-32602), "{arguments}");
        assert!(response.get("result").is_none(), "{response}");
    }

    // An unknown argument key, named.
    let response = session.ask(
        json!(4),
        "tools/call",
        json!({"name": TOOL_OCR, "arguments": {"fileName": "x"}}),
    );
    assert!(
        response["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("fileName"),
        "a silently ignored key would look like a tool that read the wrong file: {response}"
    );

    session.assert_still_running();
    session.assert_stdout_is_protocol_only();
}

#[test]
fn a_malformed_line_is_answered_with_an_error_and_the_server_keeps_serving() {
    let mut session = Session::start(&unreachable_base_url());
    session.handshake();

    // Not JSON at all.
    session.send_bytes(b"{not json");
    let response = session.recv_frame();
    assert_eq!(response["error"]["code"], json!(-32700), "{response}");
    assert!(
        response["id"].is_null(),
        "JSON-RPC requires a null id when the frame could not be parsed: {response}"
    );

    // JSON, but not a request.
    session.send_bytes(br#"{"jsonrpc":"2.0"}"#);
    let response = session.recv_frame();
    assert_eq!(response["error"]["code"], json!(-32600), "{response}");

    // Bytes that are not even text. A `read_line` implementation would die here;
    // the server has to answer instead, because one bad byte is not a dead client.
    session.send_bytes(b"\xff\xfe\xfd not utf-8");
    let response = session.recv_frame();
    assert_eq!(response["error"]["code"], json!(-32700), "{response}");

    // After three bad frames, a real request is still served.
    let listed = session.ask(json!(9), "tools/list", json!({}));
    assert_eq!(listed["result"]["tools"].as_array().map(Vec::len), Some(1));

    session.assert_still_running();
    session.assert_stdout_is_protocol_only();
}

// ------------------------------------------------------------------- the stream --

#[test]
fn nothing_but_protocol_frames_reaches_stdout_and_notifications_get_nothing() {
    let mut session = Session::start(&unreachable_base_url());
    session.handshake();

    // A notification: an id-less frame that JSON-RPC forbids answering. If the
    // server replied, the *next* frame the client reads would be that reply rather
    // than the answer to the request below — which is what this ordering detects.
    session.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    session.send(&json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {}}));

    let ping = session.ask(json!(2), "ping", json!({}));
    assert!(ping.get("error").is_none(), "{ping}");

    session.ask(json!(3), "tools/list", json!({}));
    session.ask(json!(4), "resources/list", json!({}));
    session.send_bytes(b"junk on stdin");
    session.recv_frame();
    session.ask(
        json!(5),
        "tools/call",
        json!({"name": "nope", "arguments": {}}),
    );

    // Six requests above (initialize, ping, tools/list, resources/list, the junk
    // line, and the unknown tool), so six frames: the two notifications produced
    // none, and every line is a JSON-RPC frame. A `println!` on the server's path
    // would show up here as a line that does not parse.
    let seen = session.seen();
    assert_eq!(
        seen.len(),
        6,
        "one frame per request and none for a notification: {seen:?}"
    );
    session.assert_stdout_is_protocol_only();

    // stderr is where diagnostics are allowed to be, and is deliberately not
    // asserted empty: the rule is that stdout is clean, not that the server is
    // mute.
}

// ---------------------------------------------------------------- the subcommand --

#[test]
fn a_capability_pie_does_not_host_is_refused_and_prints_nothing_to_stdout() {
    let home = tempfile::tempdir().expect("a temporary PIE_HOME");
    let output = Command::new(env!("CARGO_BIN_EXE_pie"))
        .args(["mcp", "ocrd"])
        .env("PIE_HOME", home.path())
        .env("PIE_LOG", "warn")
        .stdin(Stdio::null())
        .output()
        .expect("failed to run the pie binary");

    assert_eq!(
        output.status.code(),
        Some(2),
        "a config error, like every other bad argument (stderr: {})",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stdout.is_empty(),
        "stdout is a frame channel even on the way out: {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("ocrd"), "{stderr}");
    assert!(
        stderr.contains(TOOL_OCR),
        "the error must name the capability pie does host: {stderr}"
    );
}
