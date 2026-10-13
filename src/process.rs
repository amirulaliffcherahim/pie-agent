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
//!
//! # One spawn policy, in one place (`DESIGN.md` §20)
//!
//! Every child pie runs is spawned by [`configured_command`] below — the vendor CLIs
//! through [`run_streaming`], the tool loop's child through [`ChildSession`], stage B's
//! `bash` tool through whichever of the two fits. That is deliberate rather than tidy:
//! the policy (hidden window for I1, an env scrub for I2, an own process group so a kill
//! reaches the tree) is an invariant, and M10 recorded what a *copy* of an invariant does
//! — it stops holding silently, because nothing fails when the second copy drifts.
//! `tests/process.rs::src_spawns_children_in_exactly_one_module` is what asserts the count
//! instead of leaving it asserted by absence.

use std::ffi::OsString;
use std::io::{BufRead, BufReader as BlockingBufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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

/// Configure a child exactly the way every child pie runs is configured.
///
/// **The one copy of the spawn policy.** Callers differ in a single argument — the `stdin`
/// they hand the child — and that difference is the whole reason [`ChildSession`] exists:
/// a delegate turn wants `Stdio::null()` (nothing more to say), a session wants
/// `Stdio::piped()` (a `tool_result` is still to come). Everything else lives here, once:
///
/// - `CREATE_NO_WINDOW` on Windows, so I1 holds for every child rather than for the ones
///   whose author remembered the flag;
/// - its own process group on unix, so a kill reaches the whole tree rather than the direct
///   child — the equivalent of Windows' `taskkill /T` (`planning/DESIGN.md` §1);
/// - the caller's scrub list, removed from the child's copy of our environment (I2).
fn configured_command(spec: &RunSpec, stdin: Stdio) -> std::process::Command {
    let mut cmd = std::process::Command::new(&spec.program);
    cmd.args(&spec.args)
        .current_dir(&spec.cwd)
        .stdin(stdin)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }

    for key in &spec.scrub {
        cmd.env_remove(key);
    }

    cmd
}

/// Spawn `spec`, deliver each stdout line to `on_line`, and enforce the deadline.
///
/// On deadline expiry the process **tree** is killed and
/// [`PieError::Timeout`] is returned — a hung vendor CLI must never outlive us.
///
/// One-way by construction, and that is the point rather than an oversight: the child's
/// stdin is `null`, because a delegate-mode turn (`agy --print`, `codex exec`) is told
/// everything it needs up front and has nothing to be told afterwards. A caller that needs
/// to *keep talking* uses [`ChildSession`] below, which is the same policy with a pipe on
/// stdin — not a second policy.
pub async fn run_streaming(
    spec: &RunSpec,
    on_line: &mut (dyn FnMut(&str) + Send),
) -> Result<RunSummary> {
    // `kill_on_drop` is tokio's flag rather than the OS's, so it is set after the
    // conversion; the policy itself (`configured_command`) is `std`'s and is shared with the
    // blocking session, which is what keeps this a conversion rather than a copy.
    let mut cmd = Command::from(configured_command(spec, Stdio::null()));
    cmd.kill_on_drop(true);

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

/// A **duplex** session with a child: frames in on stdin, lines out on stdout.
///
/// ## Why this exists next to [`run_streaming`] rather than instead of it
///
/// [`run_streaming`] is one-way by construction: it hands the child `stdin(null)` and only
/// delivers stdout, because a delegate-mode turn (`agy --print`, `codex exec`) says
/// everything it has to say up front. The tool loop is the opposite — it *injects* a
/// `tool_result` mid-turn, and the child has to still be reading to receive it. That single
/// difference (`Stdio::piped()` instead of `Stdio::null()`) is why M9 wrote a **second**
/// copy of the spawn policy inside `tool_loop.rs`, and why M11 found it there: a copy of an
/// invariant stops holding silently (`planning/ACCEPTANCE.md` §19, `planning/DESIGN.md` §20).
/// The pipe is the whole difference, so the pipe is all this adds.
///
/// ## What it owns
///
/// Everything the copy owned, once: the child, a **writer thread** for stdin, a **reader
/// thread** for stdout, a **detached** stderr drain, a whole-turn **deadline**, and the
/// **kill tree** on the way out. What it does *not* own is any framing: it moves bytes and
/// lines, so claude's `stream-json`, a `bash` tool's output, and a test's stand-in all use
/// it unchanged.
///
/// ## The two-pipe deadlock, and why it is unreachable here
///
/// The deadlock is: the caller blocks writing a frame into the child's stdin while the
/// child blocks writing into a full stdout pipe nobody is draining. It needs a frame larger
/// than the OS pipe buffer, which a real `tool_result` (a `read` of a large file) is. Three
/// structural choices make it unreachable:
///
/// 1. **[`ChildSession::send_bytes`] never touches the pipe.** It hands the bytes to the
///    writer thread and returns, so the read path can always keep draining stdout — which
///    is what unblocks the child.
/// 2. **A reader thread owns the blocking read**, pushing lines into a channel that
///    [`ChildSession::next_line`] drains. That is what makes the deadline possible at all:
///    a blocking read on a pipe cannot be cancelled, but `recv_timeout` on a channel can.
/// 3. **The frame queue is not drained by anyone but the writer thread**, so a caller cannot
///    block on it. The *caller* bounds how many frames it queues — the loop queues one per
///    answered call and waits for the reply — which is why an unbounded channel is safe
///    here and a bounded one would only add a way to block the read path.
///
/// ```no_run
/// # async fn demo(spec: &pie::process::RunSpec) -> pie::error::Result<()> {
/// use pie::process::ChildSession;
///
/// let mut session = ChildSession::spawn(spec)?;
/// session.send_bytes("{\"type\":\"user\"}\n")?; // queued; the writer thread owns the pipe
/// while let Some(line) = session.next_line()? {
///     println!("{}", line.text);
/// }
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct ChildSession {
    backend: String,
    timeout: Duration,
    child: Child,
    /// Dropped to end the writer thread, which then drops the child's stdin (EOF).
    frames: Option<mpsc::Sender<Vec<u8>>>,
    incoming: Mutex<mpsc::Receiver<Incoming>>,
    /// The first failed write to the child's stdin, recorded by the writer thread.
    writer_error: Arc<Mutex<Option<std::io::Error>>>,
    /// Last bytes of the child's stderr, for a caller that wants to explain a failure.
    stderr_tail: Arc<Mutex<String>>,
    deadline: Instant,
    killed: bool,
}

impl ChildSession {
    /// Spawn `spec` and wire its stdin, stdout and stderr into a session.
    ///
    /// The child is configured by [`configured_command`], which is the one spawn policy —
    /// so the window is hidden, the env is scrubbed, and the process group is the child's
    /// own, exactly as for a delegate turn.
    pub fn spawn(spec: &RunSpec) -> Result<Self> {
        let mut child = configured_command(spec, Stdio::piped())
            .spawn()
            .map_err(|source| PieError::Spawn {
                backend: spec.backend.clone(),
                source,
            })?;

        let stdin = child.stdin.take().ok_or_else(|| {
            PieError::protocol(
                &spec.backend,
                "child produced no stdin handle to write frames to",
            )
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            PieError::protocol(
                &spec.backend,
                "child produced no stdout handle to read events from",
            )
        })?;
        let stderr = child.stderr.take().ok_or_else(|| {
            PieError::protocol(&spec.backend, "child produced no stderr handle to drain")
        })?;

        // `Instant + Duration` panics on overflow and the timeout is an unvalidated `u64`
        // from `--timeout` / `PIE_TIMEOUT_SECS`, so a nonsense value must become a config
        // error (`run_streaming` above turns it into the same one).
        let deadline = Instant::now().checked_add(spec.timeout).ok_or_else(|| {
            PieError::Config(format!(
                "timeout of {}s is too large to schedule",
                spec.timeout.as_secs()
            ))
        })?;

        let writer_error: Arc<Mutex<Option<std::io::Error>>> = Arc::new(Mutex::new(None));
        let stderr_tail: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));

        // stderr must be drained even though nothing reads it: a child that fills a full
        // stderr pipe blocks, and the caller would then wait for stdout that can never come.
        // **Deliberately detached** — joining it in `Drop` would hang on a grandchild that
        // inherited the handle, and a hung drop is worse than a thread that ends when the
        // pipe closes.
        {
            let tail = Arc::clone(&stderr_tail);
            let _ = std::thread::spawn(move || drain_tail_blocking(stderr, tail));
        }

        let (frame_tx, frame_rx) = mpsc::channel::<Vec<u8>>();
        {
            let errors = Arc::clone(&writer_error);
            let mut stdin = stdin;
            let _ = std::thread::spawn(move || {
                while let Ok(bytes) = frame_rx.recv() {
                    if let Err(error) = stdin.write_all(&bytes).and_then(|()| stdin.flush()) {
                        // Recorded, not returned: `send_bytes` has already told the caller
                        // the frame was accepted, so the failure has to be reported from
                        // there — a result the child never read must not be followed by a
                        // cheerful end of stream.
                        if let Ok(mut slot) = errors.lock() {
                            *slot = Some(error);
                        }
                        break;
                    }
                }
                // Dropping `stdin` here closes the child's stdin.
            });
        }

        let (line_tx, line_rx) = mpsc::channel::<Incoming>();
        {
            let backend = spec.backend.clone();
            let mut reader = BlockingBufReader::new(stdout);
            let _ = std::thread::spawn(move || loop {
                match read_capped_line(&mut reader) {
                    Ok(Some(line)) => {
                        if line.truncated {
                            // A line this long signals a broken or hostile stream rather
                            // than a big answer, and staying quiet would make the truncation
                            // look like a short frame (`run_streaming` warns for the same
                            // reason).
                            tracing::warn!(
                                backend = %backend,
                                cap_bytes = MAX_LINE_BYTES,
                                "truncated an oversized stdout line from the child; a line \
                                 longer than the cap cannot be buffered without bound"
                            );
                        }
                        if line_tx.send(Incoming::Line(line)).is_err() {
                            break; // the session is gone
                        }
                    }
                    Ok(None) => break,
                    Err(error) => {
                        let _ = line_tx.send(Incoming::Failed(error));
                        break;
                    }
                }
            });
        }

        Ok(Self {
            backend: spec.backend.clone(),
            timeout: spec.timeout,
            child,
            frames: Some(frame_tx),
            incoming: Mutex::new(line_rx),
            writer_error,
            stderr_tail,
            deadline,
            killed: false,
        })
    }

    /// Queue `bytes` for the child's stdin.
    ///
    /// Returns as soon as the bytes are **queued**, never after they are written — the
    /// writer thread owns the pipe, and that is what keeps this off the read path. The
    /// consequence a caller must know: a failure is reported late, by
    /// [`ChildSession::take_write_error`] on the next send or at end of stream, never as a
    /// blocked call here. A caller that treats "queued" as "delivered" will report a
    /// cheerful success for a conversation the child abandoned.
    pub fn send_bytes(&mut self, bytes: impl Into<Vec<u8>>) -> Result<()> {
        let Some(frames) = &self.frames else {
            return Err(PieError::protocol(
                &self.backend,
                "the child's stdin is already closed, so nothing more can be delivered to it",
            ));
        };

        frames.send(bytes.into()).map_err(|_| {
            PieError::protocol(
                &self.backend,
                "the child's stdin writer stopped before this frame was queued",
            )
        })
    }

    /// The first failed write to the child's stdin, taken once.
    ///
    /// Asked for rather than returned from [`ChildSession::send_bytes`], because the failure
    /// is asynchronous: the writer thread discovers it after the bytes were accepted, so the
    /// caller has to be able to ask later — on the next send, and at end of stream.
    pub fn take_write_error(&self) -> Option<std::io::Error> {
        lock(&self.writer_error).take()
    }

    /// The last bytes of the child's stderr, for a diagnostic.
    pub fn stderr_tail(&self) -> String {
        lock(&self.stderr_tail).clone()
    }

    /// The child's process id, for a diagnostic (`kill_tree` takes the tree from here).
    pub fn pid(&self) -> Option<u32> {
        Some(self.child.id())
    }

    /// The child's exit status, if it has already exited, without blocking.
    ///
    /// `Ok(None)` means "still running", which is the normal answer right after
    /// [`ChildSession::next_line`] reports end of stream: a closed stdout pipe is not an exit
    /// — a child can close it and keep working — so a caller that needs the code polls here
    /// rather than assuming one.
    pub fn try_wait(&mut self) -> Result<Option<std::process::ExitStatus>> {
        self.child.try_wait().map_err(PieError::Io)
    }

    /// The next line from the child, or `None` at end of stream.
    ///
    /// Fails with [`PieError::Timeout`] once the whole-turn deadline passes — and kills the
    /// tree before returning, so a hung child cannot outlive the caller's patience
    /// (`RISKS.md` §6). The stderr tail is logged, redacted (I10), because a timeout with no
    /// explanation is the least actionable failure pie has.
    pub fn next_line(&mut self) -> Result<Option<ChildLine>> {
        // A zero-length `recv_timeout` returns `Timeout` immediately, which would turn a
        // child that exited right at the deadline into a spurious timeout, so the last
        // instant is spent on `try_recv` instead.
        let remaining = self.deadline.checked_duration_since(Instant::now());
        let received = match remaining {
            Some(remaining) if !remaining.is_zero() => lock(&self.incoming).recv_timeout(remaining),
            _ => lock(&self.incoming)
                .try_recv()
                .map_err(|error| match error {
                    mpsc::TryRecvError::Empty => mpsc::RecvTimeoutError::Timeout,
                    mpsc::TryRecvError::Disconnected => mpsc::RecvTimeoutError::Disconnected,
                }),
        };

        match received {
            Ok(Incoming::Line(line)) => Ok(Some(line)),
            Ok(Incoming::Failed(error)) => Err(PieError::Io(error)),
            Err(mpsc::RecvTimeoutError::Disconnected) => Ok(None),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let tail = self.stderr_tail();
                let trimmed = tail.trim();
                if !trimmed.is_empty() {
                    tracing::warn!(
                        backend = %self.backend,
                        secs = self.timeout.as_secs(),
                        stderr = %truncate(&redact(trimmed), 400),
                        "the child went quiet until its deadline"
                    );
                }
                self.kill();
                Err(PieError::Timeout {
                    backend: self.backend.clone(),
                    secs: self.timeout.as_secs(),
                })
            }
        }
    }

    /// Kill the child and everything it spawned, once.
    fn kill(&mut self) {
        if self.killed {
            return;
        }
        self.killed = true;
        kill_tree_blocking(Some(self.child.id()));
        let _ = self.child.wait();
    }
}

impl Drop for ChildSession {
    fn drop(&mut self) {
        // Closing the frame channel ends the writer thread, which drops the child's stdin.
        // Then the tree is killed: a child that outlives its parent keeps a subscription
        // turn running with nobody reading it (`RISKS.md` §6).
        self.frames = None;
        self.kill();
    }
}

/// Resolve a binary name (or path) the way a shell would.
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

/// How long a kill helper may take before it is abandoned.
///
/// The kill runs on the timeout path, so it must not itself block past the deadline: a hung
/// `taskkill` would defeat the timeout it exists to enforce. Both executors of
/// [`kill_plan`] apply it, which is what keeps them one policy rather than two.
const KILL_HELPER_TIMEOUT: Duration = Duration::from_secs(5);

/// How long the tree is given to exit on the polite signal before the brutal one.
const KILL_GRACE: Duration = Duration::from_millis(250);

/// The argv of a tree-kill, per platform.
///
/// `Child::kill` only signals the direct child; vendor CLIs spawn node, subagents, and
/// shells, so we must take the whole tree. The plan is data rather than two functions, so
/// [`kill_tree`] (async, for `run_streaming`) and [`kill_tree_blocking`] (for
/// [`ChildSession`], whose callers are synchronous) cannot drift: a third flag on
/// Windows' `taskkill` reaches both or neither.
///
/// The `bool` is "pause after this step", which only unix uses: a `TERM` gives the tree a
/// chance to exit cleanly before the `KILL` escalates.
///
/// **Deliberately not `#[cfg]`-gated as a whole**, and that is a fix rather than a style
/// choice: the first version carried an outer `#[cfg(windows)]` *and* an inner
/// `#[cfg(not(windows))]` arm, so the function did not exist on unix while both callers
/// below referenced it unconditionally. Windows could not see it (`G1`-`G3` green), the
/// unix cross-check `G6` could, and it went red the first time it was run.
fn kill_plan(pid: u32) -> Vec<(&'static str, Vec<OsString>, bool)> {
    #[cfg(windows)]
    {
        vec![(
            "taskkill",
            vec![
                OsString::from("/PID"),
                OsString::from(pid.to_string()),
                OsString::from("/T"),
                OsString::from("/F"),
            ],
            false,
        )]
    }

    #[cfg(not(windows))]
    {
        // The child leads its own process group (`process_group(0)` in
        // `configured_command`), so a negative pid signals the whole tree, not just the
        // direct child.
        let group = format!("-{pid}");
        vec![
            (
                "kill",
                vec![OsString::from("-TERM"), OsString::from(group.clone())],
                true,
            ),
            (
                "kill",
                vec![OsString::from("-KILL"), OsString::from(group)],
                false,
            ),
        ]
    }
}

/// A kill helper, configured the way every child is: hidden, and without our credentials.
///
/// Even a trusted OS helper gets the scrub (`planning/DESIGN.md` §20's I2): the invariant is
/// about what any child can observe, not about which children we happen to trust.
fn kill_helper(program: &str, args: &[OsString]) -> std::process::Command {
    let mut cmd = std::process::Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    for key in crate::config::BUILTIN_SCRUB {
        cmd.env_remove(key);
    }

    cmd
}

/// Kill a process and everything it spawned.
pub async fn kill_tree(pid: Option<u32>) {
    let Some(pid) = pid else { return };

    for (program, args, pause_after) in kill_plan(pid) {
        // Bounded: `kill_tree` runs on the timeout path, so it must not itself block past
        // the deadline. A hung `taskkill` would defeat the timeout it exists to enforce.
        let _ = tokio::time::timeout(
            KILL_HELPER_TIMEOUT,
            Command::from(kill_helper(program, &args)).status(),
        )
        .await;

        if pause_after {
            // Give the tree a moment to exit cleanly before escalating.
            tokio::time::sleep(KILL_GRACE).await;
        }
    }
}

/// The same kill without a runtime, for a caller that has none.
///
/// [`ChildSession`] is synchronous ([`Transport`](crate::tool_loop::Transport) is), so it
/// cannot `await` the async version — and a `taskkill` that never returns must not hang the
/// drop that runs it, so the helper is polled to [`KILL_HELPER_TIMEOUT`] and then abandoned.
/// Same plan, same bound, same scrub; only the executor differs.
fn kill_tree_blocking(pid: Option<u32>) {
    let Some(pid) = pid else { return };

    for (program, args, pause_after) in kill_plan(pid) {
        if let Ok(mut helper) = kill_helper(program, &args).spawn() {
            let deadline = Instant::now().checked_add(KILL_HELPER_TIMEOUT);
            loop {
                match helper.try_wait() {
                    Ok(Some(_)) | Err(_) => break,
                    Ok(None) => {
                        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                            let _ = helper.kill();
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(20));
                    }
                }
            }
        }

        if pause_after {
            std::thread::sleep(KILL_GRACE);
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

/// One line of a session child's stdout, or the read that failed.
#[derive(Debug)]
enum Incoming {
    /// A complete line, lossily decoded.
    Line(ChildLine),
    /// The reader stopped. Reported rather than treated as a clean end of stream: a read
    /// that failed is not the child saying it is done.
    Failed(std::io::Error),
}

/// Read one line, retaining at most [`MAX_LINE_BYTES`] bytes, lossily — the blocking twin
/// of [`read_line_capped`], for [`ChildSession`]'s reader thread.
///
/// The same two reasons as the async reader, which is why it is not a `lines()` call: a
/// line with no newline must not grow the buffer without bound, and one byte that is not
/// valid UTF-8 must not abort a healthy turn (I5).
fn read_capped_line<R: BufRead>(reader: &mut R) -> std::io::Result<Option<ChildLine>> {
    let cap = MAX_LINE_BYTES;
    let mut out: Vec<u8> = Vec::new();
    let mut saw_any = false;
    let mut truncated = false;

    loop {
        let (mut chunk, consume, newline) = {
            let available = reader.fill_buf()?;
            if available.is_empty() {
                if !saw_any {
                    return Ok(None);
                }
                break; // end of stream: what we have is the last line
            }
            match available.iter().position(|&byte| byte == b'\n') {
                Some(at) => (available[..at].to_vec(), at + 1, true),
                None => (available.to_vec(), available.len(), false),
            }
        };

        saw_any = true;
        let room = cap.saturating_sub(out.len());
        if chunk.len() <= room {
            out.append(&mut chunk);
        } else {
            out.extend_from_slice(&chunk[..room]);
            truncated = true;
        }
        reader.consume(consume);

        if newline {
            break;
        }
    }

    Ok(Some(finish_line(out, truncated)))
}

/// Keep the last [`STDERR_TAIL_BYTES`] of a child's stderr, blocking — the twin of
/// [`drain_tail`] for [`ChildSession`]'s detached stderr thread, for the same reason:
/// stderr must be drained even though nothing reads it, or a child that fills the pipe
/// blocks and the stdout we *are* waiting for can never come.
fn drain_tail_blocking<R: Read>(reader: R, tail: Arc<Mutex<String>>) {
    let mut reader = BlockingBufReader::new(reader);
    let mut kept: Vec<u8> = Vec::new();

    while let Ok(Some(line)) = read_capped_line(&mut reader) {
        kept.extend_from_slice(line.text.as_bytes());
        kept.push(b'\n');
        if kept.len() > STDERR_TAIL_BYTES {
            let excess = kept.len() - STDERR_TAIL_BYTES;
            kept.drain(..excess);
        }
    }

    *lock(&tail) = String::from_utf8_lossy(&kept).into_owned();
}

/// Lock without `unwrap`: a poisoned mutex means another thread panicked, and the data on
/// the other side of these locks is a diagnostics tail, not a broken invariant.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
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
