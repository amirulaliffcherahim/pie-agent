//! Google Antigravity (`agy`) backend — delegate mode.
//!
//! M0 delivers the argv contract and capabilities. The stream parser lands in
//! M1; until then `stream`/`list_models`/`usage` report `Unsupported` rather
//! than pretending to work.
//!
//! Verified contract: `knowledge/agy.md`.

use std::ffi::OsString;
use std::path::PathBuf;

use async_trait::async_trait;

use crate::auth::{self, Credential};
use crate::config::Config;
use crate::error::{PieError, Result};
use crate::process;
use crate::provider::driver::{cap_for, enforce_argv_cap, TurnPlan};
use crate::provider::{
    AuthState, Capabilities, ChatRequest, Model, Provider, StreamEvent, TurnOutcome, UsageReport,
};

/// Effort values agy accepts. `--model` is rejected without `--effort`.
pub const EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

/// agy has no stdin prompt path, so the prompt must fit in argv.
pub const PROMPT_ARGV_CAP: usize = 20_000;

/// Hint shown when a prompt exceeds the argv cap.
pub const PROMPT_CAP_HINT: &str =
    "agy has no stdin prompt path; shorten the prompt or split the task";

/// The agy backend.
#[derive(Debug, Default, Clone, Copy)]
pub struct Agy;

#[async_trait]
impl Provider for Agy {
    fn id(&self) -> &'static str {
        "agy"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            // `agy --print` is one-shot: it runs its own tools and returns a
            // final answer, so pie can never inject a tool_result (RISKS §1).
            tool_loop: false,
            resume: true,
            stdin_prompt: false,
            max_prompt_argv: PROMPT_ARGV_CAP,
        }
    }

    fn binary(&self, cfg: &Config) -> Option<PathBuf> {
        process::resolve_binary(&cfg.agy.bin)
    }

    async fn auth_status(&self, cfg: &Config) -> AuthState {
        if self.binary(cfg).is_none() {
            return AuthState::BinaryMissing {
                path: cfg.agy.bin.clone(),
            };
        }
        match auth::agy_credential() {
            Credential::Subscription { .. } => AuthState::Ready,
            Credential::ApiKeyOnly { evidence } => AuthState::Error(format!(
                "agy reports an API-key credential ({evidence}); pie drives the subscription login only"
            )),
            Credential::Absent => AuthState::NeedsLogin {
                hint: "run `agy` interactively and complete the Google login".to_string(),
            },
            Credential::Unreadable(why) => AuthState::Error(why),
        }
    }

    async fn list_models(&self, _cfg: &Config) -> Result<Vec<Model>> {
        Err(PieError::Unsupported {
            backend: "agy".into(),
            feature: "model catalog (M1: `agy models` + 24h cache)".into(),
        })
    }

    async fn stream(
        &self,
        _cfg: &Config,
        _req: &ChatRequest,
        _sink: &mut (dyn FnMut(StreamEvent) + Send),
    ) -> Result<TurnOutcome> {
        Err(PieError::Unsupported {
            backend: "agy".into(),
            feature: "delegate turn (M1: stream-json parser)".into(),
        })
    }

    async fn usage(&self, _cfg: &Config) -> Result<UsageReport> {
        Err(PieError::Unsupported {
            backend: "agy".into(),
            feature: "quota probe (M1: `agy --print /usage --output-format json`)".into(),
        })
    }
}

/// Build the argv for one turn. Pure and unit-tested — no process involved.
///
/// Verified shape (`knowledge/agy.md`):
/// ```text
/// agy --print <PROMPT> --dangerously-skip-permissions --disable-slash-commands
///     --output-format stream-json --print-timeout <N>s --add-dir <CWD>
///     [--model <ID> --effort <E>] [--conversation <ID>]
/// ```
pub fn build_args(cfg: &Config, req: &ChatRequest) -> Result<Vec<OsString>> {
    let mut args: Vec<OsString> = Vec::with_capacity(14);

    // `--print` consumes the next token as the prompt.
    args.push(OsString::from("--print"));
    args.push(OsString::from(&req.prompt));

    args.push(OsString::from("--dangerously-skip-permissions"));
    // Keeps a leading `/` in a prompt from being expanded as a slash command.
    args.push(OsString::from("--disable-slash-commands"));
    args.push(OsString::from("--output-format"));
    args.push(OsString::from("stream-json"));
    args.push(OsString::from("--print-timeout"));
    args.push(OsString::from(format!("{}s", cfg.agy.print_timeout_secs)));
    args.push(OsString::from("--add-dir"));
    args.push(req.cwd.clone().into_os_string());

    if let Some(model) = &req.model {
        // agy rejects `--model` without `--effort`, so we must supply one.
        let effort = match req
            .effort
            .clone()
            .or_else(|| cfg.agy.default_effort.clone())
        {
            Some(e) => e,
            None => {
                return Err(PieError::Config(format!(
                    "agy requires an effort whenever a model is chosen, but no `--effort` was \
                     given and `agy.default_effort` is unset — pass `--effort <{}>`",
                    EFFORTS.join("|")
                )))
            }
        };
        args.push(OsString::from("--model"));
        args.push(OsString::from(model));
        args.push(OsString::from("--effort"));
        args.push(OsString::from(effort));
    }

    if let Some(conversation) = &req.resume {
        args.push(OsString::from("--conversation"));
        args.push(OsString::from(conversation));
    }

    Ok(args)
}

/// argv for the quota probe.
///
/// Deliberately **without** `--disable-slash-commands`: `/usage` is a slash
/// expansion, and disabling expansion would make it a literal prompt
/// (`knowledge/agy.md`).
pub fn usage_args() -> Vec<OsString> {
    [
        "--print",
        "/usage",
        "--output-format",
        "json",
        "--print-timeout",
        "30s",
    ]
    .iter()
    .map(OsString::from)
    .collect()
}

/// Build a runnable plan, enforcing the argv cap.
pub fn build_plan(cfg: &Config, req: &ChatRequest) -> Result<TurnPlan> {
    let program = cfg.agy.bin.clone();
    let program_path = PathBuf::from(&program);
    let args = build_args(cfg, req)?;

    let cap = cap_for(&self_caps(), cfg);
    enforce_argv_cap("agy", &program_path, &args, cap, PROMPT_CAP_HINT)?;

    Ok(TurnPlan {
        program: program_path,
        args,
        cwd: req.cwd.clone(),
        scrub: cfg.scrub_keys(cfg.scrub_enabled()),
        timeout: req.timeout.unwrap_or_else(|| cfg.timeout()),
    })
}

fn self_caps() -> Capabilities {
    Agy.capabilities()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn cfg() -> Config {
        Config::default()
    }

    fn req() -> ChatRequest {
        let mut r = ChatRequest::new("hi");
        r.cwd = PathBuf::from("C:/work");
        r
    }

    fn args_as_strings(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|a| a.to_string_lossy().to_string())
            .collect()
    }

    #[test]
    fn argv_matches_the_verified_contract() {
        let a = args_as_strings(&build_args(&cfg(), &req()).unwrap());
        assert_eq!(a[0], "--print");
        assert_eq!(a[1], "hi", "prompt immediately follows --print");
        for required in [
            "--dangerously-skip-permissions",
            "--disable-slash-commands",
            "--output-format",
            "stream-json",
            "--print-timeout",
            "--add-dir",
            "C:/work",
        ] {
            assert!(
                a.contains(&required.to_string()),
                "missing {required} in {a:?}"
            );
        }
        assert!(
            !a.iter().any(|x| x == "--model"),
            "no model means no --model"
        );
    }

    #[test]
    fn print_timeout_is_always_passed_with_a_seconds_suffix() {
        let a = args_as_strings(&build_args(&cfg(), &req()).unwrap());
        let i = a.iter().position(|x| x == "--print-timeout").unwrap();
        assert_eq!(a[i + 1], "0s", "0 means wait for completion by default");

        let mut c = cfg();
        c.agy.print_timeout_secs = 45;
        let a = args_as_strings(&build_args(&c, &req()).unwrap());
        let i = a.iter().position(|x| x == "--print-timeout").unwrap();
        assert_eq!(a[i + 1], "45s");
    }

    #[test]
    fn a_model_always_gets_an_effort() {
        let mut r = req();
        r.model = Some("gemini-3.8-flash".into());
        let a = args_as_strings(&build_args(&cfg(), &r).unwrap());
        let m = a.iter().position(|x| x == "--model").unwrap();
        assert_eq!(a[m + 1], "gemini-3.8-flash");
        let e = a.iter().position(|x| x == "--effort").unwrap();
        assert_eq!(a[e + 1], "medium", "config default_effort is injected");
    }

    #[test]
    fn explicit_effort_beats_the_default() {
        let mut r = req();
        r.model = Some("gemini-3.1-pro".into());
        r.effort = Some("high".into());
        let a = args_as_strings(&build_args(&cfg(), &r).unwrap());
        let e = a.iter().position(|x| x == "--effort").unwrap();
        assert_eq!(a[e + 1], "high");
    }

    #[test]
    fn model_without_any_effort_is_a_config_error_not_a_broken_command() {
        let mut c = cfg();
        c.agy.default_effort = None;
        let mut r = req();
        r.model = Some("gemini-3.8-flash".into());

        let err = build_args(&c, &r).unwrap_err();
        assert!(matches!(err, PieError::Config(_)));
        assert_eq!(err.exit_code(), 2);
        assert!(
            err.to_string().contains("effort"),
            "message explains the fix"
        );
    }

    #[test]
    fn usage_argv_does_not_disable_slash_commands() {
        let a = args_as_strings(&usage_args());
        assert!(a.contains(&"/usage".to_string()));
        assert!(
            !a.contains(&"--disable-slash-commands".to_string()),
            "/usage needs slash expansion — this asymmetry is deliberate"
        );
        assert!(a.contains(&"--print".to_string()));
    }

    #[test]
    fn resume_uses_conversation_flag() {
        let mut r = req();
        r.resume = Some("conv-123".into());
        let a = args_as_strings(&build_args(&cfg(), &r).unwrap());
        let i = a.iter().position(|x| x == "--conversation").unwrap();
        assert_eq!(a[i + 1], "conv-123");
    }

    #[test]
    fn oversized_prompt_is_refused_before_spawning() {
        let mut r = req();
        r.prompt = "p".repeat(30_000);
        let err = build_plan(&cfg(), &r).unwrap_err();
        assert!(matches!(err, PieError::PromptTooLong { .. }));
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn plan_carries_the_scrub_list_and_deadline() {
        let plan = build_plan(&cfg(), &req()).unwrap();
        assert!(plan.scrub.contains(&"OPENAI_API_KEY".to_string()));
        assert_eq!(plan.timeout.as_secs(), 600);

        let mut r = req();
        r.timeout = Some(std::time::Duration::from_secs(3));
        assert_eq!(build_plan(&cfg(), &r).unwrap().timeout.as_secs(), 3);
    }
}
