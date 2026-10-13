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
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use pie::error::PieError;
use pie::process::{run_streaming, ChildSession, RunSpec, RunSummary};

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

// ---------------------------------------------------------------------------
// The duplex session: the piped-stdin case `run_streaming` cannot serve
// ---------------------------------------------------------------------------

/// The one difference that made M9's copy necessary, asserted rather than assumed: a
/// session's child has an **open stdin**.
///
/// `run_streaming` hands its child `stdin(null)` — a delegate turn is one-way by
/// construction. The tool loop cannot work that way: it injects a `tool_result` mid-turn,
/// and the child has to still be reading to receive it. So the stand-in here reads one line
/// from its stdin and echoes it back, and the assertion is on the *content* it read. With
/// `stdin(null)` the read returns end of stream, the echo comes back empty, and this test
/// fails — which is the failure the whole `ChildSession` exists to prevent.
#[test]
fn a_session_keeps_the_childs_stdin_open_and_carries_a_line_across_it() {
    let dir = tempfile::tempdir().unwrap();

    #[cfg(windows)]
    let path = dir.path().join("echo-line.bat");
    #[cfg(not(windows))]
    let path = dir.path().join("echo-line.sh");

    // A batch file expands `%line%` per line, so the `set /p` above is visible to the
    // `echo` below; that is why this is a script rather than one `cmd /C` line.
    #[cfg(windows)]
    let body = "@echo off\r\nset /p line=\r\necho got:%line%\r\n";
    #[cfg(not(windows))]
    let body = "#!/bin/sh\nread -r line\nprintf 'got:%s\\n' \"$line\"\n";

    std::fs::write(&path, body).unwrap();
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let spec = RunSpec {
        backend: "test-session".to_string(),
        program: path,
        args: Vec::new(),
        cwd: dir.path().to_path_buf(),
        scrub: Vec::new(),
        timeout: Duration::from_secs(30),
    };

    let mut session = ChildSession::spawn(&spec).expect("the stand-in spawns");
    session
        .send_bytes("hello from pie\n")
        .expect("the line is queued for the child's stdin");

    let mut seen: Vec<String> = Vec::new();
    while let Some(line) = session.next_line().expect("the session reads a line") {
        seen.push(line.text.clone());
        if seen.iter().any(|line| line.contains("got:hello from pie")) {
            break;
        }
    }

    assert!(
        seen.iter().any(|line| line.contains("got:hello from pie")),
        "the child read nothing before it answered, so its stdin was not a pipe: {seen:?}"
    );
}

/// The deadline is the session's own, and it fires even though nothing is arriving.
///
/// A blocking read on a pipe cannot be cancelled; `recv_timeout` on the reader thread's
/// channel can, which is what makes a whole-turn deadline possible at all. A child that
/// goes quiet has to become a *failure* within the deadline rather than a hung caller.
#[test]
fn a_session_that_hears_nothing_stops_at_its_deadline() {
    // `ping` writes a banner line before it settles into one per second, so its stdout is
    // discarded: this test is about a child that says *nothing* until its deadline.
    let sleeper = if cfg!(windows) {
        "ping -n 30 127.0.0.1 > nul"
    } else {
        "sleep 30"
    };

    let started = Instant::now();
    let spec = spec_for(sleeper, Duration::from_millis(400), Vec::new());
    let mut session = ChildSession::spawn(&spec).expect("spawn");
    let error = session
        .next_line()
        .expect_err("a child that says nothing is a failed run, not a hung one");

    match error {
        PieError::Timeout { backend, secs } => {
            assert_eq!(backend, "test-shell");
            assert_eq!(secs, 0, "400ms truncates to 0 whole seconds");
        }
        other => panic!("expected a timeout, got {other:?}"),
    }

    assert!(
        started.elapsed() < Duration::from_secs(10),
        "a 400ms deadline took {:?} — the child was not killed",
        started.elapsed()
    );
}

// ---------------------------------------------------------------------------
// The count of spawn policies is one
// ---------------------------------------------------------------------------

/// **The spawn policy exists once, and this is what says so.**
///
/// M9 wrote a second copy of it inside `tool_loop.rs` — hidden window, process group, env
/// scrub, deadline, kill tree — because `run_streaming` hands the child `stdin(null)` and
/// the loop needs a pipe. M10 recorded the risk (`ACCEPTANCE.md` §19): a mirrored invariant
/// can stop holding without anything failing, because nothing fails when the copy drifts.
/// `DESIGN.md` §20 folds the copy into `process.rs`, and a `bash` tool would otherwise have
/// been a third. This is the assertion that the count is one, rather than the assumption.
///
/// Modelled on `tests/profile.rs::nothing_under_src_reads_or_writes_vendor_config`, including
/// the two ways it keeps the scan honest: a scan that found no files, or that never looked at
/// the file it reasons about, reports zero offenders for reasons that have nothing to do with
/// the rule. So the sources must be found, both files must be among them, and `process.rs`
/// must itself contain the marker — otherwise deleting the spawn outright would make this
/// pass.
#[test]
fn src_spawns_children_in_exactly_one_module() {
    const MARKER: &str = "Command::new";
    const THE_ONE: &str = "src/process.rs";

    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut sources = Vec::new();
    rust_sources_in(&root.join("src"), &mut sources);

    assert!(sources.len() > 10, "the scan found no sources: {sources:?}");
    for expected in [THE_ONE, "src/tool_loop.rs"] {
        assert!(
            sources
                .iter()
                .any(|path| relative_to(&root, path) == expected),
            "the scan never looked at {expected}, so it cannot say anything about it: {sources:?}"
        );
    }
    assert!(
        std::fs::read_to_string(root.join(THE_ONE))
            .unwrap()
            .contains(MARKER),
        "{THE_ONE} does not spawn anything, so `everything else must not` is vacuous"
    );

    let mut offenders: Vec<String> = Vec::new();
    for path in &sources {
        let file = relative_to(&root, path);
        if file == THE_ONE {
            continue;
        }
        let text = std::fs::read_to_string(path).unwrap();
        for (index, line) in text.lines().enumerate() {
            // A comment that documents the rule is not a second copy of the policy.
            if line.trim_start().starts_with("//") {
                continue;
            }
            if line.contains(MARKER) {
                offenders.push(format!("{file}:{} {}", index + 1, line.trim()));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "a child may only be spawned by {THE_ONE} (DESIGN.md §20, \"One spawn policy, not \
         three\"); every other site is a copy of the guarantee free to drift from it: \
         {offenders:?}"
    );
}

/// **The one policy still carries every clause, and nothing else can make it fail.**
///
/// This is a **text** assertion, and it says so rather than pretending otherwise. The two
/// clauses it guards are the ones no test in this repository can observe:
///
/// - `CREATE_NO_WINDOW` is invisible from inside a test process. A child spawned from a
///   console-attached test run shows no window either way; the flag only matters when the
///   parent has no console at all, which is the GUI/detached case a suite cannot be.
/// - `process_group(0)` is what makes a kill reach the *tree*. On unix its absence shows up
///   only as a slow failure (`a_session_that_hears_nothing_stops_at_its_deadline` would sit
///   on `wait()` for the child's full 30 seconds), and on Windows it is not compiled at all.
///
/// Both were removed by hand to check this: every other test in both binaries stayed green.
/// A gate nobody has seen fail is a gate nobody knows works, so the clause list is asserted
/// here — against the one function, which is the other half of "the policy exists once".
#[test]
fn the_one_spawn_policy_still_carries_every_clause() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let source = std::fs::read_to_string(root.join("src/process.rs")).unwrap();
    let policy = function_body(&source, "fn configured_command(");

    for (clause, why) in [
        (
            "creation_flags(CREATE_NO_WINDOW)",
            "I1: a child must never show a console window",
        ),
        (
            "process_group(0)",
            "I1/RISKS.md §6: the kill must reach the child's whole tree, not just the child",
        ),
        ("env_remove", "I2: no API-key env var may reach a child"),
    ] {
        assert!(
            policy.contains(clause),
            "`configured_command` no longer applies `{clause}` ({why})\n{policy}"
        );
    }

    // The policy must be applied to the spawn itself, not merely present in the file: a
    // `configured_command` that built a command nobody spawned would satisfy the above.
    assert!(
        source.contains("configured_command(spec, Stdio::null())")
            && source.contains("configured_command(spec, Stdio::piped())"),
        "both entry points (the delegate turn and the duplex session) must go through the one \
         policy; `DESIGN.md` §20 asks for one spawn path, not one declaration of it"
    );
}

/// The body of a top-level `fn`, found by signature and cut at the first closing brace that
/// starts a line.
fn function_body(source: &str, signature: &str) -> String {
    let start = source
        .find(signature)
        .unwrap_or_else(|| panic!("`{signature}` is not in the source any more"));
    let rest = &source[start..];
    let end = rest.find("\n}\n").unwrap_or(rest.len());
    rest[..end].to_string()
}

fn relative_to(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
        .replace('\\', "/")
}

fn rust_sources_in(dir: &Path, into: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_sources_in(&path, into);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            into.push(path);
        }
    }
}
