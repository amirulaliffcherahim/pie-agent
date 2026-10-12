//! `pie --list-backends` and `pie doctor`.
//!
//! Both print the same backend table: id, resolved binary, version, auth state.
//! `doctor` additionally reports local state and writes the version snapshot
//! used for CLI-drift detection (`RISKS.md` §3).
//!
//! Neither command calls a model, so neither spends subscription quota (I7).

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

/// `pie --list-backends`
pub async fn list(cfg: &Config) -> Result<()> {
    let _ = table(cfg).await?;
    Ok(())
}

/// `pie doctor`
pub async fn doctor(cfg: &Config) -> Result<()> {
    let versions = table(cfg).await?;
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
async fn table(cfg: &Config) -> Result<Vec<(String, String)>> {
    println!(
        "{:<ID_WIDTH$} {:<BIN_WIDTH$} {:<VERSION_WIDTH$} {:<AUTH_WIDTH$} DETAIL",
        "BACKEND", "BINARY", "VERSION", "AUTH"
    );

    let mut versions: Vec<(String, String)> = Vec::new();

    for p in provider::registry() {
        let id = p.id();

        let binary = match p.binary(cfg) {
            Some(path) => short_path(&path.display().to_string(), BIN_WIDTH),
            None => format!("(not found: {})", configured_bin(cfg, id)),
        };

        let version = provider::driver::probe_version(&*p, cfg, VERSION_PROBE_TIMEOUT)
            .await
            // Redacted before it is printed or written into the version snapshot:
            // a CLI whose `--version` echoed a credential would otherwise be
            // persisted into ~/.pie/cache/versions.json (I10).
            .map(|raw| process::redact(&raw))
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

    Ok(versions)
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
        .skip(tail.chars().count().saturating_sub(width - 1))
        .collect();
    format!("…{keep}")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

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
