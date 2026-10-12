//! Gate G4 for M5 stage D: `pie ocr` against a loopback stand-in for itsmini.
//!
//! `ACCEPTANCE.md` §8 is explicit that a real itsmini is **not verifiable here** —
//! itsmini is a separate, unbuilt project. So nothing in this file claims that a
//! conversion works against the real thing. What is verifiable, and what is tested
//! here, is:
//!
//! * the success path, **executed** rather than asserted: a loopback server that
//!   speaks the frozen contract, and its text coming back out of `convert` and out
//!   of the real `pie ocr` binary;
//! * the cache is the token argument for the whole capability, so it is tested
//!   properly: the same *content* under two names is converted once, different
//!   content twice, the key covers the parameters that change the answer, and the
//!   entry lands under `PIE_HOME` holding text and not the image;
//! * the failure path that matters most while itsmini is absent (`RISKS.md` §11):
//!   an unreachable itsmini is a stated error naming itsmini and the URL, never an
//!   empty success and never an empty string;
//! * a non-2xx answer and a 200 with the wrong shape are errors, not empty text;
//! * `pie ocr` spawns no vendor CLI, spends no model quota and writes no session
//!   row — behaviourally for the table, and statically for the source paths.
//!
//! Nothing here reaches off this machine: every server binds `127.0.0.1:0`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use pie::ocr;

/// A response body in the documented shape. `{n}` is replaced by the request's
/// ordinal, so a test can tell one answer from another — which is what proves a
/// cache hit returned the *stored* text rather than a new one.
const RECORDED: &str = r#"{"text":"RECORDED TEXT {n}","pages":2,"confidence":0.93}"#;

/// Stand-in image bytes. Any bytes do: pie hashes the file and the stub decides
/// what it can read. The marker exists so a test can prove the image is not what
/// ended up in the cache.
const IMAGE_A: &[u8] = b"\x89PNG\r\n\x1a\nIMAGE-A-BYTES";
const IMAGE_B: &[u8] = b"\x89PNG\r\n\x1a\nIMAGE-B-BYTES";

// ------------------------------------------------------------- the stand-in --

/// One request the stub received.
#[derive(Debug, Clone)]
struct Recorded {
    method: String,
    path: String,
    body: String,
}

/// A loopback HTTP server that answers the itsmini contract and records what it
/// was asked.
///
/// Deliberately not built on `tokio`'s `TcpListener`: the CLI tests run a child
/// process and block on it, so a stub that shares the test's async runtime could
/// not answer while that block is in progress. A plain `std` thread has no such
/// coupling and needs no change to `Cargo.toml`.
struct Stub {
    base_url: String,
    addr: SocketAddr,
    recorded: Arc<Mutex<Vec<Recorded>>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Stub {
    /// Serve `body` with `status` on an ephemeral loopback port.
    fn start(status: u16, body: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind the loopback stub");
        let addr = listener.local_addr().expect("read the stub's address");
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));

        let thread = {
            let recorded = Arc::clone(&recorded);
            let stop = Arc::clone(&stop);
            let body = body.to_string();
            std::thread::spawn(move || {
                for connection in listener.incoming() {
                    // The probe connection `Drop` opens exists only to get us
                    // here, so a set flag means "stop" rather than "answer".
                    if stop.load(Ordering::SeqCst) {
                        return;
                    }
                    match connection {
                        Ok(mut socket) => handle(&mut socket, status, &body, &recorded),
                        Err(_) => return,
                    }
                }
            })
        };

        Self {
            base_url: format!("http://{addr}"),
            addr,
            recorded,
            stop,
            thread: Some(thread),
        }
    }

    /// What the stub was asked, in order.
    fn recorded(&self) -> Vec<Recorded> {
        self.recorded
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn request_count(&self) -> usize {
        self.recorded().len()
    }
}

impl Drop for Stub {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Unblock the accept loop so the thread can observe the flag and return.
        let _ = TcpStream::connect(self.addr);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Answer one request: read it, record it, reply, close.
fn handle(socket: &mut TcpStream, status: u16, body: &str, recorded: &Mutex<Vec<Recorded>>) {
    // A wedged test must fail rather than hang, so a client or stub mistake that
    // leaves the socket half-open turns into a read timeout.
    let _ = socket.set_read_timeout(Some(Duration::from_secs(10)));
    let _ = socket.set_write_timeout(Some(Duration::from_secs(10)));

    let ordinal = {
        let mut log = recorded
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(request) = read_request(socket) {
            log.push(request);
        }
        log.len()
    };

    let body = body.replace("{n}", &ordinal.to_string());
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        body.len(),
        reason = reason_for(status),
    );

    let _ = socket.write_all(response.as_bytes());
    let _ = socket.flush();
    let _ = socket.shutdown(Shutdown::Both);
}

/// Read one HTTP/1.1 request: the head, then exactly `Content-Length` bytes.
fn read_request(socket: &mut TcpStream) -> Option<Recorded> {
    let mut reader = BufReader::new(socket);
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

    let mut lines = head.lines();
    let mut request_line = lines.next()?.split_whitespace();
    let method = request_line.next()?.to_string();
    let path = request_line.next()?.to_string();

    let length: usize = lines
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse().ok())
        .unwrap_or(0);

    let mut body = vec![0u8; length];
    if length > 0 {
        reader.read_exact(&mut body).ok()?;
    }

    Some(Recorded {
        method,
        path,
        body: String::from_utf8_lossy(&body).into_owned(),
    })
}

/// The reason phrase for a status line. Only the codes the tests use are named.
fn reason_for(status: u16) -> &'static str {
    match status {
        200 => "OK",
        404 => "Not Found",
        500 => "Internal Server Error",
        _ => "Status",
    }
}

// ------------------------------------------------------------------- helpers --

/// A `Request` aimed at `stub`, with its cache pinned to `cache`.
///
/// The cache directory is passed rather than read from `PIE_HOME` because
/// mutating the process environment from a parallel integration test is the race
/// `testing.rs` documents. The child-process tests below cover the real default.
fn request<'a>(file: &'a Path, cache: &'a Path, stub: &'a Stub) -> ocr::Request<'a> {
    ocr::Request {
        base_url: Some(stub.base_url.as_str()),
        cache_dir: Some(cache),
        ..ocr::Request::new(file)
    }
}

fn image(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, bytes).expect("write the stand-in image");
    path
}

/// A base URL nothing is listening on.
///
/// A just-released ephemeral port, so the connection is refused immediately
/// instead of waiting out the deadline. `192.0.2.1` (TEST-NET-1) would be
/// unroutable in the intended sense and would take the full timeout to say so.
fn unreachable_base_url() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("reserve a loopback port");
    let addr = listener.local_addr().expect("read the reserved address");
    drop(listener);
    format!("http://{addr}")
}

/// The files in a directory, sorted. Empty for a directory that does not exist.
fn files_in(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|entries| entries.filter_map(|e| e.ok()).map(|e| e.path()).collect())
        .unwrap_or_default();
    files.sort();
    files
}

/// Run the real binary with its own `PIE_HOME`, pointed at `itsmini_url`.
fn pie(home: &Path, itsmini_url: &str, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_pie"))
        .args(args)
        .env("PIE_HOME", home)
        .env("PIE_ITSMINI_URL", itsmini_url)
        .env("PIE_LOG", "warn")
        .output()
        .expect("failed to run the pie binary")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

// --------------------------------------------------------------- happy path --

/// The success path, executed: a server that speaks the documented contract, and
/// the text it returned reaching the result.
#[tokio::test]
async fn a_reachable_itsmini_is_converted_and_the_request_follows_the_contract() {
    let stub = Stub::start(200, RECORDED);
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("ocr-cache");
    let file = image(dir.path(), "scan.png", IMAGE_A);

    let result = ocr::convert(request(&file, &cache, &stub)).await.unwrap();

    assert_eq!(result.text, "RECORDED TEXT 1");
    assert_eq!(result.pages, 2);
    assert_eq!(result.confidence, Some(0.93));
    assert!(!result.cached, "the first conversion cannot be a cache hit");

    let recorded = stub.recorded();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].method, "POST");
    assert_eq!(recorded[0].path, "/ocr", "the frozen contract's endpoint");

    let body: serde_json::Value = serde_json::from_str(&recorded[0].body).unwrap();
    assert_eq!(body["file"].as_str().unwrap(), file.display().to_string());
    assert_eq!(body["output"].as_str().unwrap(), "text");
    assert!(
        body.get("lang").is_none(),
        "no language hint means no `lang` key: {body}"
    );
}

#[tokio::test]
async fn a_language_hint_is_forwarded_when_it_is_given() {
    let stub = Stub::start(200, RECORDED);
    let dir = tempfile::tempdir().unwrap();
    let file = image(dir.path(), "scan.png", IMAGE_A);

    let result = ocr::convert(ocr::Request {
        lang: Some("ch"),
        ..request(&file, &dir.path().join("ocr-cache"), &stub)
    })
    .await
    .unwrap();

    assert_eq!(result.text, "RECORDED TEXT 1");
    let body: serde_json::Value = serde_json::from_str(&stub.recorded()[0].body).unwrap();
    assert_eq!(body["lang"].as_str().unwrap(), "ch");
}

// ----------------------------------------------------------------- the cache --

/// The token argument: the same image is converted once. Content-addressed, so a
/// copy under a different name is the *same* content and costs nothing.
#[tokio::test]
async fn the_same_content_is_converted_once_however_it_is_named() {
    let stub = Stub::start(200, RECORDED);
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("ocr-cache");
    let first = image(dir.path(), "scan.png", IMAGE_A);
    let copy = image(dir.path(), "renamed-copy.png", IMAGE_A);
    let other = image(dir.path(), "different.png", IMAGE_B);

    let one = ocr::convert(request(&first, &cache, &stub)).await.unwrap();
    let two = ocr::convert(request(&copy, &cache, &stub)).await.unwrap();
    let three = ocr::convert(request(&other, &cache, &stub)).await.unwrap();

    assert_eq!(
        stub.request_count(),
        2,
        "the copy must not cost a request; different content must"
    );
    assert!(!one.cached, "first conversion");
    assert!(
        two.cached,
        "a copy of the same bytes is served from the cache"
    );
    assert!(!three.cached, "different bytes are a different key");
    assert_eq!(one.text, "RECORDED TEXT 1");
    assert_eq!(
        two.text, "RECORDED TEXT 1",
        "the cache returns the stored text, not a fresh answer"
    );
    assert_eq!(three.text, "RECORDED TEXT 2");
}

/// The key must cover what changes the answer. Keying on the bytes alone would
/// serve the English text for a request that asked for Chinese.
#[tokio::test]
async fn the_cache_key_covers_the_parameters_that_change_the_answer() {
    let stub = Stub::start(200, RECORDED);
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("ocr-cache");
    let file = image(dir.path(), "scan.png", IMAGE_A);

    let plain = ocr::convert(request(&file, &cache, &stub)).await.unwrap();
    let hinted = ocr::convert(ocr::Request {
        lang: Some("ch"),
        ..request(&file, &cache, &stub)
    })
    .await
    .unwrap();
    let again = ocr::convert(request(&file, &cache, &stub)).await.unwrap();

    assert_eq!(
        stub.request_count(),
        2,
        "a different --lang is a different question"
    );
    assert_eq!(plain.text, "RECORDED TEXT 1");
    assert_eq!(hinted.text, "RECORDED TEXT 2");
    assert!(again.cached, "and the original is still a hit");
    assert_eq!(again.text, "RECORDED TEXT 1");
}

#[tokio::test]
async fn refresh_ignores_a_cached_answer() {
    let stub = Stub::start(200, RECORDED);
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("ocr-cache");
    let file = image(dir.path(), "scan.png", IMAGE_A);

    let first = ocr::convert(request(&file, &cache, &stub)).await.unwrap();
    let forced = ocr::convert(ocr::Request {
        refresh: true,
        ..request(&file, &cache, &stub)
    })
    .await
    .unwrap();

    assert_eq!(stub.request_count(), 2, "--refresh asks again");
    assert!(!first.cached);
    assert!(!forced.cached);
    assert_eq!(forced.text, "RECORDED TEXT 2");
}

/// The entry holds text and names no file: a cache directory must not become a
/// second index of the user's images.
#[tokio::test]
async fn the_cache_file_holds_the_text_and_not_the_image() {
    let stub = Stub::start(200, RECORDED);
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("ocr-cache");
    let file = image(dir.path(), "scan.png", IMAGE_A);

    ocr::convert(request(&file, &cache, &stub)).await.unwrap();

    let entries = files_in(&cache);
    assert_eq!(entries.len(), 1, "one entry per content key: {entries:?}");

    let name = entries[0]
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let key = name.strip_suffix(".json").unwrap_or(&name);
    assert_eq!(
        key.len(),
        32,
        "the name is the 128-bit content key, not the path: {name}"
    );
    assert!(
        key.chars().all(|c| c.is_ascii_hexdigit()),
        "a key must be a key: {name}"
    );

    let stored = std::fs::read_to_string(&entries[0]).unwrap();
    assert!(stored.contains("RECORDED TEXT 1"), "{stored}");
    assert!(
        !stored.contains("IMAGE-A-BYTES"),
        "the cache stores text, never the image: {stored}"
    );
}

// -------------------------------------------------------------- the failures --

/// `RISKS.md` §11: while itsmini does not exist, this is the behaviour that will
/// actually run. A clear "itsmini not reachable" — never an empty success.
#[tokio::test]
async fn an_unreachable_itsmini_is_a_stated_error_naming_itsmini_and_the_url() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("ocr-cache");
    let file = image(dir.path(), "scan.png", IMAGE_A);
    let url = unreachable_base_url();

    let error = ocr::convert(ocr::Request {
        base_url: Some(&url),
        cache_dir: Some(&cache),
        ..ocr::Request::new(&file)
    })
    .await
    .unwrap_err();

    let message = error.to_string();
    assert!(message.contains("itsmini"), "must name itsmini: {message}");
    assert!(message.contains(&url), "must name the URL: {message}");
    assert!(
        message.contains("refused") || message.contains("os error"),
        "the cause must reach the user, not only the URL reqwest echoes back: {message}"
    );
    assert!(!message.trim().is_empty(), "never an empty failure");
    assert_eq!(error.kind(), "not_reachable");
    assert_eq!(error.exit_code(), 1);

    assert!(
        files_in(&cache).is_empty(),
        "a failure is not an answer and must not be cached"
    );
}

/// A 200 is only an answer if its body is the documented one.
#[tokio::test]
async fn a_200_with_the_wrong_shape_is_an_error_not_empty_text() {
    for wrong in [
        r#"{"hello":"world"}"#,
        r#"{"text":42,"pages":1}"#,
        r#"{"text":"hi"}"#,
        r#"[]"#,
        "<html><body>itsmini</body></html>",
    ] {
        let stub = Stub::start(200, wrong);
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("ocr-cache");
        let file = image(dir.path(), "scan.png", IMAGE_A);

        let error = ocr::convert(request(&file, &cache, &stub))
            .await
            .unwrap_err();
        let message = error.to_string();

        assert!(
            message.contains("itsmini"),
            "`{wrong}` must be reported against itsmini: {message}"
        );
        assert!(!message.trim().is_empty());
        assert_eq!(error.kind(), "protocol", "`{wrong}` -> {message}");
        assert_eq!(error.exit_code(), 4, "`{wrong}`");
        assert!(files_in(&cache).is_empty(), "`{wrong}` must not be cached");
    }
}

#[tokio::test]
async fn a_non_2xx_answer_is_an_error_that_names_the_status() {
    let stub = Stub::start(500, "itsmini is still loading a model");
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("ocr-cache");
    let file = image(dir.path(), "scan.png", IMAGE_A);

    let error = ocr::convert(request(&file, &cache, &stub))
        .await
        .unwrap_err();
    let message = error.to_string();

    assert!(message.contains("itsmini"), "{message}");
    assert!(message.contains("500"), "{message}");
    assert!(
        message.contains("still loading"),
        "the body is reported: {message}"
    );
    assert_eq!(error.exit_code(), 1);
    assert!(files_in(&cache).is_empty(), "a failure must not be cached");
}

/// The judgement call worth stating plainly: a 200 carrying no text is treated as
/// a failure, because "silently nothing" is the outcome `ACCEPTANCE.md` §8 rules
/// out and an unbuilt server is a likelier cause than a blank page. The page count
/// and confidence stay in the message, so a genuinely blank scan is diagnosable.
#[tokio::test]
async fn an_empty_text_from_a_200_is_not_an_empty_success() {
    for empty in [
        r#"{"text":"","pages":3,"confidence":0.5}"#,
        r#"{"text":"   \n ","pages":1}"#,
    ] {
        let stub = Stub::start(200, empty);
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("ocr-cache");
        let file = image(dir.path(), "scan.png", IMAGE_A);

        let error = ocr::convert(request(&file, &cache, &stub))
            .await
            .unwrap_err();
        let message = error.to_string();

        assert!(message.contains("itsmini"), "{message}");
        assert!(message.contains("empty"), "{message}");
        assert_eq!(error.kind(), "protocol", "{message}");
        assert_eq!(error.exit_code(), 4, "{message}");
        assert!(files_in(&cache).is_empty(), "nothing was converted");
    }
}

// -------------------------------------------------------------- the command --

#[test]
fn the_ocr_command_prints_the_text_and_records_nothing() {
    let stub = Stub::start(200, RECORDED);
    let home = tempfile::tempdir().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let file = image(dir.path(), "scan.png", IMAGE_A);

    let output = pie(
        home.path(),
        &stub.base_url,
        &["ocr", file.to_str().unwrap()],
    );

    assert!(output.status.success(), "stderr: {}", stderr(&output));
    assert_eq!(
        stdout(&output),
        "RECORDED TEXT 1\n",
        "human mode prints the text and nothing else, so it can be redirected"
    );

    // The cache lands under PIE_HOME, which is the path the acceptance gate names.
    let cache = home.path().join("ocr-cache");
    let entries = files_in(&cache);
    assert_eq!(entries.len(), 1, "one entry: {entries:?}");
    assert_eq!(
        entries[0].parent().unwrap(),
        cache.as_path(),
        "$PIE_HOME/ocr-cache/"
    );

    // ACCEPTANCE §8: no session row, and no transcript, from `pie ocr`.
    assert!(
        !home.path().join("usage.jsonl").exists(),
        "`pie ocr` spends no quota, so it may not write a usage row"
    );
    assert!(
        !home.path().join("sessions").exists(),
        "`pie ocr` records no transcript"
    );
}

#[test]
fn the_json_flag_reports_the_structured_result_and_the_cache_hit() {
    let stub = Stub::start(200, RECORDED);
    let home = tempfile::tempdir().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let file = image(dir.path(), "scan.png", IMAGE_A);
    let args = ["--json", "ocr", file.to_str().unwrap()];

    let first = pie(home.path(), &stub.base_url, &args);
    assert!(first.status.success(), "stderr: {}", stderr(&first));
    let value: serde_json::Value = serde_json::from_str(&stdout(&first)).unwrap();
    assert_eq!(value["text"], "RECORDED TEXT 1");
    assert_eq!(value["pages"], 2);
    assert_eq!(value["confidence"], 0.93);
    assert_eq!(value["cached"], false, "the first run converted");

    let second = pie(home.path(), &stub.base_url, &args);
    assert!(second.status.success(), "stderr: {}", stderr(&second));
    let value: serde_json::Value = serde_json::from_str(&stdout(&second)).unwrap();
    assert_eq!(value["text"], "RECORDED TEXT 1");
    assert_eq!(value["cached"], true, "the second run was free");

    assert_eq!(stub.request_count(), 1, "one conversion, two commands");
}

#[test]
fn an_unreachable_itsmini_fails_the_command_without_printing_an_empty_success() {
    let home = tempfile::tempdir().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let file = image(dir.path(), "scan.png", IMAGE_A);
    let url = unreachable_base_url();

    let output = pie(home.path(), &url, &["ocr", file.to_str().unwrap()]);

    assert!(!output.status.success(), "must not report success");
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stdout(&output).trim().is_empty(),
        "nothing may be printed as if it were text: {}",
        stdout(&output)
    );

    let message = stderr(&output);
    assert!(message.contains("itsmini"), "{message}");
    assert!(message.contains(&url), "{message}");
}

#[test]
fn a_missing_file_and_a_bad_base_url_are_reported_as_such() {
    let home = tempfile::tempdir().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let absent = dir.path().join("not-here.png");

    let missing = pie(
        home.path(),
        "http://127.0.0.1:1",
        &["ocr", absent.to_str().unwrap()],
    );
    assert_eq!(missing.status.code(), Some(1));
    assert!(
        stderr(&missing).contains("not-here.png"),
        "a missing file must be named: {}",
        stderr(&missing)
    );

    let file = image(dir.path(), "scan.png", IMAGE_A);
    let bad = pie(
        home.path(),
        "ftp://itsmini.lan",
        &["ocr", file.to_str().unwrap()],
    );
    assert_eq!(
        bad.status.code(),
        Some(2),
        "a bad setting is a config error"
    );
    assert!(
        stderr(&bad).contains("PIE_ITSMINI_URL"),
        "and must name the setting: {}",
        stderr(&bad)
    );
}

// ---------------------------------------------------------- no spawn, no row --

/// `ACCEPTANCE.md` §8: no session row from `ocr`, and the same guarantee `pie
/// sessions` gets — a static check that neither source path can spawn a CLI or
/// write a transcript.
#[test]
fn the_ocr_command_and_client_can_neither_spawn_nor_record() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));

    // The command does exactly one thing the deliverable allows (an HTTP request
    // through `ocr.rs`) and nothing else.
    scan_for(
        &root.join("src/commands/ocr.rs"),
        &[
            "crate::process",
            "process::",
            ".stream(",
            "session_log::append",
            "Command::new",
            "Session::",
            "record(",
            "record_best_effort",
        ],
    );

    // The client is allowed to borrow `process::redact` for its diagnostics, and
    // nothing that spawns or records.
    scan_for(
        &root.join("src/ocr.rs"),
        &[
            "Command::new",
            "session_log::",
            "session::",
            "Session::",
            "record(",
            "record_best_effort",
        ],
    );
}

/// Fail if `path` names any of `forbidden` on a non-comment line.
fn scan_for(path: &Path, forbidden: &[&str]) {
    let text = std::fs::read_to_string(path).expect("read the source under audit");
    let mut offenders = Vec::new();

    for (index, line) in text.lines().enumerate() {
        if line.trim_start().starts_with("//") {
            continue;
        }
        for token in forbidden {
            if line.contains(token) {
                offenders.push(format!("{}:{} {token}", index + 1, line.trim()));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "{} must neither spawn nor record, but it names: {offenders:?}",
        path.display()
    );
}
