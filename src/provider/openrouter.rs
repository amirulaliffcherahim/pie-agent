//! `openrouter` backend — COMMENTED OUT.
//!
//! Re-enable by removing the comments below **and** uncommenting
//! `pub mod openrouter;` plus its registry entry in `provider/mod.rs`.
//! Deferred to M10 (`ROADMAP.md`).
//!
//! This is a plain API-key backend, so unlike agy/codex/claude it is *allowed*
//! to read `OPENROUTER_API_KEY` — which is exactly why its registration must be
//! explicit: it is the one backend that must **not** be scrubbed or refused.
//! It also needs the tool loop (`tool_calls` over SSE), which the CLI backends
//! never exercise (`RISKS.md` §8).
//
// ```rust
// use async_trait::async_trait;
// use crate::config::Config;
// use crate::error::{PieError, Result};
// use crate::provider::{
//     AuthState, Capabilities, ChatRequest, Model, Provider, StreamEvent, TurnOutcome,
//     UsageReport,
// };
//
// /// Key env var for this backend.
// pub const KEY_ENV: &str = "OPENROUTER_API_KEY";
//
// /// OpenAI-compatible chat-completions endpoint.
// pub const BASE_URL: &str = "https://openrouter.ai/api/v1";
//
// /// OpenRouter backend.
// #[derive(Debug, Default, Clone, Copy)]
// pub struct OpenRouter;
//
// #[async_trait]
// impl Provider for OpenRouter {
//     fn id(&self) -> &'static str {
//         "openrouter"
//     }
//
//     fn capabilities(&self) -> Capabilities {
//         Capabilities {
//             // HTTP backends DO support the tool loop.
//             tool_loop: true,
//             resume: false,
//             stdin_prompt: false,
//             max_prompt_argv: 0,
//         }
//     }
//
//     fn binary(&self, _cfg: &Config) -> Option<std::path::PathBuf> {
//         // Not a subprocess backend.
//         None
//     }
//
//     async fn auth_status(&self, _cfg: &Config) -> AuthState {
//         if std::env::var(KEY_ENV).map(|v| !v.trim().is_empty()).unwrap_or(false) {
//             AuthState::Ready
//         } else {
//             AuthState::NeedsLogin {
//                 hint: format!("set {KEY_ENV}"),
//             }
//         }
//     }
//
//     async fn list_models(&self, _cfg: &Config) -> Result<Vec<Model>> {
//         Err(PieError::Unsupported {
//             backend: "openrouter".into(),
//             feature: "model catalog (M10)".into(),
//         })
//     }
//
//     async fn stream(
//         &self,
//         _cfg: &Config,
//         _req: &ChatRequest,
//         _sink: &mut (dyn FnMut(StreamEvent) + Send),
//     ) -> Result<TurnOutcome> {
//         Err(PieError::Unsupported {
//             backend: "openrouter".into(),
//             feature: "SSE streaming + tool_calls (M10)".into(),
//         })
//     }
//
//     async fn usage(&self, _cfg: &Config) -> Result<UsageReport> {
//         Err(PieError::Unsupported {
//             backend: "openrouter".into(),
//             feature: "quota probe (M10)".into(),
//         })
//     }
// }
// ```
