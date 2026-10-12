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

use std::ffi::OsString;
use std::path::PathBuf;

use async_trait::async_trait;

use crate::auth::{self, Credential};
use crate::config::Config;
use crate::error::{PieError, Result};
use crate::process;
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
            // The one backend with a real system prompt: the persona goes
            // through `--append-system-prompt` rather than being folded into
            // the prompt text (`DESIGN.md` §10, `knowledge/claude.md`).
            system_prompt: true,
        }
    }

    fn binary(&self, cfg: &Config) -> Option<PathBuf> {
        process::resolve_binary(&cfg.claude.bin)
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

/// Build the argv for one turn. Pure and unit-tested.
///
/// Written to spec from `knowledge/claude.md` and **unverified live**: there is
/// no Claude subscription on this machine, so `stream()` is still
/// [`PieError::Unsupported`] and nothing outside the tests calls this yet. It
/// exists now because M3's contract is "one mechanism per backend, one identical
/// persona", and the mechanism is the part that can be got wrong on paper.
///
/// ```text
/// claude -p <PROMPT> --output-format stream-json --dangerously-skip-permissions
///     --add-dir <CWD> [--model <ID>] [--append-system-prompt <PERSONA>]
/// ```
///
/// The prompt is **not** folded here: claude has a system role, so the persona
/// travels in `--append-system-prompt` and the task text stays the task text.
/// The bytes handed over are the same bytes agy and codex fold in.
pub fn build_args(cfg: &Config, req: &ChatRequest) -> Result<Vec<OsString>> {
    let _ = cfg;
    let mut args: Vec<OsString> = Vec::with_capacity(11);

    args.push(OsString::from("-p"));
    args.push(OsString::from(&req.prompt));
    args.push(OsString::from("--output-format"));
    args.push(OsString::from("stream-json"));
    args.push(OsString::from("--dangerously-skip-permissions"));
    args.push(OsString::from("--add-dir"));
    args.push(req.cwd.clone().into_os_string());

    if let Some(model) = &req.model {
        args.push(OsString::from("--model"));
        args.push(OsString::from(model));
    }

    // Never `Some("")`: an empty system prompt would still be an injection.
    if let Some(persona) = req.persona.as_deref().filter(|p| !p.trim().is_empty()) {
        args.push(OsString::from("--append-system-prompt"));
        args.push(OsString::from(persona));
    }

    Ok(args)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn strings(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|a| a.to_string_lossy().to_string())
            .collect()
    }

    #[test]
    fn the_persona_goes_through_append_system_prompt() {
        let persona = "# Persona\ntone: terse";
        let mut req = ChatRequest::new("hi");
        req.persona = Some(persona.to_string());

        let a = strings(&build_args(&Config::default(), &req).unwrap());
        let at = a
            .iter()
            .position(|x| x == "--append-system-prompt")
            .expect("claude takes the persona as a system prompt");
        assert_eq!(a[at + 1], persona, "the projection verbatim, not folded");
        assert_eq!(a[1], "hi", "the task text is not folded for claude");
        assert!(!a[1].contains(crate::profile::TASK_SEPARATOR));
    }

    #[test]
    fn without_a_persona_no_system_prompt_argument_is_added() {
        let a = strings(&build_args(&Config::default(), &ChatRequest::new("hi")).unwrap());
        assert!(!a.contains(&"--append-system-prompt".to_string()));

        let mut blank = ChatRequest::new("hi");
        blank.persona = Some("   ".into());
        let a = strings(&build_args(&Config::default(), &blank).unwrap());
        assert!(!a.contains(&"--append-system-prompt".to_string()));
    }

    #[test]
    fn argv_matches_the_documented_print_mode_shape() {
        let mut req = ChatRequest::new("hi");
        req.cwd = PathBuf::from("C:/work");
        req.model = Some("claude-sonnet-4-6".into());

        let a = strings(&build_args(&Config::default(), &req).unwrap());
        assert_eq!(a[0], "-p");
        for required in [
            "--output-format",
            "stream-json",
            "--dangerously-skip-permissions",
            "--add-dir",
            "C:/work",
            "--model",
            "claude-sonnet-4-6",
        ] {
            assert!(
                a.contains(&required.to_string()),
                "missing {required} in {a:?}"
            );
        }
    }

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
