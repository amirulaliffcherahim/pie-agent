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
use crate::provider::http::{self, live_client, HttpSpec, HttpTransport};
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

    /// The model catalog endpoint (`GET`).
    pub fn models_url(&self) -> String {
        format!("{}/models", self.base_url)
    }

    /// The credit endpoint, where the vendor has one.
    ///
    /// Only OpenRouter: it is a prepaid balance rather than a quota window, which is the
    /// closest thing either API-key vendor has to "remaining". DeepSeek exposes nothing, and
    /// `None` here is what keeps `usage` honest for it instead of inventing a bucket.
    pub fn credit_url(&self) -> Option<String> {
        (self.id == OPENROUTER.id).then(|| format!("{}/key", self.base_url))
    }

    /// The headers a request to this vendor carries.
    ///
    /// One place, so a catalog request and a completion request cannot disagree about
    /// attribution — and so the credential is attached in exactly one function.
    pub fn headers(&self, key: &str) -> Vec<(String, String)> {
        let mut headers = vec![("Authorization".to_string(), format!("Bearer {key}"))];
        if self.id == OPENROUTER.id {
            // OpenRouter attributes traffic by header. Sent because its own docs ask for it,
            // which also keeps pie identifiable rather than anonymous.
            headers.push(("X-Title".to_string(), crate::NAME.to_string()));
        }
        headers
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
        let Some(key) = self.key() else {
            // Refused before any request, so a machine with no key never has one attached to
            // an outbound call. The same shape as `stream`, for the same reason.
            return Err(PieError::AuthRequired {
                backend: self.0.id.to_string(),
                hint: format!("{} is not set", self.0.key_env),
            });
        };

        let client = live_client(READ_TIMEOUT)?;
        let value = http::get_json(
            client.as_ref(),
            self.0.id,
            &self.0.models_url(),
            &self.0.headers(&key),
        )?;
        Ok(models_from_catalog(&value))
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
        // A prepaid balance, where the vendor exposes one. DeepSeek exposes nothing, and the
        // honest answer there is a report with no buckets and a note saying Why — never a
        // zero, which would read as "nothing left" rather than "not reported".
        let notes = vec![
            "cost is reported per turn in the session-log totals, from the endpoint's own \
             accounting; an API-key account has no quota window"
                .to_string(),
        ];

        let Some(url) = self.0.credit_url() else {
            return Ok(UsageReport {
                backend: self.0.id.to_string(),
                buckets: Vec::new(),
                source: "pay-as-you-go: this vendor exposes no balance endpoint".to_string(),
                notes,
            });
        };

        let Some(key) = self.key() else {
            return Ok(UsageReport {
                backend: self.0.id.to_string(),
                buckets: Vec::new(),
                source: format!("balance needs {}", self.0.key_env),
                notes,
            });
        };

        let client = live_client(READ_TIMEOUT)?;
        let value = http::get_json(client.as_ref(), self.0.id, &url, &self.0.headers(&key))?;
        let buckets = credit_bucket(&value).into_iter().collect();

        Ok(UsageReport {
            backend: self.0.id.to_string(),
            buckets,
            source: format!("{} /key", self.0.base_url),
            notes,
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

/// Parse an OpenAI-compatible `GET /models` body.
///
/// **Pure**, so the catalog mapping is tested without a network — the transport's own tests
/// cover the HTTP half, and a shape test that needed a live endpoint would fail the day a
/// vendor reordered its JSON, which teaches nothing about pie.
///
/// A row with no usable `id` is skipped rather than given a placeholder: a model nobody can
/// name is not selectable, and inventing an id would put a `--model` value in the picker that
/// the endpoint rejects.
pub fn models_from_catalog(value: &serde_json::Value) -> Vec<Model> {
    let rows = match value.get("data").and_then(serde_json::Value::as_array) {
        Some(rows) => rows,
        // Some OpenAI-compatible builds answer with a bare array.
        None => match value.as_array() {
            Some(rows) => rows,
            None => return Vec::new(),
        },
    };

    let mut models: Vec<Model> = rows
        .iter()
        .filter_map(|row| {
            let id = crate::jsonl::s_nonempty(row, "id")?;
            let display = crate::jsonl::s_nonempty(row, "name").unwrap_or_else(|| id.clone());
            // One `Model` per row: these endpoints publish ids, not effort axes, so
            // `efforts` and `default_effort` stay empty rather than being invented.
            Some(Model::plain(id, display))
        })
        .collect();

    // Sorted by id so two runs over the same catalog produce the same picker order — a list
    // that reordered between runs would make the picker's highlight land somewhere different
    // for no reason the user could see.
    models.sort_by(|a, b| a.id.cmp(&b.id));
    models.dedup_by(|a, b| a.id == b.id);
    models
}

/// Parse OpenRouter's `GET /key` body into one bucket.
///
/// The fields are dollars as JSON numbers (or `null` for an unlimited key), converted to
/// micro-USD integers so the figure matches what `usage.jsonl` records rather than drifting
/// apart by a rounding step. `None` when the body carries no limit at all — an unlimited key
/// has no "remaining" to draw, and a bar for it would be a fiction.
pub fn credit_bucket(value: &serde_json::Value) -> Option<crate::provider::Bucket> {
    let data = value.get("data").unwrap_or(value);
    let limit = data.get("limit")?.as_f64()?;
    if limit <= 0.0 {
        return None;
    }
    let used = data
        .get("usage")
        .and_then(serde_json::Value::as_f64)
        .unwrap_or(0.0);

    let micros = |usd: f64| -> u64 { (usd.max(0.0) * 1_000_000.0).round() as u64 };
    Some(crate::provider::Bucket {
        label: "credit".to_string(),
        remaining_fraction: Some(((limit - used) / limit).clamp(0.0, 1.0)),
        // A prepaid balance has no reset time: it is topped up, not renewed.
        reset_at: None,
        used: Some(micros(used)),
        limit: Some(micros(limit)),
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use serde_json::json;

    /// The catalog maps ids and keeps them stable between runs.
    #[test]
    fn the_catalog_maps_ids_and_orders_them_deterministically() {
        let body = json!({"data": [
            {"id": "z-model", "name": "Z"},
            {"id": "a-model", "name": "A"},
            {"id": "a-model", "name": "A again"},
        ]});
        let models = models_from_catalog(&body);
        assert_eq!(
            models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["a-model", "z-model"],
            "sorted by id, and a duplicate id collapses rather than appearing twice"
        );
        // A bare array is accepted too, because some OpenAI-compatible builds answer that way.
        let bare = json!([{"id": "only"}]);
        assert_eq!(models_from_catalog(&bare).len(), 1);
    }

    /// A row nobody can name is skipped, not given a placeholder.
    ///
    /// A fabricated id would put a `--model` value in the picker that the endpoint rejects,
    /// which is worse than a shorter list.
    #[test]
    fn a_catalog_row_with_no_usable_id_is_skipped() {
        let body = json!({"data": [
            {"id": ""},
            {"name": "no id at all"},
            {"id": "real"},
        ]});
        let models = models_from_catalog(&body);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "real");
        assert_eq!(
            models[0].display, "real",
            "with no name the id is its own display, not an empty cell"
        );
    }

    /// A body that is neither shape yields nothing rather than panicking.
    #[test]
    fn a_catalog_of_an_unexpected_shape_is_empty_not_a_panic() {
        for body in [
            json!({}),
            json!("a string"),
            json!({"data": 7}),
            json!(null),
        ] {
            assert!(models_from_catalog(&body).is_empty(), "{body}");
        }
    }

    /// The credit bucket is real micros, and a fraction that cannot go out of range.
    #[test]
    fn the_credit_bucket_converts_dollars_to_micros() {
        let bucket = credit_bucket(&json!({"data": {"limit": 10.0, "usage": 2.5}}))
            .expect("a limited key has a balance");
        assert_eq!(bucket.label, "credit");
        assert_eq!(bucket.limit, Some(10_000_000));
        assert_eq!(bucket.used, Some(2_500_000));
        assert_eq!(bucket.remaining_fraction, Some(0.75));
        assert!(
            bucket.reset_at.is_none(),
            "a prepaid balance does not renew"
        );

        // Over-spend is clamped rather than rendering a negative bar.
        let over = credit_bucket(&json!({"data": {"limit": 1.0, "usage": 5.0}})).unwrap();
        assert_eq!(over.remaining_fraction, Some(0.0));
    }

    /// An unlimited key, and a body with no limit at all, both yield **no** bucket.
    ///
    /// `None` rather than a zero: a zero would render as "nothing left", which is the
    /// opposite of an unlimited key.
    #[test]
    fn an_unlimited_or_absent_limit_is_no_bucket_rather_than_zero() {
        for body in [
            json!({"data": {"limit": null, "usage": 1.0}}),
            json!({"data": {"usage": 1.0}}),
            json!({"data": {"limit": 0}}),
            json!({}),
        ] {
            assert!(credit_bucket(&body).is_none(), "{body}");
        }
    }

    /// The two vendors differ only in the constants the design document names.
    #[test]
    fn the_vendors_differ_only_where_the_design_document_says_they_do() {
        assert_eq!(OPENROUTER.id, "openrouter");
        assert_eq!(OPENROUTER.key_env, "OPENROUTER_API_KEY");
        assert_eq!(DEEPSEEK.id, "deepseek");
        assert_eq!(DEEPSEEK.key_env, "DEEPSEEK_API_KEY");
        assert_ne!(OPENROUTER.base_url, DEEPSEEK.base_url);

        // The credit endpoint exists for one of them, and its absence is what keeps
        // DeepSeek's `usage` honest instead of inventing a bucket.
        assert!(OPENROUTER.credit_url().is_some());
        assert!(DEEPSEEK.credit_url().is_none());

        // Both endpoints are derived from the base URL rather than written out twice.
        for vendor in [OPENROUTER, DEEPSEEK] {
            assert!(vendor.models_url().ends_with("/models"));
            assert!(vendor.completions_url().ends_with("/chat/completions"));
        }
    }

    /// Attribution is the only header difference, and both carry the credential.
    #[test]
    fn the_headers_carry_the_key_and_only_openrouter_gets_attribution() {
        let openrouter = OPENROUTER.headers("k");
        assert_eq!(openrouter[0].1, "Bearer k");
        assert!(openrouter.iter().any(|(name, _)| name == "X-Title"));

        let deepseek = DEEPSEEK.headers("k");
        assert_eq!(
            deepseek.len(),
            1,
            "no attribution header DeepSeek never asked for"
        );
        assert_eq!(deepseek[0].1, "Bearer k");
    }

    /// Asking for usage accounting is vendor vocabulary, not shared.
    ///
    /// It is the whole reason `cost_usd_micros` can be real, and a shared module guessing the
    /// spelling would `400` on the vendor that spells it differently.
    #[test]
    fn each_vendor_asks_for_usage_in_its_own_vocabulary() {
        assert_eq!(DEEPSEEK.usage_request().0, "stream_options");
        assert_eq!(OPENROUTER.usage_request().0, "usage");
        assert_ne!(DEEPSEEK.usage_request().1, OPENROUTER.usage_request().1);
    }
}
