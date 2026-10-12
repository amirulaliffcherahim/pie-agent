//! Shared turn driver for the CLI backends.
//!
//! Every vendor CLI is driven the same way: build argv, spawn hidden, parse
//! stdout line-by-line into normalised events, and derive the outcome from the
//! terminal event. Backends supply argv and an [`EventRouter`]; they do not
//! touch process plumbing.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::error::{PieError, Result};
use crate::process::{self, RunSpec};
use crate::provider::{Capabilities, StreamEvent, TurnOutcome, Usage};

/// Everything needed to execute one turn of a CLI backend.
#[derive(Debug, Clone)]
pub struct TurnPlan {
    /// Executable to spawn.
    pub program: PathBuf,
    /// argv, in order.
    pub args: Vec<OsString>,
    /// Working directory.
    pub cwd: PathBuf,
    /// Env vars to strip from the child.
    pub scrub: Vec<String>,
    /// Deadline.
    pub timeout: Duration,
}

/// Backend-specific stream interpretation.
///
/// Implementations accumulate state across `on_line` calls and decide the
/// outcome in `outcome`. Keeping this a trait means the parsers are testable
/// from fixtures with no process involved at all.
pub trait EventRouter: Send {
    /// Provider id, for errors.
    fn backend(&self) -> &'static str;

    /// Interpret one stdout line. Return an event to surface, or `None`.
    fn on_line(&mut self, line: &str) -> Option<StreamEvent>;

    /// Build the outcome once stdout has closed.
    ///
    /// `exit_code`/`exit_status`/`stderr_tail` describe the child; a healthy
    /// terminal event wins over a noisy stderr, and a missing terminal event is
    /// an error (`DESIGN.md` §6 rule 7).
    fn outcome(
        &self,
        exit_code: Option<i32>,
        exit_status: &str,
        stderr_tail: &str,
    ) -> Result<TurnOutcome>;
}

/// Run one turn, streaming normalised events into `sink`.
pub async fn drive(
    plan: TurnPlan,
    router: &mut dyn EventRouter,
    sink: &mut (dyn FnMut(StreamEvent) + Send),
    started: Instant,
) -> Result<TurnOutcome> {
    let backend = router.backend().to_string();

    let spec = RunSpec {
        backend: backend.clone(),
        program: plan.program.clone(),
        args: plan.args.clone(),
        cwd: plan.cwd.clone(),
        scrub: plan.scrub.clone(),
        timeout: plan.timeout,
    };

    let summary = {
        let mut on_line = |line: &str| {
            if let Some(event) = router.on_line(line) {
                if let StreamEvent::Unparsed(raw) = &event {
                    // Redacted: this is a vendor stdout line, and with
                    // `PIE_LOG=debug` it goes straight to stderr. A line like
                    // `OPENAI_API_KEY=sk-…` used to be logged verbatim (I10).
                    tracing::debug!(
                        backend = %backend,
                        line = %crate::error::truncate(&process::redact(raw), 200),
                        "stdout line was not JSON"
                    );
                }
                sink(event);
            }
        };
        process::run_streaming(&spec, &mut on_line).await?
    };

    let mut outcome = router.outcome(
        summary.exit_code,
        &summary.exit_status,
        &summary.stderr_tail,
    )?;
    outcome.duration = started.elapsed();

    // Token accounting goes out **after** `Done`, and only here can it be sent
    // whole.
    //
    // `Done` is emitted by the router *during* `on_line`, because it is what
    // makes the answer final. The complete usage figure, by contrast, exists
    // only once the process has exited and `outcome()` has merged every partial
    // report the stream carried, so this driver is the first place that has it.
    // The stream therefore reads `…, done, usage`: `done` stays terminal for the
    // answer, and `usage` is accounting a consumer of `--json` — one JSON object
    // per line, `DESIGN.md` §13 — can read from the last line.
    //
    // Deferring `Done` so that `usage` could precede it was considered and
    // rejected: buffering the terminal event introduces a new failure mode where
    // an error path swallows a `Done` that was already produced, to buy a purely
    // cosmetic ordering.
    //
    // Emitted only when the backend reported something. `Usage::merge`'s
    // documented convention is that a zero field means "not reported", not
    // "reported as zero", so sending `Usage::default()` would assert that the
    // turn cost nothing — a claim no backend made, and the criterion in
    // `planning/ACCEPTANCE.md` §5 asks for accounting that exists.
    if outcome.usage != Usage::default() {
        sink(StreamEvent::Usage(outcome.usage.clone()));
    }

    Ok(outcome)
}

/// Measure the argv as it will reach `CreateProcess`, approximately.
///
/// Windows joins argv into a single UTF-16 command line, so the length must be
/// counted in **UTF-16 code units**, not `char`s: a non-BMP character (an emoji,
/// many CJK extension characters) is 2 units there and 1 `char`, so counting
/// `chars()` under-measured and could let an over-limit prompt through to an
/// opaque `CreateProcessW` failure instead of a clear `PromptTooLong`.
///
/// On unix `execve` receives argv as separate strings and the limit is far
/// higher, so `chars()` is the right measure there.
pub fn argv_char_len(program: &Path, args: &[OsString]) -> usize {
    fn measured(value: &std::ffi::OsStr) -> usize {
        let text = value.to_string_lossy();
        if cfg!(windows) {
            text.encode_utf16().count()
        } else {
            text.chars().count()
        }
    }

    let mut total = measured(program.as_os_str());
    for arg in args {
        // Two quotes plus a separator, as before: over-estimating is the safe
        // direction for a hard cap.
        total += measured(arg) + 3;
    }
    total
}

/// Refuse a turn whose argv would exceed the cap.
///
/// Used only by backends with no stdin prompt path — for those, the prompt is
/// argv or nothing (`RISKS.md` §5).
pub fn enforce_argv_cap(
    backend: &str,
    program: &Path,
    args: &[OsString],
    cap: usize,
    hint: &str,
) -> Result<()> {
    if cap == 0 {
        return Ok(());
    }
    let len = argv_char_len(program, args);
    if len > cap {
        return Err(PieError::PromptTooLong {
            backend: backend.to_string(),
            len,
            max: cap,
            hint: hint.to_string(),
        });
    }
    Ok(())
}

/// Effective cap for a backend, honouring both the capability and the config.
pub fn cap_for(caps: &Capabilities, cfg: &crate::config::Config) -> usize {
    cfg.effective_argv_cap(caps)
}

/// Run a plan and collect every stdout line.
///
/// For **non-turn** commands only — listing models, probing quota. Turn output
/// must go through [`drive`] so events stream as they arrive rather than being
/// buffered.
///
/// A non-zero exit is only an error when nothing was printed: some CLIs exit
/// non-zero after emitting a perfectly good payload.
pub async fn collect_lines(backend: &str, plan: &TurnPlan) -> Result<Vec<String>> {
    let spec = RunSpec {
        backend: backend.to_string(),
        program: plan.program.clone(),
        args: plan.args.clone(),
        cwd: plan.cwd.clone(),
        scrub: plan.scrub.clone(),
        timeout: plan.timeout,
    };

    let mut lines: Vec<String> = Vec::new();
    let mut push = |line: &str| lines.push(line.to_string());
    let summary = process::run_streaming(&spec, &mut push).await?;

    if lines.is_empty() && !summary.success() {
        return Err(PieError::Backend {
            backend: backend.to_string(),
            status: summary.exit_status.clone(),
            detail: process::diagnostic(&summary.stderr_tail),
        });
    }

    Ok(lines)
}

/// Ask a backend for its version string, with a short deadline.
///
/// Used by `--list-backends` and `doctor` for drift detection (`RISKS.md` §3).
/// Never spends model quota; a failure yields `None` rather than an error, so a
/// single unresponsive CLI cannot break the table.
pub async fn probe_version(
    provider: &dyn crate::provider::Provider,
    cfg: &crate::config::Config,
    timeout: Duration,
) -> Option<String> {
    let (program, args) = provider.version_command(cfg)?;
    probe_version_owned(
        program,
        args,
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        cfg.scrub_keys(cfg.scrub_enabled()),
        timeout,
    )
    .await
}

/// The same probe over **owned** inputs, so callers can run several concurrently.
///
/// Each probe costs roughly half a second of process startup, so three sequential
/// probes made `--list-backends` and `doctor` feel sluggish for no reason. Owned
/// arguments are what let `tokio::task::JoinSet` spawn them in parallel.
pub async fn probe_version_owned(
    program: PathBuf,
    args: Vec<OsString>,
    cwd: PathBuf,
    scrub: Vec<String>,
    timeout: Duration,
) -> Option<String> {
    let spec = RunSpec {
        backend: "version-probe".to_string(),
        program,
        args,
        cwd,
        scrub,
        timeout,
    };

    let mut first: Option<String> = None;
    let mut capture = |line: &str| {
        if first.is_none() {
            let trimmed = line.trim();
            if !trimmed.is_empty() {
                first = Some(trimmed.to_string());
            }
        }
    };

    match process::run_streaming(&spec, &mut capture).await {
        Ok(_) => first,
        Err(e) => {
            tracing::debug!(error = %e, "version probe failed");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::config::Config;

    fn args(items: &[&str]) -> Vec<OsString> {
        items.iter().map(OsString::from).collect()
    }

    #[test]
    fn argv_len_grows_with_arguments() {
        let short = argv_char_len(Path::new("agy"), &args(&["--print", "hi"]));
        let long = argv_char_len(Path::new("agy"), &args(&["--print", &"x".repeat(1_000)]));
        assert!(long > short + 900);
    }

    #[test]
    fn cap_is_enforced_with_a_helpful_error() {
        let big = "p".repeat(30_000);
        let argv = args(&["--print", &big]);

        let err =
            enforce_argv_cap("agy", Path::new("agy"), &argv, 20_000, "shorten it").unwrap_err();
        assert_eq!(err.exit_code(), 2, "prompt cap is a usage error");
        match err {
            PieError::PromptTooLong {
                backend,
                len,
                max,
                hint,
            } => {
                assert_eq!(backend, "agy");
                assert!(len > max);
                assert_eq!(max, 20_000);
                assert_eq!(hint, "shorten it");
            }
            other => panic!("wrong error: {other:?}"),
        }

        // under the cap is fine
        assert!(enforce_argv_cap(
            "agy",
            Path::new("agy"),
            &args(&["--print", "hi"]),
            20_000,
            "h"
        )
        .is_ok());
        // 0 means unlimited
        assert!(enforce_argv_cap("agy", Path::new("agy"), &argv, 0, "h").is_ok());
    }

    #[test]
    fn cap_for_takes_the_stricter_side() {
        let mut cfg = Config::default();
        let agy_caps = Capabilities {
            max_prompt_argv: 20_000,
            ..Capabilities::default()
        };
        assert_eq!(cap_for(&agy_caps, &cfg), 20_000);

        cfg.general.prompt_argv_cap = 100;
        assert_eq!(cap_for(&agy_caps, &cfg), 100);
    }
}
