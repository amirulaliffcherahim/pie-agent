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

use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
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

    let deadline = tokio::time::Instant::now() + spec.timeout;
    let mut lines = BufReader::new(stdout).lines();
    let mut stdout_lines = 0usize;
    let mut timed_out = false;
    let mut io_error: Option<std::io::Error> = None;

    loop {
        match tokio::time::timeout_at(deadline, lines.next_line()).await {
            // deadline hit
            Err(_elapsed) => {
                timed_out = true;
                break;
            }
            Ok(Ok(Some(line))) => {
                stdout_lines += 1;
                on_line(&line);
            }
            Ok(Ok(None)) => break,
            Ok(Err(e)) => {
                io_error = Some(e);
                break;
            }
        }
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

/// Kill a process and everything it spawned.
///
/// `Child::kill` only signals the direct child; vendor CLIs spawn node,
/// subagents, and shells, so we must take the whole tree.
pub async fn kill_tree(pid: Option<u32>) {
    let Some(pid) = pid else { return };

    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .status()
            .await;
    }

    #[cfg(not(windows))]
    {
        // The child leads its own process group (see `process_group(0)` above),
        // so a negative pid signals the whole tree, not just the direct child.
        let group = format!("-{pid}");
        for signal in ["-TERM", "-KILL"] {
            let _ = Command::new("kill")
                .args([signal, &group])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .await;
            if signal == "-TERM" {
                // Give the tree a moment to exit cleanly before escalating.
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        }
    }
}

/// Read `reader` to EOF, keeping only the last `cap` bytes.
async fn drain_tail<R: AsyncRead + Unpin>(reader: R, cap: usize) -> String {
    let mut lines = BufReader::new(reader).lines();
    let mut buf: Vec<u8> = Vec::new();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                buf.extend_from_slice(line.as_bytes());
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
const OPAQUE_RUN_MIN: usize = 60;

/// Replace every unbroken 60+ character run of `[A-Za-z0-9._-]`.
fn collapse_long_runs(input: &str) -> String {
    fn is_run_char(c: char) -> bool {
        c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.'
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

fn has_secret_prefix(token: &str) -> bool {
    const PREFIXES: [&str; 5] = [
        "sk-", "eyJ", // JWT header
        "ghp_", "gho_", "ya29.",
    ];
    PREFIXES.iter().any(|p| token.starts_with(p)) || token.eq_ignore_ascii_case("bearer")
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
