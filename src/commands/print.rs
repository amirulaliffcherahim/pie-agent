//! `pie -p/--print` — one delegate turn.
//!
//! Delegate mode means the whole task goes to the vendor CLI, which runs its
//! own tools and returns a final answer. pie injects no tools of its own
//! (`RISKS.md` §1), so there are no pie tool cards here — only the CLI's own
//! progress steps, which we surface on stderr.
//!
//! Every turn is recorded in the session log, success or failure, so `pie
//! usage` and the future "know me" job (M7) have real data to work from.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::cli::Cli;
use crate::config::Config;
use crate::error::{PieError, Result};
use crate::provider::{self, ChatRequest, StreamEvent};
use crate::session_log::{self, SessionLogRow};

/// `pie -p <PROMPT> [--model provider/model]`
pub async fn run(cfg: &Config, cli: &Cli, prompt: &str) -> Result<()> {
    let (backend, model) = provider::resolve_target(cfg, cli.model.as_deref())?;

    let provider = provider::by_id(&backend).ok_or_else(|| PieError::UnknownBackend {
        backend: backend.clone(),
        known: super::models::known_backends(),
    })?;

    let request = build_request(cli, prompt, &model);
    let started = Instant::now();
    let streamed = Arc::new(AtomicBool::new(false));

    let outcome = {
        let mut sink = make_sink(cli.json, Arc::clone(&streamed));
        provider.stream(cfg, &request, &mut sink).await
    };

    let elapsed_ms = started.elapsed().as_millis() as u64;

    match outcome {
        Ok(turn) => {
            if !cli.json {
                if streamed.load(Ordering::Relaxed) {
                    // Deltas already reached stdout; just close the line.
                    println!();
                } else {
                    println!("{}", turn.text);
                }
            }

            session_log::append_best_effort(&SessionLogRow::from_turn(
                &backend,
                &model,
                &turn.usage,
                elapsed_ms,
                &turn.status,
            ));
            Ok(())
        }
        Err(error) => {
            // Record the attempt even when it failed: a timeout still costs
            // wall clock and is worth seeing in the totals.
            session_log::append_best_effort(&SessionLogRow::from_error(
                &backend,
                &model,
                error.kind(),
                elapsed_ms,
            ));
            Err(error)
        }
    }
}

/// Assemble the request from the command line.
fn build_request(cli: &Cli, prompt: &str, model: &str) -> ChatRequest {
    let mut request = ChatRequest::new(prompt);
    request.model = Some(model.to_string());
    request.effort = cli.effort.clone();
    request.cwd = cli
        .cwd
        .clone()
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));
    request.timeout = cli.timeout.map(Duration::from_secs);
    request
}

/// Where stream events go.
///
/// Human mode prints answer text to **stdout** so it can be piped, and tool
/// progress to **stderr** so it never corrupts that pipe. `--json` emits one
/// event per line on stdout instead.
fn make_sink(json: bool, streamed: Arc<AtomicBool>) -> impl FnMut(StreamEvent) + Send {
    move |event: StreamEvent| {
        if json {
            if let Ok(line) = serde_json::to_string(&event) {
                println!("{line}");
            }
            return;
        }

        match event {
            StreamEvent::TextDelta(text) => {
                use std::io::Write;
                streamed.store(true, Ordering::Relaxed);
                print!("{text}");
                let _ = std::io::stdout().flush();
            }
            StreamEvent::ToolStep { name, state, .. } => {
                eprintln!("[tool] {name} {state:?}");
            }
            // Init and Usage are folded into the outcome; Done would duplicate
            // the text we have already streamed.
            _ => {}
        }
    }
}
