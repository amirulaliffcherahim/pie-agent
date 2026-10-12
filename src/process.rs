//! Process plumbing: hidden spawn, bounded stderr, deadline, kill-tree.
//!
//! This module is policy-free about *which* backends exist — callers pass the
//! program, argv, and the env keys to strip. What it guarantees
//! (`planning/DESIGN.md` §6) is:
//!
//! 1. the child never shows a console window,
//! 2. stderr is drained concurrently and kept bounded,
//! 3. a deadline always fires and kills the whole process tree,
//! 4. stdout is delivered line-by-line, tolerantly.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::Command;

use crate::error::{truncate, PieError, Result};

/// `CREATE_NO_WINDOW`. Passed to every child so nothing flashes on screen.
#[cfg(windows)]
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// How much stderr we retain for diagnostics.
pub const STDERR_TAIL_BYTES: usize = 8 * 1024;

/// Everything needed to run one child process.
#[derive(Debug, Clone)]
pub struct RunSpec {
    /// Provider id, for error messages.
    pub backend: String,
    /// Executable.
    pub program: PathBuf,
    /// Arguments, in order.
    pub args: Vec<OsString>,
    /// Working directory for the child.
    pub cwd: PathBuf,
    /// Environment variables to remove from the child's copy of our env.
    pub scrub: Vec<String>,
    /// Whole-turn deadline.
    pub timeout: Duration,
}

/// What happened to the child.
#[derive(Debug, Clone)]
pub struct RunSummary {
    /// Exit code when the process exited normally.
    pub exit_code: Option<i32>,
    /// Human-readable exit status (`exit 0`, `signal 9`, `unknown`).
    pub exit_status: String,
    /// Last [`STDERR_TAIL_BYTES`] of stderr.
    pub stderr_tail: String,
    /// How many stdout lines we delivered.
    pub stdout_lines: usize,
}

impl RunSummary {
    /// True when the process reported success.
    pub fn success(&self) -> bool {
        self.exit_code == Some(0)
    }
}

/// Spawn `spec`, deliver each stdout line to `on_line`, and enforce the deadline.
///
/// On deadline expiry the process **tree** is killed and
/// [`PieError::Timeout`] is returned — a hung vendor CLI must never outlive us.
pub async fn run_streaming(
    spec: &RunSpec,
    on_line: &mut (dyn FnMut(&str) + Send),
) -> Result<RunSummary> {
    let mut cmd = Command::new(&spec.program);
    cmd.args(&spec.args)
        .current_dir(&spec.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);

    // On unix the child leads its own process group, so a kill reaches the whole
    // tree — the equivalent of Windows' `taskkill /T` (`planning/DESIGN.md` §1).
    #[cfg(unix)]
    cmd.process_group(0);

    for key in &spec.scrub {
        cmd.env_remove(key);
    }

    let mut child = cmd.spawn().map_err(|source| PieError::Spawn {
        backend: spec.backend.clone(),
        source,
    })?;

    let pid = child.id();
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| PieError::protocol(&spec.backend, "child produced no stdout handle"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| PieError::protocol(&spec.backend, "child produced no stderr handle"))?;

    // Drain stderr concurrently: a chatty CLI must not fill a pipe and deadlock.
    let stderr_task = tokio::spawn(drain_tail(stderr, STDERR_TAIL_BYTES));

    // `Instant + Duration` panics on overflow, and the timeout is an unvalidated
    // `u64` from `--timeout` / `PIE_TIMEOUT_SECS`, so `checked_add` turns a
    // nonsense value into a config error instead of a panic.
    let deadline = tokio::time::Instant::now()
        .checked_add(spec.timeout)
        .ok_or_else(|| {
            PieError::Config(format!(
                "timeout of {}s is too large to schedule",
                spec.timeout.as_secs()
            ))
        })?;

    let mut reader = BufReader::new(stdout);
    let mut stdout_lines = 0usize;
    let mut timed_out = false;
    let mut io_error: Option<std::io::Error> = None;
    let mut truncated_lines = 0usize;

    loop {
        match tokio::time::timeout_at(deadline, read_line_capped(&mut reader, MAX_LINE_BYTES)).await
        {
            // deadline hit
            Err(_elapsed) => {
                timed_out = true;
                break;
            }
            Ok(Ok(Some(line))) => {
                stdout_lines += 1;
                if line.truncated {
                    truncated_lines += 1;
                }
                on_line(&line.text);
            }
            Ok(Ok(None)) => break,
            Ok(Err(e)) => {
                io_error = Some(e);
                break;
            }
        }
    }

    if truncated_lines > 0 {
        tracing::warn!(
            backend = %spec.backend,
            count = truncated_lines,
            cap_bytes = MAX_LINE_BYTES,
            "truncated oversized stdout line(s); a vendor line longer than the cap cannot be \
             buffered without bound"
        );
    }

    if timed_out {
        kill_tree(pid).await;
        // Reap so we do not leak a zombie, but do not block on a stuck child.
        let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
        stderr_task.abort();
        return Err(PieError::Timeout {
            backend: spec.backend.clone(),
            secs: spec.timeout.as_secs(),
        });
    }

    if let Some(e) = io_error {
        kill_tree(pid).await;
        let _ = child.wait().await;
        stderr_task.abort();
        return Err(PieError::Io(e));
    }

    // stdout closed; wait for exit (still bounded — a child can close stdout
    // and then hang).
    let status = match tokio::time::timeout_at(deadline, child.wait()).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            stderr_task.abort();
            return Err(PieError::Io(e));
        }
        Err(_elapsed) => {
            kill_tree(pid).await;
            let _ = child.wait().await;
            stderr_task.abort();
            return Err(PieError::Timeout {
                backend: spec.backend.clone(),
                secs: spec.timeout.as_secs(),
            });
        }
    };

    let stderr_tail = stderr_task.await.unwrap_or_default();

    Ok(RunSummary {
        exit_code: status.code(),
        exit_status: describe_status(&status),
        stderr_tail,
        stdout_lines,
    })
}

/// Resolve a binary name (or path) the way a shell would.///
/// A path with separators is used as-is. Otherwise every `PATH` entry is probed
/// with each `PATHEXT` suffix on Windows. Returns `None` rather than guessing —
/// callers turn that into [`PieError::BinaryMissing`].
pub fn resolve_binary(spec: &str) -> Option<PathBuf> {
    let raw = Path::new(spec);
    let looks_like_path =
        raw.is_absolute() || spec.contains('/') || spec.contains('\\') || spec.starts_with('.');

    if looks_like_path {
        return raw.is_file().then(|| raw.to_path_buf());
    }

    let path_var = std::env::var_os("PATH")?;

    #[cfg(windows)]
    let suffixes: Vec<String> = {
        let pathext =
            std::env::var("PATHEXT").unwrap_or_else(|_| ".EXE;.CMD;.BAT;.COM".to_string());
        let mut v: Vec<String> = pathext
            .split(';')
            .map(|e| e.trim().to_ascii_lowercase())
            .filter(|e| !e.is_empty())
            .collect();
        v.push(String::new());
        v
    };

    #[cfg(not(windows))]
    let suffixes: Vec<String> = vec![String::new()];

    for dir in std::env::split_paths(&path_var) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        for suffix in &suffixes {
            let candidate = dir.join(format!("{spec}{suffix}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// How much of a single stdout line pie will buffer.
///
/// The input is a vendor CLI's stdout, which pie does not control. Without a
/// cap, one line with no newline (a base64 blob, a stuck progress bar) grows the
/// buffer without bound and OOMs before the deadline can fire. 1 MiB is far
/// larger than any real stream-json event — roughly 250k tokens of answer text —
/// so truncation signals a broken or hostile stream rather than a big answer.
pub const MAX_LINE_BYTES: usize = 1 << 20;

/// One line read from a child, with its bytes interpreted lossily.
#[derive(Debug, Clone)]
pub struct ChildLine {
    /// The line with the trailing newline (and a lone CR) removed.
    pub text: String,
    /// True when the line exceeded the cap and was cut short.
    pub truncated: bool,
}

/// Read one line, retaining at most `cap` bytes, as **lossy** UTF-8.
///
/// Deliberately not `lines()` / `next_line()`: those fail with `InvalidData` on
/// a byte that is not valid UTF-8, which aborted a healthy run (invariant I5) and
/// made the whole turn a `PieError::Io`. A vendor CLI printing one stray byte is
/// not a reason to discard a good answer. `drain_tail` already took this view for
/// stderr; this makes stdout agree.
///
/// Uses `fill_buf`/`consume` rather than `read_until` so the cap can be applied
/// *while* reading, not after a line has already been materialised.
///
/// Returns `None` at end of stream.
pub async fn read_line_capped<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    cap: usize,
) -> std::io::Result<Option<ChildLine>> {
    let mut out: Vec<u8> = Vec::new();
    let mut saw_any = false;
    let mut truncated = false;

    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return if saw_any {
                Ok(Some(finish_line(out, truncated)))
            } else {
                Ok(None)
            };
        }
        saw_any = true;

        let newline = available.iter().position(|&b| b == b'\n');
        // Bytes to consider from this chunk, excluding the newline itself.
        let take = newline.unwrap_or(available.len());
        let consume = newline.map_or(available.len(), |at| at + 1);

        let room = cap.saturating_sub(out.len());
        if take <= room {
            out.extend_from_slice(&available[..take]);
        } else {
            out.extend_from_slice(&available[..room]);
            truncated = true;
        }

        reader.consume(consume);

        if newline.is_some() {
            return Ok(Some(finish_line(out, truncated)));
        }
    }
}

/// Trim a trailing CR and interpret the bytes lossily.
fn finish_line(mut bytes: Vec<u8>, truncated: bool) -> ChildLine {
    if bytes.last() == Some(&b'\r') {
        bytes.pop();
    }
    ChildLine {
        text: String::from_utf8_lossy(&bytes).into_owned(),
        truncated,
    }
}

/// Kill a process and everything it spawned.
///
/// `Child::kill` only signals the direct child; vendor CLIs spawn node,
/// subagents, and shells, so we must take the whole tree.
pub async fn kill_tree(pid: Option<u32>) {
    let Some(pid) = pid else { return };

    #[cfg(windows)]
    {
        let mut cmd = Command::new("taskkill");
        cmd.args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW);
        // Even a trusted OS helper gets the scrub. The invariant is about what
        // any child can observe, not about which children we happen to trust.
        for key in crate::config::BUILTIN_SCRUB {
            cmd.env_remove(key);
        }
        // Bounded: `kill_tree` runs on the timeout path, so it must not itself
        // block past the deadline. A hung `taskkill` would defeat the timeout it
        // exists to enforce.
        let _ = tokio::time::timeout(Duration::from_secs(5), cmd.status()).await;
    }

    #[cfg(not(windows))]
    {
        // The child leads its own process group (see `process_group(0)` above),
        // so a negative pid signals the whole tree, not just the direct child.
        let group = format!("-{pid}");
        for signal in ["-TERM", "-KILL"] {
            let mut cmd = Command::new("kill");
            cmd.args([signal, &group])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            for key in crate::config::BUILTIN_SCRUB {
                cmd.env_remove(key);
            }
            let _ = tokio::time::timeout(Duration::from_secs(5), cmd.status()).await;
            if signal == "-TERM" {
                // Give the tree a moment to exit cleanly before escalating.
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        }
    }
}

/// Read `reader` to EOF, keeping only the last `cap` bytes.
///
/// Lines are read with the same capped, lossy reader as stdout, so neither a
/// non-UTF-8 byte nor an unbounded line can break or exhaust the tail.
async fn drain_tail<R: AsyncRead + Unpin>(reader: R, cap: usize) -> String {
    let mut reader = BufReader::new(reader);
    let mut buf: Vec<u8> = Vec::new();
    loop {
        match read_line_capped(&mut reader, MAX_LINE_BYTES).await {
            Ok(Some(line)) => {
                buf.extend_from_slice(line.text.as_bytes());
                buf.push(b'\n');
                if buf.len() > cap {
                    let excess = buf.len() - cap;
                    buf.drain(..excess);
                }
            }
            Ok(None) => break,
            Err(_) => break,
        }
    }
    // A byte-cap can land mid-character; lossy conversion keeps the tail useful
    // instead of discarding it.
    String::from_utf8_lossy(&buf).into_owned()
}

fn describe_status(status: &std::process::ExitStatus) -> String {
    match status.code() {
        Some(c) => format!("exit {c}"),
        None => "terminated by signal".to_string(),
    }
}

/// Redact anything that looks like a bearer token or a long secret blob.
///
/// Applied to every diagnostic we surface, so a credential cannot leak into an
/// error message (`RISKS.md` §10). Two independent passes, because either one
/// alone has a hole:
///
/// 1. **Long opaque runs** — any unbroken 60+ character run of
///    `[A-Za-z0-9._-]`. Catches `token=AAAA…` and raw JWTs, where the `=` or a
///    path separator would hide the value from a token-level check. That hole
///    was a real bug caught by `redacts_long_opaque_blobs`.
/// 2. **Known prefixes** — `sk-`, `ghp_`, `eyJ`, `ya29.`, and the word
///    `Bearer`, which also marks the *following* token for redaction.
pub fn redact(input: &str) -> String {
    let collapsed = collapse_long_runs(input);

    let mut out = String::with_capacity(collapsed.len());
    let mut redact_next = false;

    for piece in collapsed.split_inclusive(char::is_whitespace) {
        let trimmed = piece.trim_end();
        let trailing = &piece[trimmed.len()..];

        if trimmed.is_empty() {
            out.push_str(piece);
            continue;
        }

        if redact_next {
            out.push_str("«redacted»");
            out.push_str(trailing);
            redact_next = false;
            continue;
        }

        if has_secret_prefix(trimmed) {
            out.push_str("«redacted»");
            out.push_str(trailing);
            // `Authorization: Bearer <token>` — the secret is the next token.
            if trimmed.eq_ignore_ascii_case("bearer") {
                redact_next = true;
            }
            continue;
        }

        out.push_str(piece);
    }

    out
}

/// Length at which an unbroken character run is treated as a secret.
///
/// 40 rather than 60: Google API keys are 39 characters and GitHub PATs are 40,
/// so a 60-character threshold missed an entire class of real credentials. The
/// bias is deliberate — over-redacting a diagnostic costs a little readability,
/// under-redacting leaks a credential into a log (`RISKS.md` §10).
const OPAQUE_RUN_MIN: usize = 40;

/// Replace every unbroken run of `[A-Za-z0-9._\-+=]` that is long enough to be a
/// secret.
///
/// **`/` is deliberately NOT a run character.** It was added while fixing B13, to
/// catch base64 secrets that contain it, and that over-corrected into a new bug: a
/// diagnostic quoting a real path got the path redacted. `C:/Users/…/Temp/tmp.mYBqmsL2HF/config.toml`
/// printed as `C:«redacted»\config.toml`, and a config error that cannot name the
/// offending file is close to useless. Paths are far more common in diagnostics
/// than unprefixed base64, so the path wins.
///
/// The residual gap is accepted and recorded: a long base64 secret containing `/`
/// can survive in fragments. The precise mechanism is `SECRET_PREFIXES` below, which
/// catches known credential shapes regardless of length; the run heuristic is the
/// backstop for unknown shapes. `+` and `=` stay, since they are rare in paths and
/// common in base64.
fn collapse_long_runs(input: &str) -> String {
    fn is_run_char(c: char) -> bool {
        c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+' | '=')
    }

    fn flush(out: &mut String, run: &mut String) {
        if run.len() >= OPAQUE_RUN_MIN {
            out.push_str("«redacted»");
        } else {
            out.push_str(run);
        }
        run.clear();
    }

    let mut out = String::with_capacity(input.len());
    let mut run = String::new();
    for ch in input.chars() {
        if is_run_char(ch) {
            run.push(ch);
        } else {
            flush(&mut out, &mut run);
            out.push(ch);
        }
    }
    flush(&mut out, &mut run);
    out
}

/// Prefixes that identify a credential.
///
/// This list must remain a **superset** of the forbidden-prefix set that the
/// `no_secrets` scanner enforces; `tests/no_secrets.rs` asserts exactly that, so
/// the redactor can never be weaker than the rule it exists to uphold. It was
/// weaker: the scanner declared `AIza`, `sk-ant-`, `sk-proj-`, `xoxb-` and
/// `-----BEGIN` forbidden while the redactor recognised none of them.
pub const SECRET_PREFIXES: &[&str] = &[
    "sk-", // OpenAI / Anthropic style
    "sk-ant-",
    "sk-proj-",
    "AIza",       // Google API key
    "AKIA",       // AWS access key id
    "ya29.",      // Google OAuth access token
    "ghp_",       // GitHub personal access token
    "gho_",       // GitHub OAuth token
    "xoxb-",      // Slack bot token
    "hf_",        // Hugging Face token
    "eyJ",        // JWT header
    "-----BEGIN", // PEM-encoded private key
];

/// Whether a whitespace-delimited token looks like it carries a credential.
///
/// The prefix may appear **anywhere** in the token, not only at its start:
/// `key=AIzaSy…` and `"token":"sk-…"` are the shapes that actually occur, and a
/// start-only check let both through. A prefix must be followed by at least 8
/// characters so a bare marker (`sk-` in prose, `-----BEGIN` alone) is not
/// mistaken for a secret.
fn has_secret_prefix(token: &str) -> bool {
    if token.eq_ignore_ascii_case("bearer") {
        return true;
    }

    SECRET_PREFIXES.iter().any(|prefix| {
        token.find(prefix).is_some_and(|at| {
            // `find` returns a byte offset at a char boundary, and every prefix
            // is ASCII, so this slice cannot split a character.
            token[at + prefix.len()..].chars().count() >= 8
        })
    })
}

/// Bounded, redacted rendering of a stderr tail for an error message.
pub fn diagnostic(tail: &str) -> String {
    let redacted = redact(tail);
    let trimmed = redacted.trim();
    if trimmed.is_empty() {
        return "(no stderr)".to_string();
    }
    truncate(trimmed, 600)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn redacts_tokens_but_keeps_prose() {
        let s = "error: invalid key sk-abcdefghijklmnop for model gpt-6-luna";
        let r = redact(s);
        assert!(!r.contains("sk-abcdefghijklmnop"), "got {r}");
        assert!(r.contains("gpt-6-luna"), "prose survives: {r}");

        let jwt = "authorization: eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.payload.sig";
        assert!(!redact(jwt).contains("payload.sig"));
    }

    /// Regression: a `=` or path separator before the secret used to hide it
    /// from the token-level check. Any long opaque run must be redacted no
    /// matter what surrounds it.
    #[test]
    fn redacts_long_opaque_blobs() {
        let blob = "A".repeat(80);
        for shape in [
            format!("token={blob}"),
            format!("/tmp/{blob}/x"),
            format!("{{\"refresh_token\":\"{blob}\"}}"),
            blob.clone(),
        ] {
            let r = redact(&shape);
            assert!(!r.contains(&blob), "leaked from {shape:?} -> {r}");
            assert!(r.contains("«redacted»"), "nothing redacted in {shape:?}");
        }
    }

    #[test]
    fn redacts_the_token_after_bearer() {
        let r = redact("Authorization: Bearer abc123def456");
        assert!(!r.contains("abc123def456"), "got {r}");
        assert!(r.contains("Authorization:"), "keeps the header name: {r}");
    }

    #[test]
    fn short_ordinary_text_is_untouched() {
        let s = "codex exec failed with exit 1 in C:/Users/amiru/Local/Code/pie";
        assert_eq!(redact(s), s, "no false positives on paths and prose");
    }

    /// Every prefix the `no_secrets` scanner declares forbidden must also be
    /// redactable. The two lists had drifted: the scanner forbade `AIza`,
    /// `sk-ant-`, `sk-proj-`, `xoxb-` and `-----BEGIN`, and the redactor
    /// recognised none of them, so the mechanism I3 rests on was weaker than the
    /// rule it exists to uphold.
    #[test]
    fn redacts_every_declared_secret_prefix() {
        for prefix in SECRET_PREFIXES {
            // Both shapes that actually occur in diagnostics.
            for candidate in [
                format!("{prefix}AAAAbbbb1111CCCC"),
                format!("key={prefix}AAAAbbbb1111CCCC"),
                format!("\"token\":\"{prefix}AAAAbbbb1111CCCC\""),
            ] {
                let redacted = redact(&candidate);
                assert!(
                    !redacted.contains("AAAAbbbb1111CCCC"),
                    "prefix `{prefix}` survives redaction in {candidate:?} -> {redacted:?}"
                );
            }
        }
    }

    /// A bare marker is not a secret. Without the `>= 8 chars after the prefix`
    /// rule, prose mentioning `sk-` would be redacted.
    #[test]
    fn a_bare_prefix_without_a_value_is_not_redacted() {
        let text = "the names all start with sk- and eyJ by convention";
        assert_eq!(redact(text), text);
    }

    /// Regression: standard base64 uses `+`, `/` and `=`. They were not run
    /// characters, so one credential was split at the first `+` or `/` into
    /// fragments that each fell under the threshold and survived intact.
    /// Regression: adding `/` to the run charset (while fixing B13) made the
    /// redactor eat file paths out of diagnostics. `C:/Users/…/Temp/tmp.X/config.toml`
    /// printed as `C:«redacted»\config.toml`, which makes a config error unable to
    /// say which file failed.
    #[test]
    fn a_long_path_in_a_diagnostic_is_not_redacted() {
        let path = "C:/Users/amiru/AppData/Local/Temp/tmp.mYBqmsL2HF/config.toml";
        let message = format!("config: {path}: TOML parse error at line 1");

        let redacted = redact(&message);
        assert!(
            redacted.contains(path),
            "the path must survive redaction, or the error cannot name the file: {redacted}"
        );
        assert!(redacted.contains("TOML parse error"));

        // A unix-style path too.
        let unix = "/home/someone/very/long/project/path/that/exceeds/the/threshold/config.toml";
        assert!(
            redact(unix).contains(unix),
            "unix paths must survive too: {}",
            redact(unix)
        );
    }

    #[test]
    fn redacts_base64_containing_plus_slash_and_equals() {
        let blob = format!(
            "{a}+{b}/{c}=",
            a = "A".repeat(20),
            b = "b".repeat(20),
            c = "C".repeat(20)
        );
        assert!(blob.chars().count() > OPAQUE_RUN_MIN);

        let redacted = redact(&format!("payload {blob} end"));
        assert!(
            !redacted.contains(&blob),
            "base64 blob survived: {redacted}"
        );
        assert!(redacted.contains("payload") && redacted.contains("end"));
    }

    /// Regression for the 39-character Google API key: below the old 60-char
    /// threshold, and unprefixed by anything the redactor knew about.
    #[test]
    fn redacts_a_google_api_key_embedded_after_an_equals_sign() {
        let redacted = redact("GOOGLE_API_KEY key=AIzaSyD-1234567890abcdefghijklmnopqrs");
        assert!(
            !redacted.contains("AIzaSyD-1234567890abcdefghijklmnopqrs"),
            "google key survived: {redacted}"
        );
    }

    #[test]
    fn diagnostic_bounds_length() {
        let long = "x".repeat(5_000);
        assert!(diagnostic(&long).chars().count() < 700);
        assert_eq!(diagnostic("   "), "(no stderr)");
    }

    #[test]
    fn stderr_none_marker() {
        assert_eq!(diagnostic(""), "(no stderr)");
    }
}
