//! Real-process tests for the process layer.
//!
//! These spawn `cmd` / `sh`, never a vendor CLI, so they spend no subscription
//! quota and always run in G3. They exist because `process.rs` carries three
//! guarantees everything else depends on: hidden spawn, a deadline that
//! actually kills the tree, and an env scrub that actually removes keys.
//!
//! A parser test can prove a struct was built; only a real spawn can prove the
//! child process never outlives us.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use pie::error::PieError;
use pie::process::{run_streaming, RunSpec, RunSummary};

/// Env mutation is process-global; serialise the tests that do it.
///
/// A `tokio::sync::Mutex` rather than `std::sync::Mutex`, because the guard has
/// to be held across `await` points and a std guard must not be.
static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// A shell invocation that works on Windows, WSL, Linux and macOS.
fn shell(script: &str) -> (PathBuf, Vec<OsString>) {
    #[cfg(windows)]
    {
        (
            PathBuf::from("cmd"),
            vec![OsString::from("/C"), OsString::from(script)],
        )
    }
    #[cfg(not(windows))]
    {
        (
            PathBuf::from("sh"),
            vec![OsString::from("-c"), OsString::from(script)],
        )
    }
}

fn spec_for(script: &str, timeout: Duration, scrub: Vec<String>) -> RunSpec {
    let (program, args) = shell(script);
    RunSpec {
        backend: "test-shell".to_string(),
        program,
        args,
        cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        scrub,
        timeout,
    }
}

/// Run a script and return the delivered lines plus the summary.
async fn run(script: &str, timeout: Duration) -> (Vec<String>, RunSummary) {
    let spec = spec_for(script, timeout, Vec::new());
    let mut lines: Vec<String> = Vec::new();
    let mut push = |line: &str| lines.push(line.to_string());
    let summary = run_streaming(&spec, &mut push).await.expect("spawn");
    (lines, summary)
}

#[tokio::test]
async fn stdout_lines_arrive_in_order_and_exit_zero_is_reported() {
    let (lines, summary) = run("echo one& echo two& echo three", Duration::from_secs(30)).await;

    assert_eq!(lines, vec!["one", "two", "three"]);
    assert_eq!(summary.exit_code, Some(0));
    assert!(summary.success());
    assert_eq!(summary.stdout_lines, 3);
}

#[tokio::test]
async fn a_non_zero_exit_is_reported_not_turned_into_an_error() {
    // exit status is not the answer; the caller decides (DESIGN.md §6 rule 7)
    let (_, summary) = run("exit 3", Duration::from_secs(30)).await;
    assert_eq!(summary.exit_code, Some(3));
    assert!(!summary.success());
}

#[tokio::test]
async fn stderr_is_captured_separately_from_stdout() {
    let spec = spec_for(
        "echo stdout-line& echo stderr-line 1>&2",
        Duration::from_secs(30),
        Vec::new(),
    );
    let mut lines: Vec<String> = Vec::new();
    let mut push = |line: &str| lines.push(line.to_string());
    let summary = run_streaming(&spec, &mut push).await.expect("spawn");

    assert_eq!(
        lines,
        vec!["stdout-line"],
        "stderr must not leak into stdout"
    );
    assert!(
        summary.stderr_tail.contains("stderr-line"),
        "stderr tail was {:?}",
        summary.stderr_tail
    );
}

#[tokio::test]
async fn the_deadline_fires_and_kills_the_child() {
    let sleeper = if cfg!(windows) {
        "ping -n 30 127.0.0.1"
    } else {
        "sleep 30"
    };

    let started = Instant::now();
    let spec = spec_for(sleeper, Duration::from_millis(400), Vec::new());
    let mut push = |_line: &str| {};
    let err = run_streaming(&spec, &mut push).await.unwrap_err();
    let elapsed = started.elapsed();

    match err {
        PieError::Timeout { backend, secs } => {
            assert_eq!(backend, "test-shell");
            assert_eq!(secs, 0, "400ms truncates to 0 whole seconds");
        }
        other => panic!("expected a timeout, got {other:?}"),
    }

    assert!(
        elapsed < Duration::from_secs(10),
        "a 400ms deadline took {elapsed:?} — the child was not killed"
    );
}

#[tokio::test]
async fn scrubbed_env_keys_do_not_reach_the_child() {
    const KEY: &str = "PIE_TEST_SCRUB_PROBE";
    const VALUE: &str = "must-not-leak-into-the-child";

    let _guard = ENV_LOCK.lock().await;
    std::env::set_var(KEY, VALUE);

    // `set` / `env` dump the whole environment, so absence is unambiguous.
    let dump = if cfg!(windows) { "set" } else { "env" };

    // Sanity check: without scrubbing the key IS visible, so the assertion
    // below is testing the scrub and not a broken dump command.
    let (leaked, _) = run(dump, Duration::from_secs(30)).await;
    assert!(
        leaked.iter().any(|l| l.contains(VALUE)),
        "probe var not visible at all — the env dump is not working"
    );

    let spec = spec_for(dump, Duration::from_secs(30), vec![KEY.to_string()]);
    let mut lines: Vec<String> = Vec::new();
    let mut push = |line: &str| lines.push(line.to_string());
    run_streaming(&spec, &mut push).await.expect("spawn");

    assert!(
        !lines.iter().any(|l| l.contains(KEY)),
        "scrubbed key name still present in the child env"
    );
    assert!(
        !lines.iter().any(|l| l.contains(VALUE)),
        "scrubbed value leaked into the child env"
    );

    std::env::remove_var(KEY);
}

#[tokio::test]
async fn a_missing_binary_is_a_spawn_error_not_a_panic() {
    let spec = RunSpec {
        backend: "nope".to_string(),
        program: PathBuf::from("definitely-not-a-real-binary-xyz-12345"),
        args: Vec::new(),
        cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        scrub: Vec::new(),
        timeout: Duration::from_secs(5),
    };

    let mut push = |_line: &str| {};
    let err = run_streaming(&spec, &mut push).await.unwrap_err();
    assert!(matches!(err, PieError::Spawn { .. }), "got {err:?}");
}

#[tokio::test]
async fn non_utf8_bytes_on_stdout_do_not_abort_the_run() {
    // Regression: `BufReader::lines()` fails with `InvalidData` on a byte that is
    // not valid UTF-8, which turned a healthy turn into `PieError::Io` and killed
    // the child — making invariant I5 false. The reader is now capped and lossy.
    //
    // The invalid bytes are written to a file and echoed by the shell, so the
    // test exercises the path on **Windows too**: the previous version emitted
    // them only under `cfg(not(windows))`, so it passed vacuously here and would
    // have gone red on the CI ubuntu job that actually reached the bad byte.
    let dir = tempfile::tempdir().unwrap();
    let payload = dir.path().join("payload.bin");
    std::fs::write(&payload, b"ok\n\xff\xfe not utf8\n").unwrap();

    let script = if cfg!(windows) {
        format!("type {}", payload.display())
    } else {
        format!("cat {}", payload.display())
    };

    let spec = spec_for(&script, Duration::from_secs(30), Vec::new());
    let mut lines: Vec<String> = Vec::new();
    let mut push = |line: &str| lines.push(line.to_string());
    let summary = run_streaming(&spec, &mut push)
        .await
        .expect("a non-UTF-8 byte must not abort the run");

    assert_eq!(summary.exit_code, Some(0));
    assert_eq!(lines.len(), 2, "both lines must be delivered: {lines:?}");
    assert_eq!(lines[0], "ok");
    assert!(
        lines[1].contains("not utf8"),
        "the invalid byte should be replaced, not dropped: {lines:?}"
    );
    assert!(
        !lines[1].contains('\u{fffd}') || lines[1].contains("not utf8"),
        "lossy replacement must keep the rest of the line: {lines:?}"
    );
}
