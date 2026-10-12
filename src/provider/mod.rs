//! The provider contract: one shape over three vendor CLIs.
//!
//! `PLAN.md` §3 fixes the trait; `planning/DESIGN.md` §3–4 fixes the types.
//! `Capabilities` exists because the backends are *not* interchangeable
//! (`RISKS.md` §1): `agy --print` and `codex exec` are one-shot, so they run in
//! **delegate mode** and can never accept an injected `tool_result`.

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::error::{PieError, Result};

pub mod agy;
pub mod claude;
pub mod cli;
pub mod codex;

// M10 — re-enable by uncommenting both the module and its registry entry.
// pub mod openrouter;
// pub mod deepseek;

/// Provider ids, in display and registry order.
pub const BACKEND_IDS: [&str; 3] = ["agy", "codex", "claude"];

/// One selectable model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Model {
    /// Id passed to `--model`.
    pub id: String,
    /// Human-readable name.
    pub display: String,
    /// Accepted effort values, empty when the backend has no effort axis.
    pub efforts: Vec<String>,
    /// Effort used when the caller does not choose one.
    pub default_effort: Option<String>,
}

impl Model {
    /// A model with no effort axis.
    pub fn plain(id: impl Into<String>, display: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            display: display.into(),
            efforts: Vec::new(),
            default_effort: None,
        }
    }
}

/// What a backend can and cannot do. Drives delegate-vs-tool-loop branching.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    /// Can pie inject `tool_use` → `tool_result` mid-turn?
    pub tool_loop: bool,
    /// Can the backend resume a previous conversation?
    pub resume: bool,
    /// Does the CLI accept the prompt on stdin?
    pub stdin_prompt: bool,
    /// Maximum argv length for the prompt; `0` means unlimited.
    pub max_prompt_argv: usize,
}

/// One turn's request.
#[derive(Debug, Clone)]
pub struct ChatRequest {
    /// The user prompt.
    pub prompt: String,
    /// Bare model id, backend already selected.
    pub model: Option<String>,
    /// Reasoning effort, backend-specific vocabulary.
    pub effort: Option<String>,
    /// Working directory handed to the CLI.
    pub cwd: PathBuf,
    /// Conversation/thread id to resume.
    pub resume: Option<String>,
    /// Deadline override.
    pub timeout: Option<Duration>,
}

impl ChatRequest {
    /// A minimal one-shot request in the current directory.
    pub fn new(prompt: impl Into<String>) -> Self {
        Self {
            prompt: prompt.into(),
            model: None,
            effort: None,
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            resume: None,
            timeout: None,
        }
    }
}

/// Token accounting for one turn.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Usage {
    /// Prompt tokens.
    pub input_tokens: u64,
    /// Completion tokens.
    pub output_tokens: u64,
    /// Reasoning/thinking tokens, when reported separately.
    pub thinking_tokens: u64,
    /// Prompt tokens served from cache.
    pub cache_read_tokens: u64,
    /// Prompt tokens written to cache.
    pub cache_write_tokens: u64,
    /// Backend-reported total, when it differs from the sum.
    pub total_tokens: u64,
}

impl Usage {
    /// Sum of the parts, falling back to the backend's own total.
    pub fn computed_total(&self) -> u64 {
        let parts = self.input_tokens + self.output_tokens + self.thinking_tokens;
        if parts == 0 {
            self.total_tokens
        } else {
            parts
        }
    }
}

/// State of an in-flight tool call reported by a backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolState {
    /// Started.
    Active,
    /// Finished.
    Done,
    /// Failed.
    Error,
}

/// Normalised stream event. Unknown vendor events are forwarded, never fatal.
///
/// Adjacent tagging (`{"event":"text","data":"…"}`) rather than internal
/// tagging: an internally-tagged enum cannot represent a newtype variant
/// holding a bare string, which would have made `--json` fail at runtime.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", content = "data", rename_all = "snake_case")]
pub enum StreamEvent {
    /// Turn started; carries the conversation id we can resume later.
    #[serde(rename = "init")]
    Init {
        /// Vendor conversation/thread id.
        conversation_id: Option<String>,
        /// Working directory the CLI reported.
        cwd: Option<PathBuf>,
        /// Tools the CLI says it has.
        tools: Vec<String>,
    },
    /// A chunk of the answer.
    #[serde(rename = "text")]
    TextDelta(String),
    /// A tool step the CLI performed on its own.
    #[serde(rename = "tool")]
    ToolStep {
        /// Tool name.
        name: String,
        /// Lifecycle state.
        state: ToolState,
        /// Short description, when the CLI provides one.
        info: Option<String>,
    },
    /// Token accounting.
    #[serde(rename = "usage")]
    Usage(Usage),
    /// Terminal event.
    #[serde(rename = "done")]
    Done {
        /// Full answer text.
        text: String,
        /// Vendor status string (`SUCCESS`, `ERROR`, …).
        status: String,
    },
    /// An event kind we do not model yet — forwarded so nothing is lost.
    #[serde(rename = "other")]
    Other {
        /// The kind we saw.
        kind: String,
        /// The raw event.
        raw: serde_json::Value,
    },
    /// A stdout line that was not JSON (banner, warning).
    #[serde(rename = "unparsed")]
    Unparsed(String),
}

impl StreamEvent {
    /// Short label used by `--json` consumers and the human renderer.
    pub fn kind(&self) -> &'static str {
        match self {
            StreamEvent::Init { .. } => "init",
            StreamEvent::TextDelta(_) => "text",
            StreamEvent::ToolStep { .. } => "tool",
            StreamEvent::Usage(_) => "usage",
            StreamEvent::Done { .. } => "done",
            StreamEvent::Other { .. } => "other",
            StreamEvent::Unparsed(_) => "unparsed",
        }
    }
}

/// The finished turn.
#[derive(Debug, Clone)]
pub struct TurnOutcome {
    /// Full answer text.
    pub text: String,
    /// Token accounting.
    pub usage: Usage,
    /// Vendor status string.
    pub status: String,
    /// Conversation id for a later resume.
    pub conversation_id: Option<String>,
    /// Wall-clock duration.
    pub duration: Duration,
}

/// Whether a backend can be used right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthState {
    /// Credential present and usable.
    Ready,
    /// Installed but not logged in.
    NeedsLogin {
        /// What the user should run.
        hint: String,
    },
    /// Installed, but this machine has no subscription for it.
    NoSubscription,
    /// Binary not found.
    BinaryMissing {
        /// Path we looked at.
        path: String,
    },
    /// Probe failed.
    Error(String),
}

impl AuthState {
    /// Short label for tables.
    pub fn label(&self) -> &'static str {
        match self {
            AuthState::Ready => "ready",
            AuthState::NeedsLogin { .. } => "needs-login",
            AuthState::NoSubscription => "no-subscription",
            AuthState::BinaryMissing { .. } => "binary-missing",
            AuthState::Error(_) => "error",
        }
    }

    /// Extra detail for tables.
    pub fn detail(&self) -> Option<String> {
        match self {
            AuthState::NeedsLogin { hint } => Some(hint.clone()),
            AuthState::BinaryMissing { path } => Some(path.clone()),
            AuthState::Error(e) => Some(e.clone()),
            _ => None,
        }
    }
}

/// One quota bucket.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Bucket {
    /// Bucket name (`5h`, `weekly`, …).
    pub label: String,
    /// Fraction remaining, 0.0–1.0, when the backend reports it.
    pub remaining_fraction: Option<f64>,
    /// Reset timestamp, as reported.
    pub reset_at: Option<String>,
    /// Units consumed, when reported.
    pub used: Option<u64>,
    /// Unit limit, when reported.
    pub limit: Option<u64>,
}

/// Quota snapshot for one backend.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageReport {
    /// Provider id.
    pub backend: String,
    /// Buckets, in backend order.
    pub buckets: Vec<Bucket>,
    /// Where the numbers came from (`agy /usage`, `chatgpt wham/usage`).
    pub source: String,
    /// Caveats worth showing the user.
    pub notes: Vec<String>,
}

/// The trait every backend implements.
///
/// `async_trait` is required: native `async fn` in traits is not
/// dyn-compatible, and the registry holds `Box<dyn Provider>`.
#[async_trait]
pub trait Provider: Send + Sync {
    /// Stable id, also the `--model` prefix.
    fn id(&self) -> &'static str;

    /// What this backend supports.
    fn capabilities(&self) -> Capabilities;

    /// Resolved binary path, or `None` when not installed.
    fn binary(&self, cfg: &Config) -> Option<PathBuf>;

    /// Argv for the version snapshot used by drift detection.
    fn version_args(&self) -> &'static [&'static str] {
        &["--version"]
    }

    /// Program **and** leading args used to probe the version.
    ///
    /// Backends that cannot be launched directly — codex on Windows is
    /// `node <codex.js>` — must override this, or drift detection ends up
    /// recording the version of the *launcher* instead of the CLI. That was a
    /// real bug: `codex` reported node's `v25.9.0`.
    fn version_command(&self, cfg: &Config) -> Option<(PathBuf, Vec<OsString>)> {
        let program = self.binary(cfg)?;
        Some((
            program,
            self.version_args().iter().map(OsString::from).collect(),
        ))
    }

    /// Probe credentials without spending model quota.
    async fn auth_status(&self, cfg: &Config) -> AuthState;

    /// Available models. Must not spend model quota.
    async fn list_models(&self, cfg: &Config) -> Result<Vec<Model>>;

    /// Run one turn, invoking `sink` as events arrive.
    async fn stream(
        &self,
        cfg: &Config,
        req: &ChatRequest,
        sink: &mut (dyn FnMut(StreamEvent) + Send),
    ) -> Result<TurnOutcome>;

    /// Quota snapshot. Must not spend model quota.
    async fn usage(&self, cfg: &Config) -> Result<UsageReport>;
}

/// All backends, in display order.
pub fn registry() -> Vec<Box<dyn Provider>> {
    vec![
        Box::new(agy::Agy),
        Box::new(codex::Codex),
        Box::new(claude::Claude),
        // M10: Box::new(openrouter::OpenRouter),
        // M10: Box::new(deepseek::DeepSeek),
    ]
}

/// Look up one backend by id.
pub fn by_id(id: &str) -> Option<Box<dyn Provider>> {
    registry().into_iter().find(|p| p.id() == id)
}

/// Split a `provider/model` spec.
pub fn parse_model_spec(spec: &str) -> Result<(String, String)> {
    match spec.split_once('/') {
        Some((p, m)) if !p.trim().is_empty() && !m.trim().is_empty() => {
            Ok((p.trim().to_string(), m.trim().to_string()))
        }
        _ => Err(PieError::BadModelSpec {
            spec: spec.to_string(),
        }),
    }
}

/// Which backend and model a run should use.
///
/// Order: explicit `--model`, then `general.default_model`, then an error with
/// actionable advice — never a silent default, because a wrong default spends
/// the wrong subscription.
pub fn resolve_target(cfg: &Config, model_flag: Option<&str>) -> Result<(String, String)> {
    let spec = match model_flag
        .map(str::to_string)
        .or_else(|| cfg.general.default_model.clone())
    {
        Some(s) => s,
        None => {
            return Err(PieError::Config(
                "no model selected — pass `--model provider/model` or set \
                 `general.default_model` in config.toml"
                    .to_string(),
            ))
        }
    };

    let (backend, model) = parse_model_spec(&spec)?;
    if by_id(&backend).is_none() {
        return Err(PieError::UnknownBackend {
            backend,
            known: BACKEND_IDS.iter().map(|s| (*s).to_string()).collect(),
        });
    }
    Ok((backend, model))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn registry_lists_exactly_the_three_cli_backends() {
        let ids: Vec<&str> = registry().iter().map(|p| p.id()).collect();
        assert_eq!(ids, vec!["agy", "codex", "claude"]);
        // No HTTP backend may leak in while M10 is deferred.
        assert!(!ids.contains(&"openrouter"));
        assert!(!ids.contains(&"deepseek"));
    }

    #[test]
    fn cli_backends_are_delegate_mode_not_tool_loop() {
        for p in registry() {
            let caps = p.capabilities();
            assert!(
                !caps.tool_loop,
                "{} must be delegate-mode in M0-M2 (RISKS §1)",
                p.id()
            );
        }
    }

    #[test]
    fn agy_declares_no_stdin_prompt_and_a_cap() {
        let agy = registry().into_iter().find(|p| p.id() == "agy").unwrap();
        let caps = agy.capabilities();
        assert!(!caps.stdin_prompt, "agy has no stdin prompt path");
        assert_eq!(caps.max_prompt_argv, 20_000);
    }

    #[test]
    fn model_spec_parsing() {
        assert_eq!(
            parse_model_spec("agy/gemini-3.8-flash").unwrap(),
            ("agy".to_string(), "gemini-3.8-flash".to_string())
        );
        // only the first slash splits, so ids containing a slash survive
        assert_eq!(
            parse_model_spec("codex/gpt-6/luna").unwrap().1,
            "gpt-6/luna"
        );
        for bad in ["", "agy", "/model", "agy/", "  "] {
            assert!(
                matches!(parse_model_spec(bad), Err(PieError::BadModelSpec { .. })),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn resolve_target_prefers_flag_then_config_then_errors() {
        let mut cfg = Config::default();
        assert!(matches!(
            resolve_target(&cfg, None),
            Err(PieError::Config(_))
        ));

        cfg.general.default_model = Some("codex/gpt-6-luna".into());
        assert_eq!(
            resolve_target(&cfg, None).unwrap(),
            ("codex".to_string(), "gpt-6-luna".to_string())
        );
        assert_eq!(
            resolve_target(&cfg, Some("agy/gemini-3.8-flash")).unwrap(),
            ("agy".to_string(), "gemini-3.8-flash".to_string())
        );
    }

    #[test]
    fn unknown_backend_lists_the_known_ones() {
        let cfg = Config::default();
        let err = resolve_target(&cfg, Some("gemini/x")).unwrap_err();
        match err {
            PieError::UnknownBackend { backend, known } => {
                assert_eq!(backend, "gemini");
                assert!(known.contains(&"agy".to_string()));
            }
            other => panic!("wrong error: {other:?}"),
        }
    }

    #[test]
    fn usage_total_falls_back_to_backend_total() {
        let mut u = Usage {
            total_tokens: 42,
            ..Usage::default()
        };
        assert_eq!(u.computed_total(), 42);
        u.input_tokens = 10;
        u.output_tokens = 5;
        assert_eq!(u.computed_total(), 15, "parts win when present");
    }

    #[test]
    fn events_serialise_with_a_kind_tag() {
        let v = serde_json::to_value(StreamEvent::TextDelta("hi".into())).unwrap();
        assert_eq!(v["event"], "text");
        assert_eq!(v["data"], "hi");

        let v = serde_json::to_value(StreamEvent::Done {
            text: "x".into(),
            status: "SUCCESS".into(),
        })
        .unwrap();
        assert_eq!(v["event"], "done");
        assert_eq!(v["data"]["status"], "SUCCESS");
        assert_eq!(v["data"]["text"], "x");

        // Every variant must round-trip; internally-tagged enums cannot hold a
        // newtype variant, which is why this uses adjacent tagging.
        let events = vec![
            StreamEvent::Init {
                conversation_id: Some("c".into()),
                cwd: Some(PathBuf::from("/w")),
                tools: vec!["view_file".into()],
            },
            StreamEvent::Usage(Usage::default()),
            StreamEvent::ToolStep {
                name: "run_command".into(),
                state: ToolState::Done,
                info: None,
            },
            StreamEvent::Other {
                kind: "mystery".into(),
                raw: serde_json::json!({"a":1}),
            },
            StreamEvent::Unparsed("banner".into()),
        ];
        for ev in events {
            let s = serde_json::to_string(&ev).expect("serialises");
            let tag = serde_json::to_value(&ev).unwrap()["event"]
                .as_str()
                .expect("tag")
                .to_string();
            // The `--json` wire tag and the human renderer's label must agree.
            assert_eq!(tag, ev.kind(), "tag/label drift for {s}");
            let back: StreamEvent = serde_json::from_str(&s).expect("round-trips");
            assert_eq!(back, ev, "{s}");
        }
    }
}
