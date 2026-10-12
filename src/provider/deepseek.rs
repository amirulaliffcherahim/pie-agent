//! `deepseek` backend — COMMENTED OUT.
//!
//! Re-enable by removing the comments below **and** uncommenting
//! `pub mod deepseek;` plus its registry entry in `provider/mod.rs`.
//! Deferred to M10 (`ROADMAP.md`).
//!
//! Same shape as `openrouter.rs`: a plain API-key, OpenAI-compatible backend
//! that needs the SSE `tool_calls` path. Keeping both commented files present
//! (rather than deleted) is what `PLAN.md` §1 asks for — they come back with a
//! `fix:`/`feat:` commit, not a rewrite.
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
// pub const KEY_ENV: &str = "DEEPSEEK_API_KEY";
//
// /// OpenAI-compatible chat-completions endpoint.
// pub const BASE_URL: &str = "https://api.deepseek.com/v1";
//
// /// DeepSeek backend.
// #[derive(Debug, Default, Clone, Copy)]
// pub struct DeepSeek;
//
// #[async_trait]
// impl Provider for DeepSeek {
//     fn id(&self) -> &'static str {
//         "deepseek"
//     }
//
//     fn capabilities(&self) -> Capabilities {
//         Capabilities {
//             tool_loop: true,
//             resume: false,
//             stdin_prompt: false,
//             max_prompt_argv: 0,
//         }
//     }
//
//     fn binary(&self, _cfg: &Config) -> Option<std::path::PathBuf> {
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
//             backend: "deepseek".into(),
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
//             backend: "deepseek".into(),
//             feature: "SSE streaming + tool_calls (M10)".into(),
//         })
//     }
//
//     async fn usage(&self, _cfg: &Config) -> Result<UsageReport> {
//         Err(PieError::Unsupported {
//             backend: "deepseek".into(),
//             feature: "quota probe (M10)".into(),
//         })
//     }
// }
// ```
