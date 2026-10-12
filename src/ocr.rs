//! `pie ocr` — an image or a scanned PDF turned into text, via itsmini.
//!
//! itsmini is a separate home-server project (`PLAN.md` §10) and **does not exist
//! yet**, so the properties that matter most here are the ones that hold while it
//! is absent: an unreachable itsmini is a stated error naming itsmini and the
//! base URL — never an empty success, never a silent empty string (`RISKS.md`
//! §11, `ACCEPTANCE.md` §8). A non-2xx answer, or a 200 whose body is not the
//! documented shape, is likewise an error rather than empty text.
//!
//! Three invariants shape the rest:
//!
//! * **A cache hit makes no request.** That is the token argument for the whole
//!   capability (`DESIGN.md` §11): ~1–2k vision tokens become ~200–500 text
//!   tokens, and only once per image rather than once per turn. The cache is keyed
//!   by the file's *contents* plus the parameters that change the answer, so the
//!   same image under a second name is served without a second conversion.
//! * **Nothing here spawns a vendor CLI, and nothing here writes a session row.**
//!   The work happens on itsmini; pie is only the client. So `pie ocr` spends no
//!   model quota and records no turn.
//! * **The cache stores text, never the image.** No image byte is persisted or
//!   logged: an entry holds the content length, the page count, the confidence and
//!   the text.
//!
//! Stage E — the MCP server that exposes this to agy/codex/claude — is
//! deliberately not built here. `DESIGN.md` §11 keeps it a separate stage so an
//! incomplete milestone shows up at the stage that is incomplete.

use std::collections::hash_map::DefaultHasher;
use std::hash::Hasher;
use std::io::Read;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::cache;
use crate::error::{PieError, Result};
use crate::paths;

/// Environment variable naming the itsmini base URL.
///
/// Read **at call time** and never persisted: itsmini is a home server whose
/// address can change (a DHCP lease, a laptop moving between networks), so a
/// value fixed at startup would be wrong by the time it is used. Pending a
/// configuration field for this, the environment is the only source.
pub const BASE_URL_ENV: &str = "PIE_ITSMINI_URL";

/// Where itsmini is assumed to listen when nothing overrides it.
///
/// A labelled guess: itsmini does not exist yet, so no default can be verified.
/// It is deliberately loopback — trying the user's own machine first is the only
/// default that cannot send an image path to a host the user did not name — and
/// adopted from the conventional local server port.
pub const DEFAULT_BASE_URL: &str = "http://127.0.0.1:8000";

/// The endpoint path, frozen by the itsmini contract in `PLAN.md` §10.
pub const OCR_PATH: &str = "/ocr";

/// The `output` value sent when none is requested.
///
/// The contract makes `output` a required field, so something must always be
/// sent. `text` is the only value whose meaning is obvious; itsmini may well
/// accept others (markdown, hOCR), and `--output` forwards whatever the user
/// supplies.
pub const DEFAULT_OUTPUT: &str = "text";

/// How long one conversion may take before pie gives up on the answer.
///
/// A local-network request against a home server that may still be loading a
/// model is slower than an API call, hence a minute rather than a few seconds.
/// The bound is what keeps a wedged server from hanging a shell forever.
pub const REQUEST_TIMEOUT_SECS: u64 = 120;

/// Domain separator for the cache key.
///
/// Versioned so a future change to what the key covers cannot be mistaken for an
/// entry written under the old rules: bumping this string orphans the old
/// entries, which then cost one conversion each, never a wrong answer.
const KEY_DOMAIN: &[u8] = b"pie-ocr-cache-v1";

/// Separator between the keyed parameters, so `("a","bc")` and `("ab","c")` hash
/// differently.
const KEY_SEPARATOR: u8 = 0x1f;

/// Bytes hashed per read.
///
/// The file is hashed in a stream rather than read whole: a scanned PDF can be
/// hundreds of megabytes, and a cache lookup must not cost that much memory.
const HASH_CHUNK: usize = 64 * 1024;

/// One conversion request.
///
/// The cache directory and base URL are request fields rather than process
/// globals for the same reason `Session::start_in` exists: a test must be able to
/// point them somewhere else without mutating the process environment, which is
/// process-wide and racy under parallel tests.
#[derive(Debug, Clone)]
pub struct Request<'a> {
    /// The image or scanned PDF to convert.
    pub file: &'a Path,
    /// Language hint forwarded as `lang`. An empty hint is no hint, and is then
    /// omitted from the body entirely.
    pub lang: Option<&'a str>,
    /// Output format forwarded as `output`. `None` means [`DEFAULT_OUTPUT`].
    pub output: Option<&'a str>,
    /// Ignore any cached text and ask itsmini again.
    pub refresh: bool,
    /// itsmini base URL. `None` reads [`BASE_URL_ENV`] now.
    pub base_url: Option<&'a str>,
    /// Cache directory. `None` is [`paths::ocr_cache_dir`].
    pub cache_dir: Option<&'a Path>,
}

impl<'a> Request<'a> {
    /// A request for `file` with every default in place: no language hint, the
    /// default output, no refresh, and the environment's itsmini with pie's own
    /// cache directory.
    pub fn new(file: &'a Path) -> Self {
        Self {
            file,
            lang: None,
            output: None,
            refresh: false,
            base_url: None,
            cache_dir: None,
        }
    }
}

/// The text itsmini returned, plus where pie got it from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OcrResult {
    /// The extracted text.
    pub text: String,
    /// Pages itsmini processed — 1 for a single image.
    pub pages: u32,
    /// itsmini's confidence, when it reports one.
    pub confidence: Option<f64>,
    /// Whether this was served from the content-hash cache, with no request.
    ///
    /// Reported rather than hidden: "was that free?" is a question the user of a
    /// token-saving tool is entitled to an answer to, and it is the only way to
    /// tell a working cache from a working server.
    pub cached: bool,
}

/// What pie has cached for one content key. Not the documented wire shape: the
/// wire has no reason to tell us which revision of the file it answered from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct CachedOcr {
    /// Length of the source in bytes, recorded so an entry is refused if the file
    /// has changed since.
    source_bytes: u64,
    /// Pages itsmini processed.
    pages: u32,
    /// itsmini's confidence, when it reported one.
    #[serde(default)]
    confidence: Option<f64>,
    /// The extracted text — the entire point, and all that is persisted.
    text: String,
}

impl CachedOcr {
    /// Present a cached record as a result, saying whether it came from disk.
    fn into_result(self, cached: bool) -> OcrResult {
        OcrResult {
            text: self.text,
            pages: self.pages,
            confidence: self.confidence,
            cached,
        }
    }
}

/// The documented request: `{file, lang?, output}` (`PLAN.md` §10).
#[derive(Debug, Serialize)]
struct WireRequest<'a> {
    /// The file, sent as the path the user typed — see [`convert`].
    file: String,
    /// Language hint, omitted when absent rather than sent empty.
    #[serde(skip_serializing_if = "Option::is_none")]
    lang: Option<&'a str>,
    /// Requested output format.
    output: &'a str,
}

/// The documented response: `{text, pages, confidence}`.
#[derive(Debug, Deserialize)]
struct WireResponse {
    /// The extracted text.
    text: String,
    /// Pages processed.
    pages: u32,
    /// itsmini's confidence, **when it supplies one**.
    ///
    /// Optional on purpose while itsmini does not exist: the contract lists the
    /// field, but its scale and whether it is always present are both unverified,
    /// and requiring it would make pie reject a server that answered correctly
    /// while omitting a number pie does not act on. `text` and `pages` stay
    /// required — those are what a wrong-shape check must catch.
    #[serde(default)]
    confidence: Option<f64>,
}

/// Convert one file, serving the text from the content-hash cache when it can.
///
/// `file` goes on the wire as the path the user typed. Whether itsmini wants a
/// path it can reach on its own filesystem, a mount, or the bytes themselves is
/// **unverified** — itsmini does not exist — so pie sends exactly what the frozen
/// contract names and nothing else. In particular it never puts the image on the
/// wire and never persists it.
pub async fn convert(request: Request<'_>) -> Result<OcrResult> {
    let file = request.file;
    if !file.is_file() {
        return Err(PieError::MissingFile(file.to_path_buf()));
    }

    let base = normalize_base(&match request.base_url {
        Some(url) => url.to_string(),
        None => base_url(),
    })?;
    let endpoint = format!("{base}{OCR_PATH}");
    let lang = effective_lang(request.lang);
    let output = effective_output(request.output);

    // One pass over the file decides both the key and the recorded length, so the
    // two can never disagree about which revision was hashed.
    let (key, source_bytes) = content_key(file, lang, output, &endpoint)?;
    let cache_file = request
        .cache_dir
        .map_or_else(paths::ocr_cache_dir, PathBuf::from)
        .join(format!("{key}.json"));

    if !request.refresh {
        if let Some(record) = read_cache(&cache_file, source_bytes) {
            tracing::debug!(
                file = %file.display(),
                cache = %cache_file.display(),
                "ocr served from the content-hash cache; no request was made"
            );
            return Ok(record.into_result(true));
        }
    }

    let wire = fetch(
        &endpoint,
        &WireRequest {
            file: file.display().to_string(),
            lang,
            output,
        },
    )
    .await?;

    let record = CachedOcr {
        source_bytes,
        pages: wire.pages,
        confidence: wire.confidence,
        text: wire.text,
    };
    store(&cache_file, &record);
    Ok(record.into_result(false))
}

/// The itsmini base URL: [`BASE_URL_ENV`] read now, else [`DEFAULT_BASE_URL`].
pub fn base_url() -> String {
    std::env::var(BASE_URL_ENV)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_BASE_URL.to_string())
}

/// `lang` as it goes on the wire: a blank hint is no hint.
fn effective_lang(lang: Option<&str>) -> Option<&str> {
    lang.map(str::trim).filter(|value| !value.is_empty())
}

/// `output` as it goes on the wire, defaulted — the contract requires the field.
fn effective_output(output: Option<&str>) -> &str {
    output
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(DEFAULT_OUTPUT)
}

/// A base URL with no trailing slash, so `{base}/ocr` never doubles one.
///
/// Validated because a bad base URL is a user error with an obvious fix, and
/// "itsmini not reachable at ftp://…" would send them looking at the network
/// instead of at the setting.
fn normalize_base(base: &str) -> Result<String> {
    let trimmed = base.trim().trim_end_matches('/').to_string();
    if !trimmed.starts_with("http://") && !trimmed.starts_with("https://") {
        return Err(PieError::Config(format!(
            "{BASE_URL_ENV} must be an http(s) URL, e.g. {DEFAULT_BASE_URL} — got `{base}`"
        )));
    }
    Ok(trimmed)
}

/// Read the cached text for this key, refusing an entry recorded for a different
/// revision of the file.
///
/// Tolerant by construction: `cache::read` treats a missing, oversized, or corrupt
/// file as a miss, so a damaged cache costs one request and never a failure.
fn read_cache(path: &Path, source_bytes: u64) -> Option<CachedOcr> {
    let record = cache::read::<CachedOcr>(path)?.value;
    if record.source_bytes != source_bytes {
        tracing::debug!(
            path = %path.display(),
            recorded = record.source_bytes,
            observed = source_bytes,
            "ignoring an OCR cache entry recorded for a different file revision"
        );
        return None;
    }
    Some(record)
}

/// Record the text. **Best effort**: a cache write failure must never fail a
/// conversion that already succeeded — the same rule `DESIGN.md` §11 sets for a
/// session write, applied to what is the same kind of optimisation. Losing the
/// entry costs one more request; failing the command would lose the answer.
fn store(path: &Path, record: &CachedOcr) {
    if let Err(error) = cache::write_now(path, record) {
        tracing::warn!(
            path = %path.display(),
            error = %error,
            "could not cache the OCR text; the conversion itself succeeded"
        );
    }
}

/// The cache key: a 128-bit digest of the file's bytes **and** of everything that
/// changes the answer.
///
/// # What is hashed, and what is not
///
/// The bytes, the request's own parameters (`lang`, `output`), and the endpoint
/// are all folded in. The parameters matter for correctness rather than hygiene:
/// the same image converted with `--lang ch` is a different answer, and keying on
/// the bytes alone would serve the `en` text for it. The endpoint matters because
/// the text is *a specific server's* answer; pointing pie at a different itsmini
/// must not inherit it.
///
/// The file's **name** is deliberately not hashed, so a copy under a second path
/// is converted once. Nothing in the key can be read back out of a directory
/// listing either: a cache entry names no file.
///
/// # Why this hash
///
/// `sha2` is not a dependency, and adding one is outside this stage. So the key is
/// `std`'s `DefaultHasher` (SipHash-1-3) run twice over the same stream with
/// different domain bytes, giving 128 bits from one pass. The honest reading:
/// this is a cache key, not a security boundary. A collision would mean the wrong
/// text for a file the user already owns, and the mitigations are the recorded
/// byte length (checked on every read), the 128-bit width, and `--refresh`.
///
/// `DefaultHasher::new` is deterministic within a build, but its algorithm is
/// **not** specified across Rust releases, so a toolchain upgrade can change the
/// key and orphan every entry. That failure mode is a cache miss — a conversion
/// that used to be free — never a wrong answer.
///
/// Returns the key and the number of bytes actually hashed, so the caller records
/// the revision that was described rather than a separately read length.
fn content_key(
    file: &Path,
    lang: Option<&str>,
    output: &str,
    endpoint: &str,
) -> Result<(String, u64)> {
    let mut first = DefaultHasher::new();
    let mut second = DefaultHasher::new();

    for (hasher, domain) in [(&mut first, 0x01u8), (&mut second, 0x02u8)] {
        hasher.write_u8(domain);
        hasher.write(KEY_DOMAIN);
        hasher.write(endpoint.as_bytes());
        hasher.write_u8(KEY_SEPARATOR);
        hasher.write(lang.unwrap_or_default().as_bytes());
        hasher.write_u8(KEY_SEPARATOR);
        hasher.write(output.as_bytes());
        hasher.write_u8(KEY_SEPARATOR);
    }

    let mut source = std::fs::File::open(file)?;
    let mut buffer = vec![0u8; HASH_CHUNK];
    let mut hashed: u64 = 0;

    loop {
        let read = source.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        let chunk = &buffer[..read];
        first.write(chunk);
        second.write(chunk);
        hashed += read as u64;
    }

    Ok((
        format!("{:016x}{:016x}", first.finish(), second.finish()),
        hashed,
    ))
}

/// The HTTP client.
///
/// `#[cfg]`-gated because `--no-default-features` drops `reqwest` (`Cargo.toml`):
/// everything that needs a client lives here, so a build without TLS has no
/// unreachable transport code and [`fetch`] answers with a config error instead.
#[cfg(feature = "http")]
mod transport {
    use std::time::Duration;

    use super::{WireRequest, WireResponse, BASE_URL_ENV, REQUEST_TIMEOUT_SECS};
    use crate::error::{truncate, PieError, Result};
    use crate::process;

    /// POST the request and validate the answer.
    pub(super) async fn fetch(endpoint: &str, request: &WireRequest<'_>) -> Result<WireResponse> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
            .build()
            .map_err(|error| not_reachable(endpoint, &bounded(&error.to_string())))?;

        let response = client
            .post(endpoint)
            .json(request)
            .send()
            .await
            .map_err(|error| transport_error(endpoint, &error))?;

        let status = response.status();
        // Read the body before branching so a failure can report what came back.
        let body = response
            .text()
            .await
            .map_err(|error| transport_error(endpoint, &error))?;

        if !status.is_success() {
            return Err(PieError::Backend {
                backend: "itsmini".into(),
                status: format!("HTTP {}", status.as_u16()),
                detail: format!("{endpoint}: {}", snippet(&body)),
            });
        }

        let parsed: WireResponse = serde_json::from_str(&body).map_err(|error| {
            PieError::protocol(
                "itsmini",
                format!(
                    "{endpoint} answered 200 with something other than the documented \
                     {{text, pages, confidence}} shape: {error} (body: {})",
                    snippet(&body)
                ),
            )
        })?;

        // A 200 is not automatically an answer. While itsmini is unbuilt, an empty
        // text is far more likely to be a half-started server than a blank page,
        // and "silently nothing" is the one outcome `ACCEPTANCE.md` §8 rules out.
        // Refusing it keeps the page count and confidence in the message, so a
        // genuinely blank scan is still diagnosable rather than invisible.
        if parsed.text.trim().is_empty() {
            return Err(PieError::protocol(
                "itsmini",
                format!(
                    "{endpoint} answered 200 with no text (pages {}, confidence {}) — refusing \
                     to report an empty success",
                    parsed.pages,
                    confidence_label(parsed.confidence)
                ),
            ));
        }

        Ok(parsed)
    }

    /// A transport failure: itsmini is absent, wedged, or answering something that
    /// is not HTTP. Never an empty success.
    fn transport_error(endpoint: &str, error: &reqwest::Error) -> PieError {
        let reason = if error.is_timeout() {
            format!("no answer within {REQUEST_TIMEOUT_SECS}s")
        } else {
            bounded(&deepest_cause(error))
        };
        not_reachable(endpoint, &reason)
    }

    /// The most specific cause the transport reports.
    ///
    /// reqwest's own message is mostly the URL we already name; the cause beneath
    /// it is the useful part, because "connection refused" and "name or service not
    /// known" mean different things to fix. Walked to the bottom rather than
    /// printed as a chain, so the informative end is not what a bound truncates
    /// away.
    fn deepest_cause(error: &reqwest::Error) -> String {
        let mut deepest = error.to_string();
        let mut cause: Option<&(dyn std::error::Error + 'static)> = Some(error);

        while let Some(current) = cause {
            let text = current.to_string();
            if !text.trim().is_empty() {
                deepest = text;
            }
            cause = current.source();
        }

        deepest
    }

    /// A transport message, bounded and redacted.
    ///
    /// Redacted because the error text is not ours: a proxy in front of a home
    /// server can echo a header back. Bounded because it is rendered into a
    /// terminal line.
    fn bounded(detail: &str) -> String {
        truncate(&process::redact(detail), 300)
    }

    /// The stated error `RISKS.md` §11 asks for, naming itsmini and the URL.
    fn not_reachable(endpoint: &str, reason: &str) -> PieError {
        PieError::NotReachable(format!(
            "itsmini not reachable at {endpoint}: {reason} — start the itsmini home server, or \
             set {BASE_URL_ENV} to point at it"
        ))
    }

    /// A bounded, redacted, single-line snippet of a response body.
    ///
    /// Not `process::diagnostic`: that says "(no stderr)" for an empty string,
    /// which is the wrong noun for an HTTP body.
    fn snippet(body: &str) -> String {
        let collapsed = body.split_whitespace().collect::<Vec<_>>().join(" ");
        let redacted = process::redact(&collapsed);
        if redacted.trim().is_empty() {
            return "(empty body)".to_string();
        }
        truncate(redacted.trim(), 300)
    }

    /// Render an optional confidence for a human-readable diagnostic.
    fn confidence_label(confidence: Option<f64>) -> String {
        match confidence {
            Some(value) => format!("{value:.3}"),
            None => "not reported".to_string(),
        }
    }

    #[cfg(test)]
    mod tests {
        #![allow(clippy::unwrap_used, clippy::expect_used)]
        use super::*;

        #[test]
        fn the_unreachable_message_names_itsmini_the_url_and_the_way_out() {
            let error = not_reachable("http://127.0.0.1:9/ocr", "connection refused");
            let message = error.to_string();

            assert!(message.contains("itsmini"), "{message}");
            assert!(message.contains("http://127.0.0.1:9/ocr"), "{message}");
            assert!(message.contains(BASE_URL_ENV), "{message}");
            assert_eq!(error.kind(), "not_reachable");
            assert_eq!(error.exit_code(), 1);
            assert!(!message.trim().is_empty(), "never an empty message");
        }

        #[test]
        fn a_snippet_is_one_line_bounded_and_never_empty() {
            assert_eq!(snippet("   \n  "), "(empty body)");
            assert_eq!(snippet("a\nb\tc"), "a b c");

            let bounded = snippet(&"x".repeat(4_000));
            assert!(bounded.chars().count() < 400, "{bounded}");
        }

        #[test]
        fn a_confidence_is_rendered_when_present_and_named_when_absent() {
            assert_eq!(confidence_label(Some(0.9336)), "0.934");
            assert_eq!(confidence_label(None), "not reported");
        }
    }
}

#[cfg(feature = "http")]
use transport::fetch;

/// Without the `http` feature there is no client to reach itsmini with.
#[cfg(not(feature = "http"))]
async fn fetch(endpoint: &str, _request: &WireRequest<'_>) -> Result<WireResponse> {
    Err(PieError::Config(format!(
        "pie was built without the `http` feature, so it cannot reach itsmini at {endpoint}"
    )))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn scratch() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn fixture(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn the_content_key_follows_content_not_the_name() {
        let dir = scratch();
        let first = fixture(dir.path(), "scan.png", b"the same bytes");
        let copy = fixture(dir.path(), "a-copy.png", b"the same bytes");
        let other = fixture(dir.path(), "scan-copy.png", b"other bytes");

        let key = |path: &Path| {
            content_key(path, None, "text", "http://host/ocr")
                .unwrap()
                .0
        };

        assert_eq!(key(&first), key(&copy), "a copy is the same content");
        assert_ne!(key(&first), key(&other));
    }

    #[test]
    fn the_content_key_covers_everything_that_changes_the_answer() {
        let dir = scratch();
        let file = fixture(dir.path(), "scan.png", b"bytes");
        let key = |lang: Option<&str>, output: &str, endpoint: &str| {
            content_key(&file, lang, output, endpoint).unwrap().0
        };
        let base = key(None, "text", "http://host/ocr");

        assert_ne!(
            base,
            key(Some("ch"), "text", "http://host/ocr"),
            "a language hint changes the text itsmini returns"
        );
        assert_ne!(
            base,
            key(None, "markdown", "http://host/ocr"),
            "a different output format is a different answer"
        );
        assert_ne!(
            base,
            key(None, "text", "http://other-host/ocr"),
            "the text is one server's answer, not a fact about the file"
        );
        assert_eq!(base, key(None, "text", "http://host/ocr"));
    }

    #[test]
    fn the_content_key_is_stable_bounded_and_covers_the_whole_file() {
        let dir = scratch();
        // Bigger than one hash chunk, so the streaming loop is exercised.
        let large = vec![0x7fu8; HASH_CHUNK * 2 + 17];
        let file = fixture(dir.path(), "big.pdf", &large);

        let (first, bytes) = content_key(&file, None, "text", "http://host/ocr").unwrap();
        let (again, _) = content_key(&file, None, "text", "http://host/ocr").unwrap();

        assert_eq!(first, again, "the key must be deterministic");
        assert_eq!(bytes, large.len() as u64, "the whole file is hashed");
        assert_eq!(first.len(), 32, "128 bits, as hex");
        assert!(first.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn a_blank_hint_is_no_hint_and_a_blank_format_is_the_default() {
        assert_eq!(effective_lang(None), None);
        assert_eq!(effective_lang(Some("")), None);
        assert_eq!(effective_lang(Some("  ")), None);
        assert_eq!(effective_lang(Some(" ch ")), Some("ch"));
        assert_eq!(effective_output(None), "text");
        assert_eq!(effective_output(Some("")), "text");
        assert_eq!(effective_output(Some("markdown")), "markdown");
    }

    #[test]
    fn the_wire_body_is_the_documented_shape() {
        let body = serde_json::to_value(WireRequest {
            file: "scan.png".into(),
            lang: Some("ch"),
            output: "text",
        })
        .unwrap();
        assert_eq!(body["file"], "scan.png");
        assert_eq!(body["lang"], "ch");
        assert_eq!(body["output"], "text");

        let no_hint = serde_json::to_value(WireRequest {
            file: "scan.png".into(),
            lang: None,
            output: "text",
        })
        .unwrap();
        assert!(
            no_hint.get("lang").is_none(),
            "an absent hint is omitted, not sent empty: {no_hint}"
        );
    }

    #[test]
    fn only_the_documented_shape_parses() {
        let good: WireResponse =
            serde_json::from_str(r#"{"text":"hi","pages":2,"confidence":0.93}"#).unwrap();
        assert_eq!(good.text, "hi");
        assert_eq!(good.pages, 2);
        assert_eq!(good.confidence, Some(0.93));

        // Confidence is the one field pie tolerates losing; it does not act on it.
        let without: WireResponse = serde_json::from_str(r#"{"text":"hi","pages":1}"#).unwrap();
        assert_eq!(without.confidence, None);

        for wrong in [
            r#"{"hello":"world"}"#,
            r#"{"text":42,"pages":1}"#,
            r#"{"text":"hi"}"#,
            r#"[]"#,
            "<html>not json</html>",
        ] {
            assert!(
                serde_json::from_str::<WireResponse>(wrong).is_err(),
                "`{wrong}` is not the documented shape"
            );
        }
    }

    #[test]
    fn a_base_url_must_be_http_and_loses_a_trailing_slash() {
        assert_eq!(
            normalize_base("http://127.0.0.1:8000/").unwrap(),
            "http://127.0.0.1:8000"
        );
        assert_eq!(
            normalize_base(" https://itsmini.lan ").unwrap(),
            "https://itsmini.lan"
        );
        assert!(normalize_base("127.0.0.1:8000").is_err());
        assert!(normalize_base("ftp://itsmini.lan").is_err());
        assert!(normalize_base("").is_err());
    }

    /// The seam the tests use: a request that overrides nothing reads the
    /// environment for itsmini and `PIE_HOME` for the cache.
    #[test]
    fn a_default_request_overrides_nothing() {
        let dir = scratch();
        let file = fixture(dir.path(), "scan.png", b"bytes");
        let request = Request::new(&file);

        assert_eq!(request.file, file);
        assert!(request.lang.is_none());
        assert!(request.output.is_none());
        assert!(!request.refresh);
        assert!(request.base_url.is_none());
        assert!(request.cache_dir.is_none());
    }

    /// A missing file is reported as a missing file, before anything is hashed or
    /// sent — never as empty text.
    #[tokio::test]
    async fn a_missing_file_is_a_missing_file_error() {
        let dir = scratch();
        let absent = dir.path().join("not-here.png");
        let error = convert(Request {
            cache_dir: Some(dir.path()),
            ..Request::new(&absent)
        })
        .await
        .unwrap_err();

        assert!(matches!(error, PieError::MissingFile(_)), "{error:?}");
        assert!(error.to_string().contains("not-here.png"), "{error}");
    }
}
