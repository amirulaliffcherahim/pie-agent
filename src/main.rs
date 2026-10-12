//! Entry point: parse, dispatch, map errors to exit codes.
//!
//! No business logic lives here — that is the whole point of
//! `planning/DESIGN.md` §2. This file wires the library together and prints.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::{CommandFactory, Parser};

use pie::cli::{Cli, Command};
use pie::config::{Config, Overrides};
use pie::error::{truncate, PieError, Result};
use pie::provider::{self, ChatRequest, Provider, StreamEvent};
use pie::sessionlog::{self, SessionLogRow};
use pie::{paths, usage, VERSION};

/// Version probe deadline for the `--list-backends` and `doctor` tables.
const VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(3);

#[tokio::main]
async fn main() {
    let code = run().await;
    std::process::exit(code);
}

async fn run() -> i32 {
    pie::logging::init();
    let cli = Cli::parse();

    let overrides = Overrides {
        default_model: cli.model.clone(),
        timeout_secs: cli.timeout,
    };

    let cfg = match Config::resolve(&overrides) {
        Ok(c) => c,
        Err(e) => return fail(&e),
    };

    match dispatch(cli, &cfg).await {
        Ok(()) => 0,
        Err(e) => fail(&e),
    }
}

/// Print the error (redacted) and return its exit code.
fn fail(e: &PieError) -> i32 {
    eprintln!("pie: {}", pie::proc::redact(&e.to_string()));
    e.exit_code()
}

async fn dispatch(cli: Cli, cfg: &Config) -> Result<()> {
    if cli.list_backends {
        return cmd_backends(cfg, false).await;
    }

    match cli.command {
        Some(Command::Auth) => cmd_auth(cfg).await,
        Some(Command::Models { backend, refresh }) => cmd_models(cfg, backend, refresh).await,
        Some(Command::Usage { backend }) => cmd_usage(cfg, backend).await,
        Some(Command::Doctor) => cmd_backends(cfg, true).await,
        None => match cli.print.clone() {
            Some(prompt) => cmd_print(cfg, &cli, prompt).await,
            None => {
                Cli::command()
                    .print_help()
                    .map_err(|e| PieError::Config(format!("could not print help: {e}")))?;
                println!();
                Ok(())
            }
        },
    }
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
/// A full Windows path is far too long for a column, and the *tail* is the part
/// that identifies the binary, so we keep the last two components.
fn short_path(path: &str, width: usize) -> String {
    if path.chars().count() <= width {
        return path.to_string();
    }

    let parts: Vec<&str> = path.split(['\\', '/']).filter(|s| !s.is_empty()).collect();

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

/// `--list-backends` and `doctor`.
async fn cmd_backends(cfg: &Config, doctor: bool) -> Result<()> {
    println!(
        "{:<9} {:<34} {:<13} {:<15} DETAIL",
        "BACKEND", "BINARY", "VERSION", "AUTH"
    );

    let mut versions: Vec<(String, String)> = Vec::new();

    for p in provider::registry() {
        let id = p.id();
        let bin = p
            .binary(cfg)
            .map(|b| short_path(&b.display().to_string(), 34))
            .unwrap_or_else(|| format!("(not found: {})", configured_bin(cfg, id)));

        let version = provider::cli::probe_version(&*p, cfg, VERSION_PROBE_TIMEOUT)
            .await
            .unwrap_or_else(|| "unknown".to_string());

        let auth = p.auth_status(cfg).await;
        println!(
            "{:<9} {:<34} {:<13} {:<15} {}",
            id,
            bin,
            truncate(&version, 13),
            auth.label(),
            auth.detail().unwrap_or_default()
        );

        versions.push((id.to_string(), version));
    }

    if !doctor {
        return Ok(());
    }

    println!();
    println!("state dir      {}", paths::pie_home().display());
    println!(
        "config         {}{}",
        paths::config_file().display(),
        if paths::config_file().is_file() {
            ""
        } else {
            " (absent — defaults in use)"
        }
    );
    println!("session log    {}", paths::usage_log().display());
    println!("pie version    {VERSION}");

    if !cfg.scrub_enabled() {
        println!();
        println!(
            "WARNING: PIE_STRIP_ENV is set — API-key env vars are NOT being scrubbed from child \
             processes. A vendor CLI may switch to pay-as-you-go billing."
        );
    }

    let scrubbed = cfg.scrub_keys(true);
    println!("stripped env   {}", scrubbed.join(", "));

    match write_version_snapshot(&versions) {
        Ok(path) => println!("snapshot       {}", path.display()),
        Err(e) => println!("snapshot       failed: {e}"),
    }

    Ok(())
}

/// Persist the CLI versions for drift detection (`RISKS.md` §3).
fn write_version_snapshot(versions: &[(String, String)]) -> Result<PathBuf> {
    let path = paths::version_snapshot();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let backends: serde_json::Map<String, serde_json::Value> = versions
        .iter()
        .map(|(id, v)| (id.clone(), serde_json::Value::String(v.clone())))
        .collect();

    let snapshot = serde_json::json!({
        "generated_at": sessionlog::now_millis(),
        "pie_version": VERSION,
        "backends": backends,
    });

    let mut text = serde_json::to_string_pretty(&snapshot)?;
    text.push('\n');
    std::fs::write(&path, text)?;
    Ok(path)
}

/// `pie auth` — no subprocesses, no quota.
async fn cmd_auth(cfg: &Config) -> Result<()> {
    println!("{:<9} {:<15} DETAIL", "BACKEND", "AUTH");
    for p in provider::registry() {
        let auth = p.auth_status(cfg).await;
        println!(
            "{:<9} {:<15} {}",
            p.id(),
            auth.label(),
            auth.detail().unwrap_or_default()
        );
    }
    println!();
    println!("pie never stores tokens; auth comes from each CLI's own login.");
    Ok(())
}

/// `pie models`
async fn cmd_models(cfg: &Config, backend: Option<String>, refresh: bool) -> Result<()> {
    let targets: Vec<Box<dyn Provider>> = match &backend {
        Some(id) => {
            let p = provider::by_id(id).ok_or_else(|| PieError::UnknownBackend {
                backend: id.clone(),
                known: provider::BACKEND_IDS
                    .iter()
                    .map(|s| (*s).to_string())
                    .collect(),
            })?;
            vec![p]
        }
        None => provider::registry(),
    };

    if refresh {
        println!("note: --refresh is wired to the on-disk cache in M1 (agy) and M2 (codex).");
    }

    for p in targets {
        let id = p.id();
        match p.list_models(cfg).await {
            Ok(models) if models.is_empty() => println!("{id}: no models reported"),
            Ok(models) => {
                println!("{id}:");
                for m in models {
                    let effort = if m.efforts.is_empty() {
                        String::new()
                    } else {
                        format!("  efforts: {}", m.efforts.join("|"))
                    };
                    println!("  {:<28} {}{}", m.id, m.display, effort);
                }
            }
            Err(PieError::Unsupported { feature, .. }) => {
                println!("{id}: not implemented yet — {feature}");
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// `pie usage` — quota buckets plus pie's own session-log totals.
async fn cmd_usage(cfg: &Config, backend: Option<String>) -> Result<()> {
    if let Some(id) = &backend {
        let p = provider::by_id(id).ok_or_else(|| PieError::UnknownBackend {
            backend: id.clone(),
            known: provider::BACKEND_IDS
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
        })?;
        match p.usage(cfg).await {
            Ok(report) => println!("{}", usage::render_report(&report)),
            Err(PieError::Unsupported { feature, .. }) => {
                println!("{}: quota probe not implemented yet — {feature}", p.id());
            }
            Err(e) => return Err(e),
        }
        println!();
    }

    let rows = sessionlog::read_all(&paths::usage_log())?;
    let rows = usage::rows_for_backend(&rows, backend.as_deref());

    let now = sessionlog::now_millis();
    const DAY_MS: u64 = 24 * 60 * 60 * 1_000;

    println!(
        "{}",
        usage::render_totals("all time", &usage::totals(&rows))
    );
    println!();
    println!(
        "{}",
        usage::render_totals(
            "last 7 days",
            &usage::totals(&usage::rows_since(&rows, now.saturating_sub(7 * DAY_MS)))
        )
    );
    println!();
    println!(
        "{}",
        usage::render_totals(
            "last 30 days",
            &usage::totals(&usage::rows_since(&rows, now.saturating_sub(30 * DAY_MS)))
        )
    );

    if backend.is_none() {
        println!();
        println!("pass a backend (`pie usage agy`) for its quota buckets.");
    }

    Ok(())
}

/// `pie -p <PROMPT>` — one delegate turn.
async fn cmd_print(cfg: &Config, cli: &Cli, prompt: String) -> Result<()> {
    let (backend, model) = provider::resolve_target(cfg, cli.model.as_deref())?;
    let p = provider::by_id(&backend).ok_or_else(|| PieError::UnknownBackend {
        backend: backend.clone(),
        known: provider::BACKEND_IDS
            .iter()
            .map(|s| (*s).to_string())
            .collect(),
    })?;

    let mut req = ChatRequest::new(prompt);
    req.model = Some(model.clone());
    req.effort = cli.effort.clone();
    req.cwd = cli
        .cwd
        .clone()
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));
    req.timeout = cli.timeout.map(Duration::from_secs);

    let json = cli.json;
    let started = Instant::now();
    let streamed = Arc::new(AtomicBool::new(false));

    let result = {
        let streamed = Arc::clone(&streamed);
        let mut sink = move |ev: StreamEvent| {
            if json {
                if let Ok(line) = serde_json::to_string(&ev) {
                    println!("{line}");
                }
                return;
            }
            match ev {
                StreamEvent::TextDelta(text) => {
                    use std::io::Write;
                    streamed.store(true, Ordering::Relaxed);
                    print!("{text}");
                    let _ = std::io::stdout().flush();
                }
                StreamEvent::ToolStep { name, state, .. } => {
                    eprintln!("[tool] {name} {state:?}");
                }
                // Init/Usage/Done are surfaced by the outcome, not the stream.
                _ => {}
            }
        };
        p.stream(cfg, &req, &mut sink).await
    };

    let elapsed_ms = started.elapsed().as_millis() as u64;

    match result {
        Ok(outcome) => {
            if !json {
                if streamed.load(Ordering::Relaxed) {
                    // Deltas already went to stdout; just end the line.
                    println!();
                } else {
                    println!("{}", outcome.text);
                }
            }
            sessionlog::append_best_effort(&SessionLogRow::from_turn(
                &backend,
                &model,
                &outcome.usage,
                elapsed_ms,
                &outcome.status,
            ));
            Ok(())
        }
        Err(e) => {
            sessionlog::append_best_effort(&SessionLogRow::from_error(
                &backend,
                &model,
                e.kind(),
                elapsed_ms,
            ));
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn short_paths_that_fit_are_left_alone() {
        // A path that already fits is returned unchanged — including its native
        // separators, since on Linux and WSL those are forward slashes.
        assert_eq!(short_path("agy", 34), "agy");
        assert_eq!(short_path("bin/agy.exe", 34), "bin/agy.exe");
        assert_eq!(short_path(r"bin\agy.exe", 34), r"bin\agy.exe");
    }

    #[test]
    fn long_paths_keep_the_identifying_tail() {
        let long = r"C:\Users\amiru\AppData\Local\agy\bin\agy.exe";
        let short = short_path(long, 34);
        assert!(
            short.chars().count() <= 34,
            "{short} is {} chars",
            short.chars().count()
        );
        assert!(short.contains("agy.exe"), "{short}");
        assert!(!short.contains("C:\\Users"), "prefix dropped: {short}");
    }

    #[test]
    fn short_path_never_exceeds_the_column_width() {
        let long = format!("C:\\{}\\binary.exe", "x".repeat(200));
        assert!(short_path(&long, 20).chars().count() <= 20);
        // A single over-long component still has to be clipped.
        let one = "y".repeat(100);
        assert!(short_path(&one, 10).chars().count() <= 10);
    }

    #[test]
    fn unix_style_paths_are_shortened_too() {
        let long = "/home/amiru/.local/share/pie/bin/agy";
        let short = short_path(long, 20);
        assert!(short.chars().count() <= 20, "{short}");
        assert!(short.contains("agy"), "{short}");
    }
}
