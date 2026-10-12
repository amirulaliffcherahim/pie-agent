//! OpenAI Codex (`codex`) backend — delegate mode.
//!
//! M0 delivers binary resolution, the argv contract, and the subscription-only
//! auth gate. The JSONL parser lands in M2.
//!
//! Verified contract: `knowledge/codex.md`.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use async_trait::async_trait;

use crate::auth::{self, Credential};
use crate::config::Config;
use crate::error::{PieError, Result};
use crate::proc;
use crate::provider::cli::{self, TurnPlan};
use crate::provider::{
    AuthState, Capabilities, ChatRequest, Model, Provider, StreamEvent, TurnOutcome, UsageReport,
};

/// Path of the npm launcher script, relative to an npm root.
pub const NODE_SCRIPT_RELATIVE: &str = "node_modules/@openai/codex/bin/codex.js";

/// Hint shown when no subscription credential is available.
pub const NO_SUBSCRIPTION_HINT: &str =
    "run `codex login` to use your ChatGPT/Codex plan; pie will not run codex in API-key mode";

/// The codex backend.
#[derive(Debug, Default, Clone, Copy)]
pub struct Codex;

#[async_trait]
impl Provider for Codex {
    fn id(&self) -> &'static str {
        "codex"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            // `codex exec` is one-shot (RISKS §1).
            tool_loop: false,
            resume: true,
            // No prompt arg reads stdin; `-` also reads stdin.
            stdin_prompt: true,
            // 0 = no inherent argv limit. M0 still passes the prompt on argv, so
            // the config cap applies; M2 switches oversized prompts to stdin.
            max_prompt_argv: 0,
        }
    }

    fn binary(&self, cfg: &Config) -> Option<PathBuf> {
        program_and_prefix(cfg).map(|(program, _)| program)
    }

    /// codex is launched as `node <codex.js>` on this machine, so the version
    /// probe must carry the script prefix or it reports node's version.
    fn version_command(&self, cfg: &Config) -> Option<(PathBuf, Vec<OsString>)> {
        let (program, mut prefix) = program_and_prefix(cfg)?;
        prefix.push(OsString::from("--version"));
        Some((program, prefix))
    }

    async fn auth_status(&self, cfg: &Config) -> AuthState {
        if self.binary(cfg).is_none() {
            return AuthState::BinaryMissing {
                path: cfg.codex.bin.clone(),
            };
        }
        match auth::codex_credential() {
            Credential::Subscription { .. } => AuthState::Ready,
            Credential::ApiKeyOnly { evidence } => AuthState::Error(format!(
                "codex credential is API-key only ({evidence}) — {NO_SUBSCRIPTION_HINT}"
            )),
            Credential::Absent => AuthState::NeedsLogin {
                hint: "run `codex login`".to_string(),
            },
            Credential::Unreadable(why) => AuthState::Error(why),
        }
    }

    async fn list_models(&self, _cfg: &Config) -> Result<Vec<Model>> {
        Err(PieError::Unsupported {
            backend: "codex".into(),
            feature: "model catalog (M2: `~/.codex/models_cache.json`)".into(),
        })
    }

    async fn stream(
        &self,
        _cfg: &Config,
        _req: &ChatRequest,
        _sink: &mut (dyn FnMut(StreamEvent) + Send),
    ) -> Result<TurnOutcome> {
        Err(PieError::Unsupported {
            backend: "codex".into(),
            feature: "delegate turn (M2: JSONL parser)".into(),
        })
    }

    async fn usage(&self, _cfg: &Config) -> Result<UsageReport> {
        Err(PieError::Unsupported {
            backend: "codex".into(),
            feature: "quota probe (M2: ChatGPT wham/usage)".into(),
        })
    }
}

/// Resolve how to actually start codex: `(program, leading_args)`.
///
/// The Windows `codex.cmd` shim **cannot be spawned without a shell**, so it is
/// never used (`knowledge/codex.md`). Preference order:
///
/// 1. an explicit path from config (a `.js` path is run through `node`),
/// 2. a native `codex.exe` / `codex` on `PATH`,
/// 3. `node <npm-root>/node_modules/@openai/codex/bin/codex.js`.
pub fn program_and_prefix(cfg: &Config) -> Option<(PathBuf, Vec<OsString>)> {
    let configured = PathBuf::from(&cfg.codex.bin);

    // 1. Explicit / path-like configuration.
    if configured.is_file() {
        if is_javascript(&configured) {
            let node = proc::resolve_binary("node")?;
            return Some((node, vec![configured.into_os_string()]));
        }
        return Some((configured, Vec::new()));
    }

    // 2. A real executable on PATH — but never the `.cmd`/`.bat` shim.
    if let Some(found) = resolve_executable(&cfg.codex.bin) {
        return Some((found, Vec::new()));
    }

    // 3. node + the npm launcher, which is what the shim would have run.
    if cfg.codex.use_node_shim {
        if let (Some(script), Some(node)) = (find_node_script(cfg), proc::resolve_binary("node")) {
            return Some((node, vec![script.into_os_string()]));
        }
    }

    None
}

fn is_javascript(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("js") | Some("mjs") | Some("cjs")
    )
}

/// Resolve a binary on `PATH`, skipping Windows shell shims.
fn resolve_executable(spec: &str) -> Option<PathBuf> {
    let found = proc::resolve_binary(spec)?;
    if is_shell_shim(&found) {
        return None;
    }
    Some(found)
}

/// A `.cmd`/`.bat`/`.ps1` script is not directly spawnable.
fn is_shell_shim(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("cmd") | Some("bat") | Some("ps1")
    )
}

/// Locate the npm-installed `codex.js`, if node/npm put it somewhere standard.
fn find_node_script(cfg: &Config) -> Option<PathBuf> {
    for root in npm_root_candidates(cfg) {
        let candidate = root.join(NODE_SCRIPT_RELATIVE);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Places an npm global install may live, per platform.
fn npm_root_candidates(cfg: &Config) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();

    // Next to the configured binary, if that resolves to a shim.
    if let Some(shim) = proc::resolve_binary(&cfg.codex.bin) {
        if let Some(dir) = shim.parent() {
            roots.push(dir.to_path_buf());
            // `%APPDATA%\npm\codex.cmd` → `%APPDATA%\npm\node_modules\...`
            if let Some(grandparent) = dir.parent() {
                roots.push(grandparent.to_path_buf());
            }
        }
    }

    if let Some(home) = crate::paths::home_dir() {
        // npm's default per-user global root, and common alternatives.
        roots.push(home.join("node_modules"));
        roots.push(home.join(".npm-global").join("lib"));
        roots.push(home.join(".local").join("lib"));
        // nvm / fnm / volta style layouts.
        roots.push(home.join(".volta").join("tools").join("image"));
        roots.push(home.join(".local").join("share").join("fnm"));
    }

    if let Some(appdata) = std::env::var_os("APPDATA") {
        roots.push(PathBuf::from(appdata).join("npm"));
    }

    // System-wide installs (Linux, WSL, macOS).
    roots.push(PathBuf::from("/usr/lib"));
    roots.push(PathBuf::from("/usr/local/lib"));
    roots.push(PathBuf::from("/opt/homebrew/lib"));

    roots.push(PathBuf::from(NODE_SCRIPT_RELATIVE));

    let _ = cfg;
    roots
}

/// Build the argv for one turn. Pure and unit-tested.
///
/// Verified shape (`knowledge/codex.md`):
/// ```text
/// codex exec --cd <CWD> --approve-for-me --skip-git-repo-check --json
///     [--model <ID>] <PROMPT>
/// ```
pub fn build_args(cfg: &Config, req: &ChatRequest) -> Result<Vec<OsString>> {
    let _ = cfg;
    let mut args: Vec<OsString> = Vec::with_capacity(12);

    args.push(OsString::from("exec"));

    // Resume is a subcommand: `codex exec resume <id>`.
    // NOTE(M2): confirm the argument order against `codex exec --help` before
    // relying on it; resume is exercised in M5, not by the M0-M2 gates.
    if let Some(id) = &req.resume {
        args.push(OsString::from("resume"));
        args.push(OsString::from(id));
    }

    args.push(OsString::from("--cd"));
    args.push(req.cwd.clone().into_os_string());
    // Auto-approve within the workspace-write sandbox. Mutually exclusive with
    // `--sandbox`, so that flag must never be added alongside.
    args.push(OsString::from("--approve-for-me"));
    // pie runs in arbitrary workspaces, including non-repos.
    args.push(OsString::from("--skip-git-repo-check"));
    args.push(OsString::from("--json"));

    if let Some(model) = &req.model {
        args.push(OsString::from("--model"));
        args.push(OsString::from(model));
    }

    args.push(OsString::from(&req.prompt));
    Ok(args)
}

/// Build a runnable plan.
pub fn build_plan(cfg: &Config, req: &ChatRequest) -> Result<TurnPlan> {
    let (program, prefix) = program_and_prefix(cfg).ok_or_else(|| PieError::BinaryMissing {
        backend: "codex".into(),
        path: cfg.codex.bin.clone(),
    })?;

    let mut args = prefix;
    args.extend(build_args(cfg, req)?);

    // M0 passes the prompt on argv, so the config argv cap applies even though
    // codex can read stdin. M2 lifts this by piping oversized prompts.
    let cap = cli::cap_for(&Codex.capabilities(), cfg);
    cli::enforce_argv_cap(
        "codex",
        &program,
        &args,
        cap,
        "M0 caps argv; M2 adds a stdin path for oversized prompts",
    )?;

    Ok(TurnPlan {
        program,
        args,
        cwd: req.cwd.clone(),
        scrub: cfg.scrub_keys(cfg.scrub_enabled()),
        timeout: req.timeout.unwrap_or_else(|| cfg.timeout()),
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn req() -> ChatRequest {
        let mut r = ChatRequest::new("hi");
        r.cwd = PathBuf::from("C:/work");
        r
    }

    fn strings(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|a| a.to_string_lossy().to_string())
            .collect()
    }

    #[test]
    fn argv_matches_the_verified_contract() {
        let a = strings(&build_args(&Config::default(), &req()).unwrap());
        assert_eq!(a[0], "exec");
        assert_eq!(a[1], "--cd");
        assert_eq!(a[2], "C:/work");
        assert!(a.contains(&"--approve-for-me".to_string()));
        assert!(a.contains(&"--skip-git-repo-check".to_string()));
        assert!(a.contains(&"--json".to_string()));
        assert_eq!(a.last().unwrap(), "hi", "prompt is last");
    }

    #[test]
    fn sandbox_is_never_combined_with_approve_for_me() {
        // They are mutually exclusive in codex.
        let a = strings(&build_args(&Config::default(), &req()).unwrap());
        assert!(!a.contains(&"--sandbox".to_string()));
        assert!(a.contains(&"--approve-for-me".to_string()));
    }

    #[test]
    fn model_is_passed_only_when_chosen() {
        let mut r = req();
        let a = strings(&build_args(&Config::default(), &r).unwrap());
        assert!(!a.contains(&"--model".to_string()));

        r.model = Some("gpt-6-luna".into());
        let a = strings(&build_args(&Config::default(), &r).unwrap());
        let i = a.iter().position(|x| x == "--model").unwrap();
        assert_eq!(a[i + 1], "gpt-6-luna");
    }

    #[test]
    fn resume_uses_the_resume_subcommand() {
        let mut r = req();
        r.resume = Some("thread-9".into());
        let a = strings(&build_args(&Config::default(), &r).unwrap());
        assert_eq!(a[0], "exec");
        assert_eq!(a[1], "resume");
        assert_eq!(a[2], "thread-9");
        assert_eq!(a[3], "--cd");
    }

    #[test]
    fn capabilities_declare_stdin_and_no_tool_loop() {
        let caps = Codex.capabilities();
        assert!(caps.stdin_prompt, "codex reads stdin when given no prompt");
        assert!(!caps.tool_loop, "exec is one-shot");
        assert_eq!(caps.max_prompt_argv, 0, "no inherent argv limit");
    }

    #[test]
    fn shell_shims_are_recognised_and_refused() {
        assert!(is_shell_shim(Path::new(
            r"C:\Users\x\AppData\Roaming\npm\codex.cmd"
        )));
        assert!(is_shell_shim(Path::new("codex.bat")));
        assert!(is_shell_shim(Path::new("codex.ps1")));
        assert!(!is_shell_shim(Path::new("codex.exe")));
        assert!(!is_shell_shim(Path::new("codex")));
    }

    #[test]
    fn javascript_paths_are_recognised() {
        assert!(is_javascript(Path::new("codex.js")));
        assert!(is_javascript(Path::new("codex.MJS")));
        assert!(!is_javascript(Path::new("codex.exe")));
        assert!(!is_javascript(Path::new("codex")));
    }

    #[test]
    fn node_root_candidates_include_the_relative_script_and_system_paths() {
        let roots = npm_root_candidates(&Config::default());
        assert!(roots.contains(&PathBuf::from(NODE_SCRIPT_RELATIVE)));
        assert!(roots
            .iter()
            .any(|r| r.to_string_lossy().contains("node_modules")));
    }

    #[test]
    fn oversized_prompt_is_refused_in_m0() {
        let mut r = req();
        r.prompt = "p".repeat(30_000);
        let err = build_plan(&Config::default(), &r).unwrap_err();
        assert!(matches!(err, PieError::PromptTooLong { .. }));
    }

    #[test]
    fn missing_binary_reports_binary_missing() {
        let mut cfg = Config::default();
        cfg.codex.bin = "definitely-not-a-real-codex-binary-xyz".into();
        cfg.codex.use_node_shim = false;
        let err = build_plan(&cfg, &req()).unwrap_err();
        assert!(matches!(err, PieError::BinaryMissing { .. }));
        assert_eq!(err.exit_code(), 6);
    }

    /// Regression: the version probe must include the launcher prefix, or
    /// `doctor` records the launcher's version instead of codex's.
    #[test]
    fn version_probe_carries_the_node_script_prefix() {
        use crate::provider::Provider;

        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("codex.js");
        std::fs::write(&script, "// stub").unwrap();

        let mut cfg = Config::default();
        cfg.codex.bin = script.to_string_lossy().to_string();

        // Only assert when node is actually resolvable; otherwise the probe
        // legitimately yields None and there is nothing to check.
        if let Some((program, args)) = Codex.version_command(&cfg) {
            assert!(is_javascript(&program) || program.to_string_lossy().contains("node"));
            let a = strings(&args);
            assert_eq!(a.last().unwrap(), "--version");
            assert!(
                a.iter().any(|x| x.ends_with("codex.js")),
                "the script must precede --version: {a:?}"
            );
        }
    }
}
