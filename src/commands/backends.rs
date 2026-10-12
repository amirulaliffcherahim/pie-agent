//! `pie --list-backends` and `pie doctor`.
//!
//! Both print the same backend table: id, resolved binary, version, auth state.
//! `doctor` additionally reports local state and writes the version snapshot
//! used for CLI-drift detection (`RISKS.md` §3).
//!
//! Neither command calls a model, so neither spends subscription quota (I7).

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use crate::config::Config;
use crate::error::{truncate, Result};
use crate::paths;
use crate::process;
use crate::provider;
use crate::session_log::now_millis;
use crate::VERSION;

/// Columns: `id | binary | version | auth | detail`.
const ID_WIDTH: usize = 9;
const BIN_WIDTH: usize = 34;
const VERSION_WIDTH: usize = 13;
const AUTH_WIDTH: usize = 15;

/// A version probe must never hang the table.
const VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// How long a recorded version snapshot is trusted for `--list-backends`.
///
/// The three probes cost about 1.5 s in total, and parallelising them does **not**
/// help: measured 2026-10-12, the probes sum to 1.54 s sequentially and 1.46 s
/// concurrently, because the cost is Windows process creation plus antivirus
/// inspection rather than anything pie controls. Not spawning at all is the only
/// fix, so the snapshot written by `doctor` is reused. Six hours is safe because a
/// CLI only changes when it updates, and `doctor` always re-probes.
const VERSION_SNAPSHOT_TTL: Duration = Duration::from_secs(6 * 60 * 60);

/// `pie --list-backends`
pub async fn list(cfg: &Config) -> Result<()> {
    let _ = table(cfg, false).await?;
    Ok(())
}

/// `pie doctor`
pub async fn doctor(cfg: &Config) -> Result<()> {
    // Always re-probes: this is the command that is *supposed* to be authoritative
    // about what is installed, and it is the one that refreshes the snapshot
    // `--list-backends` reads.
    let versions = table(cfg, true).await?;
    println!();
    report_local_state(cfg);

    match write_version_snapshot(&versions) {
        Ok(path) => println!("snapshot       {}", path.display()),
        // A failed snapshot must not fail `doctor`; the table above is the
        // part that matters.
        Err(e) => println!("snapshot       failed: {e}"),
    }

    Ok(())
}

/// Print the backend table and return `(id, version)` pairs.
async fn table(cfg: &Config, refresh: bool) -> Result<Vec<(String, String)>> {
    println!(
        "{:<ID_WIDTH$} {:<BIN_WIDTH$} {:<VERSION_WIDTH$} {:<AUTH_WIDTH$} DETAIL",
        "BACKEND", "BINARY", "VERSION", "AUTH"
    );

    // Prefer the snapshot `doctor` wrote. Spawning three vendor CLIs costs about
    // 1.5 s (process creation plus antivirus inspection), and parallelising them
    // does not help, so the only real fix is not to spawn when the answer is
    // already known.
    let cached = if refresh {
        None
    } else {
        read_fresh_version_snapshot()
    };
    let stale_note = cached.as_ref().map(|(age, _)| *age);

    let probed = if let Some((_, versions)) = cached {
        versions
    } else {
        versions_in_parallel(cfg).await
    };

    let mut versions: Vec<(String, String)> = Vec::new();

    for p in provider::registry() {
        let id = p.id();

        let binary = match p.binary(cfg) {
            Some(path) => short_path(&path.display().to_string(), BIN_WIDTH),
            // A backend with no binary is **not a missing one** (M10). OpenRouter and
            // DeepSeek are not subprocesses at all, and "(not found: openrouter)" told the
            // reader the wrong story twice over: that a binary was expected, and that this
            // machine failed to have it. The distinction the cell has to carry is
            // "there is nothing to look for".
            None if is_subprocess_backend(id) => {
                format!("(not found: {})", configured_bin(cfg, id))
            }
            None => "(no binary: API endpoint)".to_string(),
        };

        // Redacted before it is printed or written into the version snapshot: a
        // CLI whose `--version` echoed a credential would otherwise be persisted
        // into ~/.pie/cache/versions.json (I10).
        let version = probed
            .get(id)
            .map(|raw| process::redact(raw))
            .unwrap_or_else(|| "unknown".to_string());

        let auth = p.auth_status(cfg).await;

        println!(
            "{:<ID_WIDTH$} {:<BIN_WIDTH$} {:<VERSION_WIDTH$} {:<AUTH_WIDTH$} {}",
            id,
            binary,
            truncate(&version, VERSION_WIDTH),
            auth.label(),
            auth.detail().unwrap_or_default()
        );

        versions.push((id.to_string(), version));
    }

    // Said out loud, so a recorded version is never mistaken for a live probe.
    // Drift detection (I8) depends on knowing which one you are looking at.
    if let Some(age_secs) = stale_note {
        println!();
        println!(
            "versions are from a snapshot taken {age_secs}s ago; run `pie doctor` to re-probe"
        );
    }

    Ok(versions)
}

/// The recorded version snapshot, if it is still fresh.
///
/// Returns `(age_seconds, id → version)`. A missing, stale, unreadable or
/// wrong-shaped file is simply `None`, which makes the caller re-probe.
fn read_fresh_version_snapshot() -> Option<(u64, HashMap<String, String>)> {
    let text = std::fs::read_to_string(paths::version_snapshot()).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;

    let generated_at = crate::jsonl::u(&value, "generated_at")?;
    let age_secs = crate::session_log::now_millis().saturating_sub(generated_at) / 1000;
    if age_secs > VERSION_SNAPSHOT_TTL.as_secs() {
        return None;
    }

    // `backends` is a JSON **object** (`{"agy": "1.3.3", …}`), which is what
    // `write_version_snapshot` produces. Reading it with an array helper silently
    // yielded nothing, so the snapshot was never used and the command kept
    // probing — the same class of mistake as B6: a shape assumed rather than
    // checked.
    let mut versions = HashMap::new();
    if let Some(serde_json::Value::Object(map)) = crate::jsonl::at(&value, "backends") {
        for (id, version) in map {
            if let Some(text) = version.as_str() {
                versions.insert(id.clone(), text.to_string());
            }
        }
    }

    // An empty snapshot is not worth trusting over a fresh probe.
    if versions.is_empty() {
        return None;
    }

    Some((age_secs, versions))
}

/// Every backend's version, probed concurrently.
///
/// A probe that fails or is unavailable is simply absent from the map, which the
/// caller renders as `unknown` — one unresponsive CLI must not break the table.
async fn versions_in_parallel(cfg: &Config) -> HashMap<String, String> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let scrub = cfg.scrub_keys(cfg.scrub_enabled());

    let mut jobs = tokio::task::JoinSet::new();

    for p in provider::registry() {
        let Some((program, args)) = p.version_command(cfg) else {
            continue;
        };
        let id = p.id().to_string();
        let (cwd, scrub) = (cwd.clone(), scrub.clone());

        jobs.spawn(async move {
            let version = provider::driver::probe_version_owned(
                program,
                args,
                cwd,
                scrub,
                VERSION_PROBE_TIMEOUT,
            )
            .await;
            (id, version)
        });
    }

    let mut found = HashMap::new();
    while let Some(joined) = jobs.join_next().await {
        if let Ok((id, Some(version))) = joined {
            found.insert(id, version);
        }
    }
    found
}

/// The `doctor` footer: local paths, version, and the env scrub in effect.
fn report_local_state(cfg: &Config) {
    let config_path = paths::config_file();
    println!("state dir      {}", paths::pie_home().display());
    println!(
        "config         {}{}",
        config_path.display(),
        if config_path.is_file() {
            ""
        } else {
            " (absent — defaults in use)"
        }
    );
    println!("session log    {}", paths::usage_log().display());
    println!("pie version    {VERSION}");

    if cfg.scrub_enabled() {
        println!("stripped env   {}", cfg.scrub_keys(true).join(", "));
    } else {
        // Loud on purpose: this is the one switch that can silently move a
        // subscription onto pay-as-you-go billing.
        println!();
        println!(
            "WARNING: PIE_STRIP_ENV is set, so API-key variables are NOT stripped \
             from child processes. A vendor CLI may switch to pay-as-you-go billing."
        );
    }
}

/// Persist the CLI versions for drift detection (`RISKS.md` §3).
///
/// Contains version strings only — never a credential.
fn write_version_snapshot(versions: &[(String, String)]) -> Result<PathBuf> {
    let path = paths::version_snapshot();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let backends: serde_json::Map<String, serde_json::Value> = versions
        .iter()
        .map(|(id, version)| (id.clone(), serde_json::Value::String(version.clone())))
        .collect();

    let snapshot = serde_json::json!({
        "generated_at": now_millis(),
        "pie_version": VERSION,
        "backends": backends,
    });

    let mut text = serde_json::to_string_pretty(&snapshot)?;
    text.push('\n');
    std::fs::write(&path, text)?;
    Ok(path)
}

/// The configured binary for a backend, for the "not found" message.
/// Whether a backend is expected to have a binary on this machine.
///
/// `provider::BACKEND_IDS` is exactly the subprocess backends — it was defined that way at
/// M0 and kept that meaning when M10 added two endpoint backends — so asking it is asking
/// the question the table actually has, rather than testing for a hardcoded id here.
fn is_subprocess_backend(id: &str) -> bool {
    provider::BACKEND_IDS.contains(&id)
}

fn configured_bin(cfg: &Config, id: &str) -> String {
    match id {
        "agy" => cfg.agy.bin.clone(),
        "codex" => cfg.codex.bin.clone(),
        "claude" => cfg.claude.bin.clone(),
        other => other.to_string(),
    }
}

/// Shorten a path for a table cell, keeping the identifying tail.
///
/// A full Windows path is far too long for a column, and the *tail* is what
/// identifies the binary. A path that already fits is returned unchanged,
/// whatever its native separators — on Linux and WSL those are forward slashes.
fn short_path(path: &str, width: usize) -> String {
    if path.chars().count() <= width {
        return path.to_string();
    }

    let parts: Vec<&str> = path
        .split(['\\', '/'])
        .filter(|part| !part.is_empty())
        .collect();

    let tail = match parts.len() {
        0 => path.to_string(),
        1 => parts[0].to_string(),
        2 => format!("{}\\{}", parts[0], parts[1]),
        n => format!("{}\\{}\\{}", parts[n - 3], parts[n - 2], parts[n - 1]),
    };

    if tail.chars().count() <= width {
        return tail;
    }

    let keep: String = tail
        .chars()
        .skip(tail.chars().count().saturating_sub(width.saturating_sub(1)))
        .collect();
    format!("…{keep}")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn a_fresh_snapshot_is_used_and_a_stale_or_misshapen_one_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().display().to_string();
        let _env = crate::testing::EnvGuard::set(&[("PIE_HOME", Some(home.as_str()))]);

        let path = paths::version_snapshot();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();

        let write = |generated_at: u64| {
            std::fs::write(
                &path,
                format!(
                    r#"{{"generated_at":{generated_at},"pie_version":"0.1.0","backends":{{"agy":"1.3.3"}}}}"#
                ),
            )
            .unwrap();
        };

        write(crate::session_log::now_millis());
        let (age, versions) = read_fresh_version_snapshot().expect("a fresh snapshot is used");
        assert!(age < 5, "age was {age}s");
        assert_eq!(versions.get("agy").map(String::as_str), Some("1.3.3"));

        // Stale: re-probing is the whole point of a TTL.
        write(crate::session_log::now_millis() - (VERSION_SNAPSHOT_TTL.as_secs() + 60) * 1_000);
        assert!(
            read_fresh_version_snapshot().is_none(),
            "a stale snapshot must not be served"
        );

        // `backends` as an array: the shape this reader originally (and wrongly)
        // assumed. A miss, never a panic.
        std::fs::write(&path, r#"{"generated_at":1,"backends":[]}"#).unwrap();
        assert!(read_fresh_version_snapshot().is_none());

        std::fs::remove_file(&path).unwrap();
        assert!(
            read_fresh_version_snapshot().is_none(),
            "missing file is a miss"
        );
    }

    #[test]
    fn short_paths_that_fit_are_left_alone() {
        assert_eq!(short_path("agy", BIN_WIDTH), "agy");
        assert_eq!(short_path("bin/agy.exe", BIN_WIDTH), "bin/agy.exe");
        assert_eq!(short_path(r"bin\agy.exe", BIN_WIDTH), r"bin\agy.exe");
    }

    #[test]
    fn long_paths_keep_the_identifying_tail() {
        let long = r"C:\Users\amiru\AppData\Local\agy\bin\agy.exe";
        let short = short_path(long, BIN_WIDTH);

        assert!(
            short.chars().count() <= BIN_WIDTH,
            "{short} is {} chars",
            short.chars().count()
        );
        assert!(short.contains("agy.exe"), "{short}");
        assert!(
            !short.contains("C:\\Users"),
            "prefix should be dropped: {short}"
        );
    }

    #[test]
    fn short_path_never_exceeds_the_column_width() {
        let long = format!("C:\\{}\\\\binary.exe", "x".repeat(200));
        assert!(short_path(&long, 20).chars().count() <= 20);

        // A single over-long component still has to be clipped.
        let one_component = "y".repeat(100);
        assert!(short_path(&one_component, 10).chars().count() <= 10);
    }

    #[test]
    fn unix_style_paths_are_shortened_too() {
        let long = "/home/amiru/.local/share/pie/bin/agy";
        let short = short_path(long, 20);
        assert!(short.chars().count() <= 20, "{short}");
        assert!(short.contains("agy"), "{short}");
    }

    #[test]
    fn snapshot_payload_has_no_room_for_a_secret() {
        let versions = [
            ("agy".to_string(), "1.3.3".to_string()),
            ("codex".to_string(), "codex-cli 0.162.0".to_string()),
        ];
        let backends: serde_json::Map<String, serde_json::Value> = versions
            .iter()
            .map(|(id, v)| (id.clone(), serde_json::Value::String(v.clone())))
            .collect();
        let text = serde_json::to_string(&backends).unwrap();

        assert!(text.contains("1.3.3"));
        for suspicious in ["token", "key", "secret", "auth", "bearer"] {
            assert!(
                !text.to_lowercase().contains(suspicious),
                "the version snapshot must not carry a `{suspicious}` field: {text}"
            );
        }
    }
}
