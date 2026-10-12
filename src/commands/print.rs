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
use crate::profile::Profile;
use crate::provider::{self, ChatRequest, StreamEvent};
use crate::session_log::{self, SessionLogRow};

/// `pie -p <PROMPT> [--model provider/model]`
pub async fn run(cfg: &Config, cli: &Cli, prompt: &str) -> Result<()> {
    // The projection is loaded once, before anything is spawned, and handed to
    // whichever backend runs. Every backend receives the identical bytes (M3's
    // Done condition, DESIGN §10); only the mechanism differs. A malformed
    // `profile.toml` therefore fails before a turn is paid for, not after.
    let profile = Profile::from_home()?;

    let (backend, model) = provider::resolve_target(cfg, cli.model.as_deref())?;

    let provider = provider::by_id(&backend).ok_or_else(|| PieError::UnknownBackend {
        backend: backend.clone(),
        known: super::models::known_backends(),
    })?;

    let request = build_request(cli, prompt, model.as_deref(), &profile);
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

            // Surfaced so the id can be passed back via `--resume`: without it a
            // conversation is unreachable, which is what makes resume usable at
            // all. stderr, so stdout stays a clean answer channel.
            if let Some(id) = &turn.conversation_id {
                eprintln!("[resume] {id}");
            }

            session_log::append_best_effort(&SessionLogRow::from_turn(
                &backend,
                // `general.default_backend` can select a backend without a
                // model, and then the vendor CLI picks its own — which pie
                // cannot name. The frozen row schema has no null, so the field
                // is empty rather than an invented id.
                model.as_deref().unwrap_or_default(),
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
                model.as_deref().unwrap_or_default(),
                error.kind(),
                elapsed_ms,
            ));
            Err(error)
        }
    }
}

/// Assemble the request from the command line.
fn build_request(cli: &Cli, prompt: &str, model: Option<&str>, profile: &Profile) -> ChatRequest {
    let mut request = ChatRequest::new(prompt);
    request.model = model.map(str::to_string);
    request.persona = profile.persona().map(str::to_string);
    request.effort = cli.effort.clone();
    request.resume = cli.resume.clone();
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
