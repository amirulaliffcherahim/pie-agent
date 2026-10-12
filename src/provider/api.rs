//! The API-key backends: OpenRouter and DeepSeek (`planning/DEFERRED-BACKENDS.md`).
//!
//! These are the first backends that are **not subprocesses** and the first that
//! legitimately read an API key, so both facts cut against invariants the other nine
//! milestones hold. `DEFERRED-BACKENDS.md`'s "Non-negotiables" section is the contract:
//!
//! * The key is read from the environment **at call time**, never from `config.toml`,
//!   never written under `~/.pie`, and never logged — it goes straight from `std::env`
//!   into one request header and is never stored in a struct that could print it.
//! * `cost_usd_micros` is real here, which is the whole reason M10 was constrained to
//!   "only backends where `cost_usd_micros` is real".
//! * The tool loop is M9's, not a second copy (`RISKS.md` §8).
//!
//! ## One core, two vendors — a deliberate departure
//!
//! `DEFERRED-BACKENDS.md` sketches `openrouter.rs` and/or `deepseek.rs`. This is **one**
//! module holding one parameterised implementation, because that document also says
//! DeepSeek is "identical apart from the two constants" — and two near-duplicate files
//! describing one wire protocol are two places for the wire to drift. The vendor-specific
//! parts are data ([`Vendor`]), and the deviation is recorded here rather than taken
//! silently.
//!
//! ## No tools ship with pie, and that is not an oversight
//!
//! [`NoTools`] answers every call with an error result. M9 froze that scope decision: the
//! loop is the loop, and a concrete tool suite — what a `bash` tool may do unattended — is
//! a separate concern with its own questions. Answering with an *error* rather than not
//! answering is what keeps the loop from leaving a model waiting for a result that will
//! never come.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use crate::error::{PieError, Result};
use crate::provider::http::{live_client, HttpSpec, HttpTransport};
use crate::provider::{
    AuthState, Capabilities, ChatRequest, Model, Provider, StreamEvent, TurnOutcome, UsageReport,
};
use crate::tool_loop::{run_loop, ToolCall, ToolResult, ToolRunner};

/// How long one read may stall before the turn fails rather than hangs.
///
/// A **read** timeout, not a total one: a legitimate answer may stream for minutes, so a
/// wall-clock cap would cut off exactly the long turns this backend exists to serve, while
/// a stalled socket still has to fail (`RISKS.md` §6).
const READ_TIMEOUT: Duration = Duration::from_secs(120);

/// How many tool rounds one turn may take before the loop stops and says so.
///
/// A model that asks for tools forever must be bounded, and the cap's effect is reported
/// rather than swallowed (`DESIGN.md` §18).
const MAX_TOOL_ROUNDS: usize = 8;

/// Everything that differs between two OpenAI-compatible vendors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Vendor {
    /// Backend id, also the `--model` prefix.
    pub id: &'static str,
    /// Environment variable holding the key.
    pub key_env: &'static str,
    /// Base URL of the API, without a trailing slash.
    pub base_url: &'static str,
    /// A model used when the caller names none.
    pub default_model: &'static str,
}

impl Vendor {
    /// The chat-completions endpoint.
    pub fn completions_url(&self) -> String {
        format!("{}/chat/completions", self.base_url)
    }

    /// The body field that asks the endpoint for usage accounting.
    ///
    /// Not hardcoded in the shared module: a field a shared layer guesses at is a field
    /// that `400`s on the vendor spelling it differently, and the accounting it buys is the
    /// entire reason `cost_usd_micros` can be real.
    pub fn usage_request(&self) -> (&'static str, serde_json::Value) {
        match self.id {
            "deepseek" => ("stream_options", serde_json::json!({"include_usage": true})),
            // OpenRouter wants it under `usage`.
            _ => ("usage", serde_json::json!({"include": true})),
        }
    }
}

/// OpenRouter: the plainest OpenAI-compatible endpoint of the two.
pub const OPENROUTER: Vendor = Vendor {
    id: "openrouter",
    key_env: "OPENROUTER_API_KEY",
    base_url: "https://openrouter.ai/api/v1",
    default_model: "deepseek/deepseek-chat",
};

/// DeepSeek's own API.
pub const DEEPSEEK: Vendor = Vendor {
    id: "deepseek",
    key_env: "DEEPSEEK_API_KEY",
    base_url: "https://api.deepseek.com/v1",
    default_model: "deepseek-chat",
};

/// An API-key backend over one OpenAI-compatible endpoint.
#[derive(Debug, Clone, Copy)]
pub struct ApiBackend(pub Vendor);

/// Answers every tool call with a stated error.
///
/// pie ships no tools (M9's scope decision), and the important half is that it **answers**:
/// a loop that silently ignored a call would leave the model waiting for a result that
/// never comes, which is a conversation that never finishes rather than a missing feature.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoTools;

impl ToolRunner for NoTools {
    fn run(&self, call: &ToolCall) -> ToolResult {
        ToolResult {
            id: call.id.clone(),
            output: format!(
                "pie has no tools configured, so `{}` was not run. M9 builds the loop; the \
                 tool suite (read/bash/edit/write/grep/find/ls) is a separate milestone.",
                call.name
            ),
            is_error: true,
        }
    }
}

#[async_trait]
impl Provider for ApiBackend {
    fn id(&self) -> &'static str {
        self.0.id
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            // True, and it is the one place that is legitimate: this wire carries
            // `tool_calls`, and M9's loop answers them (`DEFERRED-BACKENDS.md`).
            tool_loop: true,
            // Stateless: each request carries the whole conversation, so there is no vendor
            // conversation id to resume.
            resume: false,
            stdin_prompt: false,
            max_prompt_argv: 0,
            // The OpenAI-compatible shape has a real `system` role, so the persona goes in
            // whole and unmodified (`DESIGN.md` §10 — the bytes never change, only the
            // mechanism).
            system_prompt: true,
        }
    }

    fn binary(&self, _cfg: &crate::config::Config) -> Option<std::path::PathBuf> {
        // Not a subprocess backend, so there is no binary to resolve and `--list-backends`
        // must not report one as missing.
        None
    }

    async fn auth_status(&self, _cfg: &crate::config::Config) -> AuthState {
        match self.key() {
            Some(_) => AuthState::Ready,
            None => AuthState::NeedsLogin {
                hint: format!("set {}", self.0.key_env),
            },
        }
    }

    async fn list_models(&self, _cfg: &crate::config::Config) -> Result<Vec<Model>> {
        // The catalog needs a GET, and `HttpClient` is POST-only by construction (its one
        // caller needs to stream a completion). Reporting that rather than inventing a
        // static list: a hardcoded catalog for a vendor that ships models weekly would be
        // wrong within a month and would look authoritative while being wrong.
        Err(PieError::Unsupported {
            backend: self.0.id.to_string(),
            feature: "model catalog: the shared HTTP client exposes POST only, so `GET /models` \
                      is a follow-up (M10 note)"
                .to_string(),
        })
    }

    async fn stream(
        &self,
        _cfg: &crate::config::Config,
        req: &ChatRequest,
        sink: &mut (dyn FnMut(StreamEvent) + Send),
    ) -> Result<TurnOutcome> {
        // Refused **before** any credential is read, so a machine with no key never has one
        // touched. The message names the variable rather than saying "no key", because the
        // only actionable form of that sentence names what to set.
        let Some(key) = self.key() else {
            return Err(PieError::AuthRequired {
                backend: self.0.id.to_string(),
                hint: format!(
                    "{} is not set. These backends read an API key from the environment, and \
                     pie never falls back to another backend: answering with a different \
                     subscription than the one asked for is the failure `resolve_target` \
                     exists to prevent",
                    self.0.key_env
                ),
            });
        };
        debug_assert!(!key.is_empty());

        let model = req
            .model
            .clone()
            .unwrap_or_else(|| self.0.default_model.to_string());
        let (extra_key, extra_value) = self.0.usage_request();

        let mut spec = HttpSpec::new(
            self.0.id,
            self.0.completions_url(),
            model,
            self.0.key_env,
            req.prompt.clone(),
        );
        spec.system = req.persona.clone();
        spec.body_extras.push((extra_key.to_string(), extra_value));
        if self.0.id == OPENROUTER.id {
            // OpenRouter attributes traffic by header. Sent because its own docs ask for it,
            // which also keeps pie identifiable rather than anonymous.
            spec.headers
                .push(("X-Title".to_string(), crate::NAME.to_string()));
        }

        let client = live_client(READ_TIMEOUT)?;
        let mut transport = HttpTransport::new(Arc::clone(&client), spec);
        // The first request is issued here: each HTTP request carries the whole
        // conversation, so the loop's "send a result" and the wire's "start the turn" are
        // the same operation and the transport owns the distinction.
        transport.start()?;

        let outcome = run_loop(&mut transport, &NoTools, MAX_TOOL_ROUNDS, sink)?;

        Ok(TurnOutcome {
            text: outcome.text,
            usage: outcome.usage,
            status: if outcome.capped {
                // Reported, not swallowed: a capped turn produced a partial answer, and a
                // caller that cannot tell has been told a lie (`DESIGN.md` §18).
                "tool-loop-capped".to_string()
            } else {
                "stop".to_string()
            },
            conversation_id: None,
            duration: outcome.duration,
        })
    }

    async fn usage(&self, _cfg: &crate::config::Config) -> Result<UsageReport> {
        // Neither vendor exposes a quota window: these are pay-as-you-go accounts, so there
        // is no "remaining" to report and the honest answer is none rather than a zero.
        // `pie usage` still prints the session-log totals, which for these backends now
        // carry a real `cost_usd_micros`.
        Ok(UsageReport {
            backend: self.0.id.to_string(),
            buckets: Vec::new(),
            source: "pay-as-you-go: no quota window to report".to_string(),
            notes: vec![format!(
                "cost is reported per turn in `pie usage`'s totals, from the endpoint's own \
                 accounting; there is no quota bucket for an API-key account"
            )],
        })
    }
}

impl ApiBackend {
    /// The key, read **now** and never stored.
    ///
    /// `None` for unset, empty, or whitespace-only, because a variable set to whitespace is
    /// a configuration mistake rather than a credential — and treating it as one would send
    /// an empty bearer token and turn a clear config error into an opaque 401.
    fn key(&self) -> Option<String> {
        std::env::var(self.0.key_env)
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    }
}
