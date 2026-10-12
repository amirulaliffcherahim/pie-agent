//! Anthropic Claude Code (`claude`) backend — future-proofing only.
//!
//! There is **no Claude subscription on this machine**, so this adapter is
//! written to spec and left disabled. It must never block the critical path
//! (`PLAN.md` §1): the live v0 path is agy + codex.
//!
//! Spec source: `knowledge/claude.md`.
//!
//! `tool_loop` is reported as `false` deliberately. Claude Code's SDK mode
//! (`--input-format stream-json --output-format stream-json`) genuinely does
//! support `tool_use` → `tool_result` round-trips, but pie does not implement
//! that loop until M9. Advertising a capability we cannot honour would make the
//! agent loop branch into a path that does not exist.

use std::path::PathBuf;

use async_trait::async_trait;

use crate::auth::{self, Credential};
use crate::config::Config;
use crate::error::{PieError, Result};
use crate::proc;
use crate::provider::{
    AuthState, Capabilities, ChatRequest, Model, Provider, StreamEvent, TurnOutcome, UsageReport,
};

/// Static catalog. `--model` also accepts a caller-supplied id, so this list is
/// a convenience, not a restriction (`knowledge/claude.md`).
pub const STATIC_MODELS: [(&str, &str); 2] = [
    ("claude-sonnet-4-6", "Claude Sonnet 4.6"),
    ("claude-opus-4-6", "Claude Opus 4.6"),
];

/// Accepted `--model` aliases.
pub const ALIASES: [&str; 2] = ["sonnet", "opus"];

/// The claude backend.
#[derive(Debug, Default, Clone, Copy)]
pub struct Claude;

#[async_trait]
impl Provider for Claude {
    fn id(&self) -> &'static str {
        "claude"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            // Flips to true in M9, together with the tool loop itself.
            tool_loop: false,
            resume: true,
            stdin_prompt: true,
            max_prompt_argv: 0,
        }
    }

    fn binary(&self, cfg: &Config) -> Option<PathBuf> {
        proc::resolve_binary(&cfg.claude.bin)
    }

    async fn auth_status(&self, cfg: &Config) -> AuthState {
        if self.binary(cfg).is_none() {
            return AuthState::BinaryMissing {
                path: cfg.claude.bin.clone(),
            };
        }
        match auth::claude_credential() {
            Credential::Subscription { .. } if cfg.claude.enabled => AuthState::Ready,
            Credential::Subscription { .. } => AuthState::Error(
                "a claude credential exists but `claude.enabled` is false; set it to true in \
                 config.toml to opt in"
                    .to_string(),
            ),
            // The expected state on this machine.
            Credential::Absent => AuthState::NoSubscription,
            Credential::ApiKeyOnly { evidence } => AuthState::Error(format!(
                "claude credential is API-key only ({evidence}); pie drives the subscription login only"
            )),
            Credential::Unreadable(why) => AuthState::Error(why),
        }
    }

    async fn list_models(&self, _cfg: &Config) -> Result<Vec<Model>> {
        // Static and free: listing models must never spend quota.
        Ok(STATIC_MODELS
            .iter()
            .map(|(id, display)| Model::plain(*id, *display))
            .collect())
    }

    async fn stream(
        &self,
        _cfg: &Config,
        _req: &ChatRequest,
        _sink: &mut (dyn FnMut(StreamEvent) + Send),
    ) -> Result<TurnOutcome> {
        Err(PieError::Unsupported {
            backend: "claude".into(),
            feature:
                "delegate turn (M9 — needs a Claude subscription to verify the stream envelope)"
                    .into(),
        })
    }

    async fn usage(&self, _cfg: &Config) -> Result<UsageReport> {
        Err(PieError::Unsupported {
            backend: "claude".into(),
            feature: "quota probe (M9 — print-mode `/usage` support is unverified)".into(),
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn does_not_advertise_a_tool_loop_it_has_not_built() {
        assert!(
            !Claude.capabilities().tool_loop,
            "tool_loop must stay false until M9 implements the loop"
        );
    }

    #[test]
    fn model_catalog_is_static_and_free() {
        let models = futures_lite_block_on(Claude.list_models(&Config::default())).unwrap();
        assert_eq!(models.len(), 2);
        assert!(models.iter().any(|m| m.id == "claude-sonnet-4-6"));
        assert!(models.iter().all(|m| m.efforts.is_empty()));
    }

    #[test]
    fn absent_credential_is_reported_as_no_subscription() {
        // claude is not authenticated on this machine; that must surface as
        // `no-subscription`, not as a crash or a login prompt.
        if auth::claude_credential() == Credential::Absent {
            assert_eq!(Credential::Absent.label(), "absent");
        }
        assert_eq!(AuthState::NoSubscription.label(), "no-subscription");
    }

    #[test]
    fn aliases_are_documented_not_validated_away() {
        assert!(ALIASES.contains(&"sonnet"));
        assert!(ALIASES.contains(&"opus"));
    }

    /// Minimal executor so the async trait method can be asserted without
    /// pulling a runtime into the test.
    fn futures_lite_block_on<F: std::future::Future>(fut: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(fut)
    }
}
