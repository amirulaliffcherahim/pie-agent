//! The two latency caches, and the reason they exist.
//!
//! Both are pure performance: neither changes what pie does, only whether it has
//! to spawn a vendor CLI to learn something it already knew. Measured
//! 2026-10-12 on this machine:
//!
//! * `agy --print /usage` — a **free** slash expansion that still costs agy about
//!   5–10 s of its own initialisation, for a number that moves slowly.
//! * three `--version` probes — 1.54 s in total, and parallelising them does *not*
//!   help (1.46 s), because the cost is Windows process creation plus antivirus
//!   inspection rather than anything pie controls.
//!
//! So the tests here are about cache behaviour, not about parsing: a fresh entry
//! must avoid the spawn, a stale one must not be served, and a cached reading must
//! always say that it is cached — a silently stale quota figure is worse than a
//! slow one.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::process::Command;

/// The unique text of the cache note.
///
/// Asserting on the bare word `cached` is wrong: the session-log totals below the
/// table print "cached input", so the check passes or fails for the wrong reason.
/// Learned the hard way in these very tests.
const CACHE_NOTE_MARKER: &str = "to probe every time";

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// A stand-in `agy` that appends a line to `calls.txt` every time it runs and
/// then prints `tests/fixtures/agy_usage.json`.
///
/// The counter is the whole point: it distinguishes "served from cache" from
/// "spawned the CLI again", which no assertion on the output alone can do.
fn counting_stand_in(dir: &Path) -> PathBuf {
    let payload = repo_root()
        .join("tests")
        .join("fixtures")
        .join("agy_usage.json");
    let counter = dir.join("calls.txt");

    #[cfg(windows)]
    let script = dir.join("counting-agy.bat");
    #[cfg(not(windows))]
    let script = dir.join("counting-agy.sh");

    #[cfg(windows)]
    let body = format!(
        "@echo off\r\necho call>> \"{}\"\r\ntype \"{}\"\r\n",
        counter.display(),
        payload.display()
    );
    #[cfg(not(windows))]
    let body = format!(
        "#!/bin/sh\necho call >> '{}'\ncat '{}'\n",
        counter.display(),
        payload.display()
    );

    std::fs::write(&script, body).expect("write the counting stand-in");

    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
            .expect("chmod the counting stand-in");
    }

    script
}

fn invocations(dir: &Path) -> usize {
    std::fs::read_to_string(dir.join("calls.txt"))
        .map(|text| text.lines().count())
        .unwrap_or(0)
}

/// Run `pie usage agy` with an isolated `PIE_HOME`, returning (stdout, spawns).
/// Spawns are counted inside the stand-in, not inferred.
fn run_usage(home: &Path, agy: &Path) -> (String, usize) {
    let output = Command::new(env!("CARGO_BIN_EXE_pie"))
        .args(["usage", "agy"])
        .env("PIE_HOME", home)
        .env("PIE_AGY_BIN", agy)
        .output()
        .expect("failed to run the pie binary");

    assert!(
        output.status.success(),
        "`pie usage agy` failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        invocations(agy.parent().unwrap()),
    )
}

#[test]
fn the_quota_probe_is_probed_once_and_then_served_from_cache() {
    let home = tempfile::tempdir().unwrap();
    let stand_in_dir = tempfile::tempdir().unwrap();
    let agy = counting_stand_in(stand_in_dir.path());

    let (first, spawns_first) = run_usage(home.path(), &agy);
    assert_eq!(spawns_first, 1, "the first call must probe");
    assert!(
        first.contains("Gemini Models / weekly"),
        "expected real buckets:\n{first}"
    );
    assert!(
        !first.contains(CACHE_NOTE_MARKER),
        "a freshly probed reading must not claim to be cached:\n{first}"
    );

    let (second, spawns_after) = run_usage(home.path(), &agy);
    assert_eq!(
        spawns_after, 1,
        "the second call must NOT spawn: the cache was supposed to absorb it"
    );
    assert!(
        second.contains(CACHE_NOTE_MARKER),
        "a cached reading must say so, or a stale quota figure is mistaken for a \
         live one:\n{second}"
    );
    // The data still has to be there; caching must not lose the payload.
    assert!(second.contains("Gemini Models / weekly"), "{second}");
}

#[test]
fn a_zero_ttl_disables_the_cache_and_always_probes() {
    let home = tempfile::tempdir().unwrap();
    let stand_in_dir = tempfile::tempdir().unwrap();
    let agy = counting_stand_in(stand_in_dir.path());

    std::fs::create_dir_all(home.path()).unwrap();
    std::fs::write(
        home.path().join("config.toml"),
        "[agy]\nusage_cache_ttl_secs = 0\n",
    )
    .unwrap();

    let (_, first) = run_usage(home.path(), &agy);
    let (second_output, second) = run_usage(home.path(), &agy);

    assert_eq!(first, 1);
    assert_eq!(second, 2, "ttl = 0 must probe every time");
    assert!(
        !second_output.contains(CACHE_NOTE_MARKER),
        "nothing was cached, so nothing may claim to be:\n{second_output}"
    );
}

/// A quota reading with no buckets is a failure or an empty payload, not a fact
/// worth remembering: caching it would turn a transient problem into five minutes
/// of misleading "no buckets".
#[test]
fn an_empty_probe_is_not_cached() {
    let home = tempfile::tempdir().unwrap();
    let stand_in_dir = tempfile::tempdir().unwrap();

    // A stand-in that returns a well-formed envelope with no quota payload.
    let empty = stand_in_dir.path().join("empty-agy.bat");
    let _ = empty;
    let script = {
        #[cfg(windows)]
        let path = stand_in_dir.path().join("empty-agy.bat");
        #[cfg(not(windows))]
        let path = stand_in_dir.path().join("empty-agy.sh");

        #[cfg(windows)]
        std::fs::write(&path, "@echo off\r\necho {\"status\":\"SUCCESS\"}\r\n").unwrap();
        #[cfg(not(windows))]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::write(&path, "#!/bin/sh\necho '{\"status\":\"SUCCESS\"}'\n").unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    };

    let (output, _) = run_usage(home.path(), &script);
    assert!(
        output.contains("no buckets"),
        "an empty payload must be reported as missing, not as zero:\n{output}"
    );

    let cache = home.path().join("cache").join("agy-usage.json");
    assert!(
        !cache.is_file(),
        "an empty reading must not be cached at {}",
        cache.display()
    );
}
