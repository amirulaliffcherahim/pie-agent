//! Configuration: `~/.pie/config.toml`, environment, then flags.
//!
//! Precedence, highest first (`planning/DESIGN.md` §8):
//! CLI flag → `PIE_*` env → `config.toml` → built-in default.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::{PieError, Result};

/// Environment variables that flip a subscription CLI onto pay-as-you-go
/// billing. They are removed from every child process we spawn (`RISKS.md` §10).
pub const BUILTIN_SCRUB: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "OPENAI_API_KEY",
    "GOOGLE_API_KEY",
    // A GCP service-account credential. `knowledge/internals.md` lists it among
    // agy's API-key paths and says plainly not to use those paths, while agy's
    // subscription login is its own internal OAuth - so a service account in the
    // environment is a billing switch rather than a login. Earlier versions
    // preserved it, assuming agy might need it; the documented advice is the
    // opposite, so it is scrubbed. `PIE_STRIP_ENV=0` remains the escape hatch for
    // a setup that genuinely depends on it.
    "GOOGLE_APPLICATION_CREDENTIALS",
];

/// Whole resolved configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Config {
    /// Cross-cutting settings.
    pub general: General,
    /// Google Antigravity CLI backend.
    pub agy: Agy,
    /// OpenAI Codex CLI backend.
    pub codex: Codex,
    /// Anthropic Claude Code CLI backend (future-proof, no sub on this machine).
    pub claude: Claude,
    /// Background-job settings (M7/M8). Opt-in: nothing runs by default.
    pub jobs: Jobs,
    /// The tool suite the loop may offer a model (M11). Opt-in, and off entirely by
    /// default — see [`Tools`].
    pub tools: Tools,
}

/// Settings that are not backend-specific.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct General {
    /// `provider/model`, used when `--model` is omitted.
    pub default_model: Option<String>,
    /// Backend used when neither `--model` nor `default_model` names one (M3).
    ///
    /// Defaults to [`crate::provider::DEFAULT_BACKEND`] (codex, on the measured
    /// ~3.6 s-versus-~40 s grounds in `DESIGN.md` §10). Setting it to an empty
    /// string turns routing off, so the "no model selected" error comes back.
    /// A name that is not a known backend is a config error, never a silent
    /// fallback to a different subscription.
    pub default_backend: Option<String>,
    /// Whole-turn deadline.
    pub timeout_secs: u64,
    /// Hard cap on the argv we build for a CLI with no stdin prompt path.
    pub prompt_argv_cap: usize,
    /// Colour policy for human-readable output.
    pub color: ColorMode,
    /// Extra env vars to scrub, appended to [`BUILTIN_SCRUB`].
    pub extra_scrub_env: Vec<String>,
}

impl Default for General {
    fn default() -> Self {
        Self {
            default_model: None,
            default_backend: Some(crate::provider::DEFAULT_BACKEND.to_string()),
            timeout_secs: 600,
            prompt_argv_cap: 20_000,
            color: ColorMode::Auto,
            extra_scrub_env: Vec::new(),
        }
    }
}

/// When to emit ANSI colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ColorMode {
    /// Colour only when stderr is a terminal and `NO_COLOR` is unset.
    Auto,
    /// Always colour.
    Always,
    /// Never colour.
    Never,
}

/// Google Antigravity CLI settings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Agy {
    /// Binary name or path.
    pub bin: String,
    /// Injected when a model is selected without an explicit effort —
    /// `agy --model` is rejected without `--effort`.
    pub default_effort: Option<String>,
    /// Passed as `--print-timeout <n>s`; `0` waits for completion.
    pub print_timeout_secs: u64,
    /// How long a cached `agy models` catalog stays fresh.
    pub model_cache_ttl_secs: u64,
    /// How long a cached `/usage` quota reading stays fresh. `0` disables the
    /// cache and probes every time.
    ///
    /// The probe is free (zero model tokens) but not cheap in *time*: agy spends
    /// roughly ten seconds of its own initialisation on it, measured 2026-10-12.
    /// Quota moves slowly, so five minutes is a reasonable default, and the
    /// report says when it was taken so a cached reading is never mistaken for a
    /// live one.
    pub usage_cache_ttl_secs: u64,
}

impl Default for Agy {
    fn default() -> Self {
        Self {
            bin: "agy".to_string(),
            default_effort: Some("medium".to_string()),
            print_timeout_secs: 0,
            model_cache_ttl_secs: 86_400,
            usage_cache_ttl_secs: 300,
        }
    }
}

/// OpenAI Codex CLI settings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Codex {
    /// Binary name or path.
    pub bin: String,
    /// On Windows the `codex.cmd` shim cannot be spawned directly; prefer the
    /// node launcher or the native binary (`knowledge/codex.md`).
    pub use_node_shim: bool,
    /// How long a cached model catalog stays fresh.
    pub model_cache_ttl_secs: u64,
    /// How `pie usage codex` obtains quota buckets.
    pub quota_probe: CodexQuotaProbe,
}

impl Default for Codex {
    fn default() -> Self {
        Self {
            bin: "codex".to_string(),
            use_node_shim: true,
            model_cache_ttl_secs: 86_400,
            quota_probe: CodexQuotaProbe::default(),
        }
    }
}

/// Strategy for the codex quota probe.
///
/// `codex` has no CLI subcommand that reports quota, so `PLAN.md` §6 specifies
/// the ChatGPT usage endpoint using the credential from `~/.codex/auth.json`.
/// That is the closest thing in pie to calling a vendor endpoint directly, which
/// is the pattern `PLAN.md` §5 and `knowledge/9router-findings.md` warn about.
///
/// **`off` is the default** (user decision, 2026-10-12, reversing the initial
/// reading of `PLAN.md` §6). The capability is kept and remains one config line
/// away, but no private vendor endpoint is reached unless the user asks for it:
/// 9router's own authors mark that endpoint class
/// `deprecated: true, deprecationNotice: "RISK_NOTICE"` — *"Account may be
/// restricted or banned"* — and an account risk nobody opted into is not a
/// reasonable thing for a default to spend. The endpoint is also read-only,
/// using the user's own credential, which is why it is a switch rather than a
/// removal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CodexQuotaProbe {
    /// Never call it. **The default.** `pie usage codex` still reports pie's
    /// session-log totals; only the quota buckets are unavailable.
    #[default]
    Off,
    /// Query the ChatGPT usage endpoint. Opt-in, per `PLAN.md` §6.
    Http,
}

/// How often a background job may run, as configured.
///
/// Deliberately its own enum rather than reusing [`crate::schedule::Cadence`]. The two
/// answer different questions: this one is a *serialisation* surface (`config.toml`
/// spells it); that one is a *decision* surface (`due()` takes it). Keeping them apart
/// means a config-file typo cannot change what `due()` means, and
/// [`JobCadence::to_schedule`] is an exhaustive match, so a cadence cannot be added to
/// one enum and forgotten in the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JobCadence {
    /// **The default.** Nothing is ever due, so nothing runs and nothing is spent.
    #[default]
    Never,
    /// Once a day.
    Daily,
    /// Once a month.
    Monthly,
}

impl JobCadence {
    /// The scheduling decision this configuration means.
    pub fn to_schedule(self) -> crate::schedule::Cadence {
        match self {
            JobCadence::Never => crate::schedule::Cadence::Never,
            JobCadence::Daily => crate::schedule::Cadence::Daily,
            JobCadence::Monthly => crate::schedule::Cadence::Monthly,
        }
    }
}

/// Background-job settings (M7/M8).
///
/// `ROADMAP.md`'s cross-cutting list requires background jobs to be **budgeted** and
/// **opt-in**, and `RISKS.md` §13 is the reason: an unbudgeted summarisation job can
/// silently spend premium subscription quota. Both are properties of these defaults
/// rather than of a comment, which is what makes them assertable:
///
/// * `learn_cadence = "never"` — a job does not run because it was installed.
/// * `learn_budget_tokens = 0` — **no backend pie drives is cheap.** Every one is
///   subscription-locked, so PLAN §11b's "default to a cheap/local model" has nothing
///   to name, and the honest default is to make **no model call at all**. M7's
///   extraction is rule-based for exactly this reason; a model-based consolidation is
///   an opt-in extra, and a budget of `0` makes `check_budget` refuse *before* anything
///   is spawned.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Jobs {
    /// How often the learn job may run.
    pub learn_cadence: JobCadence,
    /// Token budget for one **window** — a single cadence interval, not one run.
    ///
    /// Per-window rather than per-run on purpose, and this is the maintainer's ruling on
    /// an ambiguity stage C reported: the ledger records what a window spent, so a second
    /// run — including an explicit `pie schedule run` — spends the *remainder*
    /// (`ACCEPTANCE.md` §15). A per-run budget would make the ceiling a function of how
    /// many times the user typed a command. `0` means it may not spend at all.
    pub learn_budget_tokens: u64,
    /// Whether the learn job may read the vendor CLIs' own history stores.
    ///
    /// Off by default, read-only when on (`RISKS.md` §15). pie's own sessions are
    /// sufficient for M7; reading `~/.gemini`, `~/.codex` or `~/.claude` is a decision
    /// the user makes deliberately, not a side effect of enabling the job.
    pub read_vendor_history: bool,
    /// News feeds the monthly watchtower may fetch (M8).
    ///
    /// **Empty by default, and that is the opt-in.** With no source configured the
    /// watchtower fetches nothing at all, so a fresh install makes no network request
    /// and the milestone's "nothing runs unasked" claim is a property of the default
    /// rather than a hope about the code path. `PLAN.md` §11a calls the sources
    /// "configured" and this is where they are configured.
    ///
    /// A bare URL is enough: the report's display name comes from the host, because
    /// requiring a name as well would be two keys to express one source.
    pub news_sources: Vec<String>,
    /// Terms the news digest ranks headlines against (M8).
    ///
    /// Empty is a stated rule of `watch::rank` rather than a special case: with nothing
    /// to match, every item keeps `impact == 0` and the feed's own order is preserved,
    /// because a digest sorted by nothing is one the reader cannot interpret.
    ///
    /// It lives here rather than in `profile.toml` because `Profile` is deliberately
    /// opaque — it exposes a projection, not fields — and inventing a persona key to
    /// feed a ranking would couple the persona to one consumer.
    pub watch_stack: Vec<String>,
}

/// The tool suite the loop may offer a model (M11, `DESIGN.md` §20).
///
/// **The default is no tools at all**, which is exactly the behaviour before M11 — so
/// upgrading cannot turn an existing setup into one that executes commands. That is not a
/// cosmetic default: an unbudgeted `bash` is a worse version of the risk `RISKS.md` §13
/// names for an unbudgeted model call, and it must not arrive as a side effect of an
/// upgrade.
///
/// The shape follows from one constraint rather than from taste. M9's loop runs between
/// the model and pie with **no interactive channel**: `agy --print` and `codex exec` are
/// one-shot, claude's SDK mode streams, and the TUI is not the only entry point. There is
/// nowhere to ask "may I run this command?" mid-turn, so a tool design that assumed an
/// approval prompt would work in one front-end and silently block in the others.
/// Therefore a tool is **enabled in config with a declared scope, or it is not offered**.
/// The decision is made once, by a human who is not under time pressure, instead of
/// repeatedly by a loop that has no human in it.
///
/// Note what that rules out, because it is the tempting alternative: no per-call
/// confirmation, no "ask the user", and **no timeout-then-proceed**. A tool that became
/// available because a prompt went unanswered is a tool nobody approved.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Tools {
    /// The ceiling for every tool: no path a tool resolves may leave it.
    ///
    /// **Absent means no tool is offered at all**, and that is the single enabling
    /// decision. Without a root there is nothing to scope a read to, and the refusal a
    /// model receives names this key.
    ///
    /// A ceiling rather than a default, and checked on the **resolved** path: `..` and a
    /// symlink are the two ways a textual prefix check is wrong, so neither can be the
    /// check (`ACCEPTANCE.md` §19).
    pub workspace: Option<PathBuf>,
    /// The read-only tier: `read`, `ls`, `grep`, `find`.
    ///
    /// Defaults to `true` because `DESIGN.md` §20's tier table says the tier is
    /// *"enabled once a workspace root is configured"* — configuring
    /// [`workspace`](Self::workspace) is the decision, and this key exists so a setup that
    /// wants a workspace for a later tier can decline the read tier explicitly. With no
    /// workspace it is inert.
    pub read: bool,
    /// `write` — declared by `DESIGN.md` §20's tier table, **not built by M11**.
    ///
    /// `ACCEPTANCE.md` §19 freezes three stages (read-only, `bash`, the spawn
    /// unification) and gives this tier no criterion, so a call is answered with a stated
    /// refusal naming this key rather than with an unknown tool. See the S-item recorded
    /// against §19: it must not become a key that merely *looks* like it does something.
    pub write: bool,
    /// `edit` — declared by `DESIGN.md` §20's tier table, **not built by M11**. See
    /// [`write`](Self::write).
    pub edit: bool,
    /// `bash`. Turning this on **is** the decision, so it is opt-in twice over.
    pub bash: bool,
    /// `argv[0]` allow-list for `bash`, checked **before** the spawn.
    ///
    /// An allow-list on the *executable* rather than on the whole command string is
    /// deliberate: pattern-matching a shell line is the known-unreliable approach, and
    /// claiming safety from it would be worse than admitting the boundary is "this
    /// executable". A refused command is therefore a stated error rather than a
    /// `PermissionDenied` a model has to interpret.
    ///
    /// Empty means `bash` refuses everything, which is the honest reading of "no command
    /// is allowed" — not "all commands are allowed".
    pub bash_allow: Vec<String>,
    /// Deadline for one `bash` command, in seconds. A command that outlives it is
    /// **killed**, not waited on (`RISKS.md` §6).
    pub timeout_secs: u64,
    /// Cap on the output one tool call may return.
    ///
    /// A command that prints forever must not fill memory — the reason `read_body` is
    /// capped, and `B22`'s lesson applied to a new input.
    pub max_output_bytes: usize,
    /// Environment variables this tier's children must not see.
    ///
    /// **Not a config key** (`#[serde(skip)]`), because there is exactly one place the list may
    /// come from: `Config::scrub_keys`, which composes [`BUILTIN_SCRUB`] with the user's
    /// `general.extra_scrub_env`. A `tools.scrub` key would be a second list, free to disagree
    /// with the one every vendor child gets.
    ///
    /// The default carries [`BUILTIN_SCRUB`] rather than being empty, so a `Tools` built by hand
    /// is scrubbed by construction. An empty default would make "forgot to compose the scrub"
    /// indistinguishable from "nothing to scrub", which is the failure this field exists to
    /// correct.
    ///
    /// **This tier scrubs unconditionally, and the asymmetry is deliberate.** `PIE_STRIP_ENV=0`
    /// is a debugging escape hatch for a *vendor* CLI, where the risk is a subscription quietly
    /// switching to pay-as-you-go billing. Here the risk is different in kind: the command is
    /// chosen by a model, and an unscrubbed child can simply be asked to print its environment.
    /// So the tool path composes with `scrub_keys(true)` and never consults `scrub_enabled()`.
    #[serde(skip)]
    pub scrub: Vec<String>,
}

impl Default for Tools {
    fn default() -> Self {
        Self {
            workspace: None,
            read: true,
            write: false,
            edit: false,
            bash: false,
            bash_allow: Vec::new(),
            timeout_secs: 30,
            max_output_bytes: 1 << 20,
            // The builtin deny-list, so a hand-built `Tools` is safe by construction; the
            // user's extras are added by `Config::resolve`.
            scrub: BUILTIN_SCRUB.iter().map(|key| (*key).to_string()).collect(),
        }
    }
}

/// Anthropic Claude Code CLI settings. Disabled: no subscription on this machine.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Claude {
    /// Binary name or path.
    pub bin: String,
    /// Adapter is implemented to spec but off by default.
    pub enabled: bool,
}

impl Default for Claude {
    fn default() -> Self {
        Self {
            bin: "claude".to_string(),
            enabled: false,
        }
    }
}

/// Values supplied on the command line, applied last.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Overrides {
    /// `--model`
    pub default_model: Option<String>,
    /// `--timeout`
    pub timeout_secs: Option<u64>,
}

impl Config {
    /// Resolve configuration from the default location plus env plus flags.
    pub fn resolve(overrides: &Overrides) -> Result<Self> {
        let mut cfg = Self::load_from(&crate::paths::config_file())?;
        cfg.apply_env()?;
        cfg.apply(overrides);
        // Composed last, and with `true`, so the tool tier gets the **whole** list:
        // `BUILTIN_SCRUB` plus whatever `general.extra_scrub_env` contributes. `scrub_enabled()`
        // is deliberately not consulted - see `Tools::scrub`.
        //
        // Sound rather than merely convenient: `extra_scrub_env` has no environment override and
        // is written only by `toml::from_str`, so a config carrying extras necessarily came
        // through here. A `Tools` built by hand has no extras to lose.
        let scrub = cfg.scrub_keys(true);
        cfg.tools.scrub = scrub;
        Ok(cfg)
    }

    /// Load from a specific file. A missing file is not an error.
    pub fn load_from(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text)
                .map_err(|e| PieError::Config(format!("{}: {e}", path.display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(PieError::Config(format!("{}: {e}", path.display()))),
        }
    }

    /// Apply `PIE_*` environment overrides.
    pub fn apply_env(&mut self) -> Result<()> {
        if let Some(v) = env_non_empty("PIE_DEFAULT_MODEL") {
            self.general.default_model = Some(v);
        }
        // Deliberately not `env_non_empty`: a *set but empty* variable has to be
        // honoured, because that is how a user turns routing off and gets the
        // "no model selected" error back. Ignoring it would silently keep
        // spending a backend the user just opted out of.
        if let Ok(raw) = std::env::var("PIE_DEFAULT_BACKEND") {
            let name = raw.trim();
            self.general.default_backend = (!name.is_empty()).then(|| name.to_string());
        }
        if let Some(v) = env_non_empty("PIE_TIMEOUT_SECS") {
            self.general.timeout_secs = parse_num(&v, "PIE_TIMEOUT_SECS")?;
        }
        if let Some(v) = env_non_empty("PIE_PROMPT_ARGV_CAP") {
            self.general.prompt_argv_cap = parse_num(&v, "PIE_PROMPT_ARGV_CAP")?;
        }
        if let Some(v) = env_non_empty("PIE_COLOR") {
            self.general.color = match v.to_ascii_lowercase().as_str() {
                "auto" => ColorMode::Auto,
                "always" => ColorMode::Always,
                "never" => ColorMode::Never,
                other => {
                    return Err(PieError::Config(format!(
                        "PIE_COLOR must be auto|always|never, got `{other}`"
                    )))
                }
            };
        }
        if let Some(v) = env_non_empty("PIE_AGY_BIN") {
            self.agy.bin = v;
        }
        if let Some(v) = env_non_empty("PIE_CODEX_BIN") {
            self.codex.bin = v;
        }
        if let Some(v) = env_non_empty("PIE_CLAUDE_BIN") {
            self.claude.bin = v;
        }
        if let Some(v) = env_non_empty("PIE_CODEX_QUOTA_PROBE") {
            self.codex.quota_probe = match v.to_ascii_lowercase().as_str() {
                "http" => CodexQuotaProbe::Http,
                "off" => CodexQuotaProbe::Off,
                other => {
                    return Err(PieError::Config(format!(
                        "PIE_CODEX_QUOTA_PROBE must be http|off, got `{other}`"
                    )))
                }
            };
        }
        if let Some(v) = env_non_empty("PIE_JOBS_LEARN_CADENCE") {
            self.jobs.learn_cadence = match v.to_ascii_lowercase().as_str() {
                "never" => JobCadence::Never,
                "daily" => JobCadence::Daily,
                "monthly" => JobCadence::Monthly,
                other => {
                    return Err(PieError::Config(format!(
                        "PIE_JOBS_LEARN_CADENCE must be never|daily|monthly, got `{other}`"
                    )))
                }
            };
        }
        if let Some(v) = env_non_empty("PIE_JOBS_LEARN_BUDGET_TOKENS") {
            self.jobs.learn_budget_tokens = parse_num(&v, "PIE_JOBS_LEARN_BUDGET_TOKENS")?;
        }
        // Deliberately accepts only an explicit truthy value rather than anything
        // non-empty: this switch is what lets a job read the user's private CLI
        // history, so `PIE_JOBS_READ_VENDOR_HISTORY=0` must mean off, not "set, so on".
        if let Some(v) = env_non_empty("PIE_JOBS_READ_VENDOR_HISTORY") {
            self.jobs.read_vendor_history = match v.to_ascii_lowercase().as_str() {
                "1" | "true" | "yes" | "on" => true,
                "0" | "false" | "no" | "off" => false,
                other => {
                    return Err(PieError::Config(format!(
                        "PIE_JOBS_READ_VENDOR_HISTORY must be a boolean, got `{other}`"
                    )))
                }
            };
        }
        // Comma-separated, and an empty value is a *deliberate* opt-out rather than a
        // no-op — the same reasoning `PIE_DEFAULT_BACKEND` gets: a user turning the
        // watchtower's fetching off must be able to, and `env_non_empty` would silently
        // keep the configured list.
        if let Ok(raw) = std::env::var("PIE_JOBS_NEWS_SOURCES") {
            self.jobs.news_sources = split_list(&raw);
        }
        if let Ok(raw) = std::env::var("PIE_JOBS_WATCH_STACK") {
            self.jobs.watch_stack = split_list(&raw);
        }
        Ok(())
    }

    /// Apply command-line overrides (highest precedence).
    pub fn apply(&mut self, o: &Overrides) {
        if let Some(m) = &o.default_model {
            self.general.default_model = Some(m.clone());
        }
        if let Some(t) = o.timeout_secs {
            self.general.timeout_secs = t;
        }
    }

    /// Whole-turn deadline.
    pub fn timeout(&self) -> Duration {
        Duration::from_secs(self.general.timeout_secs.max(1))
    }

    /// Env vars to strip from every child process.
    ///
    /// The deny-list survives even when `extra_scrub_env` is set, and
    /// `PIE_STRIP_ENV=0` disables the whole mechanism for debugging only.
    pub fn scrub_keys(&self, enabled: bool) -> Vec<String> {
        if !enabled {
            return Vec::new();
        }
        let mut keys: Vec<String> = BUILTIN_SCRUB.iter().map(|s| (*s).to_string()).collect();
        for k in &self.general.extra_scrub_env {
            let k = k.trim();
            if !k.is_empty() && !keys.iter().any(|e| e.eq_ignore_ascii_case(k)) {
                keys.push(k.to_string());
            }
        }
        keys
    }

    /// Whether API-key scrubbing is active. `PIE_STRIP_ENV=0` turns it off.
    pub fn scrub_enabled(&self) -> bool {
        !matches!(
            env_non_empty("PIE_STRIP_ENV").as_deref(),
            Some("0") | Some("false")
        )
    }

    /// Effective argv cap for a backend that must pass the prompt on argv.
    pub fn effective_argv_cap(&self, caps: &crate::provider::Capabilities) -> usize {
        match caps.max_prompt_argv {
            0 => self.general.prompt_argv_cap,
            other => other.min(self.general.prompt_argv_cap),
        }
    }
}

/// Whether ANSI colour should be used for stderr diagnostics.
///
/// Called before the config file is necessarily readable, so it reads only env
/// plus terminal detection.
pub fn color_enabled() -> bool {
    if env_non_empty("NO_COLOR").is_some() {
        return false;
    }
    match env_non_empty("PIE_COLOR").as_deref() {
        Some("always") => return true,
        Some("never") => return false,
        _ => {}
    }
    if env_non_empty("CLICOLOR_FORCE").is_some() {
        return true;
    }
    use std::io::IsTerminal;
    std::io::stderr().is_terminal()
}

fn env_non_empty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

fn parse_num<T: std::str::FromStr>(raw: &str, key: &str) -> Result<T> {
    raw.trim()
        .parse::<T>()
        .map_err(|_| PieError::Config(format!("{key} must be a number, got `{raw}`")))
}

/// Split a comma-separated environment value into a list.
///
/// Blank entries are dropped so a trailing comma does not become a source with an empty
/// URL, and each entry is trimmed because a list typed into a shell profile collects
/// spaces after the commas. An **empty** value yields an empty list rather than keeping a
/// previously configured one: for both callers, an empty list is the documented opt-out
/// and "set but empty" has to mean off.
fn split_list(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    /// Serialises the env-mutating tests in this module.
    /// (Rust runs tests in threads; `PIE_*` vars are process-global.)
    use crate::testing::EnvGuard;

    #[test]
    fn defaults_are_documented_values() {
        let g = General::default();
        assert_eq!(g.timeout_secs, 600);
        assert_eq!(g.prompt_argv_cap, 20_000);
        assert_eq!(g.color, ColorMode::Auto);
        assert_eq!(
            g.default_backend.as_deref(),
            Some(crate::provider::DEFAULT_BACKEND),
            "M3 routes to codex by default (DESIGN §10)"
        );
        assert!(g.default_model.is_none(), "no model is invented by default");
        assert_eq!(Agy::default().default_effort.as_deref(), Some("medium"));
        assert_eq!(Agy::default().print_timeout_secs, 0);
        assert!(!Claude::default().enabled, "claude stays off by default");
        assert_eq!(
            Codex::default().quota_probe,
            CodexQuotaProbe::Off,
            "the private wham/usage endpoint is opt-in, not a default (user decision \
             2026-10-12)"
        );
    }

    /// The private vendor endpoint is reachable, but only when the user asks for
    /// it, in config or in the environment.
    ///
    /// This is the regression guard for the decision that reversed the original
    /// default: what matters is not merely that `off` exists, but that the
    /// **default** is `off` — a switch that defaults to on is not an opt-in.
    #[test]
    fn the_private_codex_endpoint_is_opt_in_not_a_default() {
        assert_eq!(
            Config::default().codex.quota_probe,
            CodexQuotaProbe::Off,
            "a fresh install must never call a private vendor endpoint"
        );

        // Opting in works from the file…
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[codex]\nquota_probe = \"http\"\n").unwrap();

        let cfg = Config::load_from(&path).unwrap();
        assert_eq!(cfg.codex.quota_probe, CodexQuotaProbe::Http);
        // unrelated keys keep their defaults
        assert!(cfg.codex.use_node_shim);

        // …and from the environment, case-insensitively. Each `EnvGuard` is
        // scoped: it is not re-entrant, and taking a second one while the first
        // is alive deadlocks the whole test binary (`src/testing.rs`).
        {
            let _env = EnvGuard::set(&[("PIE_CODEX_QUOTA_PROBE", Some("HTTP"))]);
            let mut cfg = Config::default();
            cfg.apply_env().unwrap();
            assert_eq!(cfg.codex.quota_probe, CodexQuotaProbe::Http);
        }

        // `off` in the environment is still honoured, so the switch works in
        // both directions rather than only in the one the default now picks.
        {
            let _env = EnvGuard::set(&[("PIE_CODEX_QUOTA_PROBE", Some("OFF"))]);
            let mut cfg = Config::default();
            cfg.apply_env().unwrap();
            assert_eq!(cfg.codex.quota_probe, CodexQuotaProbe::Off);
        }
    }

    #[test]
    fn an_unrecognised_quota_probe_is_rejected_not_guessed() {
        let _env = EnvGuard::set(&[("PIE_CODEX_QUOTA_PROBE", Some("maybe"))]);
        let mut cfg = Config::default();
        let err = cfg.apply_env().unwrap_err();
        assert!(err.to_string().contains("PIE_CODEX_QUOTA_PROBE"));
    }

    /// M3 routing: the backend can be chosen from the file or the environment,
    /// and an empty value is a *deliberate* opt-out rather than a no-op.
    #[test]
    fn the_default_backend_is_configurable_and_can_be_turned_off() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[general]\ndefault_backend = \"agy\"\n").unwrap();
        assert_eq!(
            Config::load_from(&path).unwrap().general.default_backend,
            Some("agy".to_string())
        );

        let _env = EnvGuard::set(&[("PIE_DEFAULT_BACKEND", Some("agy"))]);
        let mut cfg = Config::default();
        cfg.apply_env().unwrap();
        assert_eq!(cfg.general.default_backend.as_deref(), Some("agy"));
    }

    #[test]
    fn an_empty_default_backend_means_no_default_at_all() {
        let _env = EnvGuard::set(&[("PIE_DEFAULT_BACKEND", Some("  "))]);
        let mut cfg = Config::default();
        cfg.apply_env().unwrap();
        assert_eq!(
            cfg.general.default_backend, None,
            "a set-but-empty variable must be honoured, not ignored"
        );

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[general]\ndefault_backend = \"\"\n").unwrap();
        assert_eq!(
            Config::load_from(&path).unwrap().general.default_backend,
            Some(String::new()),
            "read as unset by resolve_target, which is where it is consumed"
        );
    }

    #[test]
    fn missing_file_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::load_from(&dir.path().join("nope.toml")).unwrap();
        assert_eq!(cfg, Config::default());
    }

    #[test]
    fn malformed_toml_is_a_config_error_with_location() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(&p, "this is not = = toml").unwrap();
        let err = Config::load_from(&p).unwrap_err();
        assert!(matches!(err, PieError::Config(_)));
        assert_eq!(err.exit_code(), 2);
        assert!(err.to_string().contains("config.toml"));
    }

    #[test]
    fn partial_file_keeps_other_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(&p, "[general]\ntimeout_secs = 42\n").unwrap();
        let cfg = Config::load_from(&p).unwrap();
        assert_eq!(cfg.general.timeout_secs, 42);
        assert_eq!(cfg.general.prompt_argv_cap, 20_000, "untouched default");
        assert_eq!(cfg.agy, Agy::default(), "untouched section");
    }

    #[test]
    fn precedence_flag_beats_env_beats_file_beats_default() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(
            &p,
            "[general]\ndefault_model = \"file/m\"\ntimeout_secs = 11\n",
        )
        .unwrap();

        let _env = EnvGuard::set(&[("PIE_DEFAULT_MODEL", Some("env/m"))]);

        let mut cfg = Config::load_from(&p).unwrap();
        // file wins over default
        assert_eq!(cfg.general.default_model.as_deref(), Some("file/m"));
        cfg.apply_env().unwrap();
        // env wins over file
        assert_eq!(cfg.general.default_model.as_deref(), Some("env/m"));
        cfg.apply(&Overrides {
            default_model: Some("flag/m".into()),
            timeout_secs: Some(99),
        });
        // flag wins over env
        assert_eq!(cfg.general.default_model.as_deref(), Some("flag/m"));
        assert_eq!(cfg.general.timeout_secs, 99);
    }

    #[test]
    fn bad_env_number_is_rejected() {
        let _env = EnvGuard::set(&[("PIE_TIMEOUT_SECS", Some("soon"))]);
        let mut cfg = Config::default();
        let err = cfg.apply_env().unwrap_err();
        assert!(err.to_string().contains("PIE_TIMEOUT_SECS"));
    }

    #[test]
    fn bad_color_env_is_rejected() {
        let _env = EnvGuard::set(&[("PIE_COLOR", Some("chartreuse"))]);
        let mut cfg = Config::default();
        assert!(cfg.apply_env().is_err());
    }

    #[test]
    fn bin_overrides_come_from_env() {
        let _env = EnvGuard::set(&[
            ("PIE_AGY_BIN", Some("/opt/agy")),
            ("PIE_CODEX_BIN", Some("/opt/codex")),
            ("PIE_CLAUDE_BIN", Some("/opt/claude")),
        ]);
        let mut cfg = Config::default();
        cfg.apply_env().unwrap();
        assert_eq!(cfg.agy.bin, "/opt/agy");
        assert_eq!(cfg.codex.bin, "/opt/codex");
        assert_eq!(cfg.claude.bin, "/opt/claude");
    }

    #[test]
    fn a_hand_built_tools_carries_the_builtin_scrub_list() {
        // `Tools::default()` is what tests and any non-`resolve` path get. If `scrub` defaulted
        // to empty, "forgot to compose the scrub" would be indistinguishable from "nothing to
        // scrub" - which is the failure S21 was.
        let tools = Tools::default();
        for key in BUILTIN_SCRUB {
            assert!(
                tools.scrub.iter().any(|candidate| candidate == key),
                "`{key}` is missing from Tools::default().scrub: {:?}",
                tools.scrub
            );
        }
    }

    #[test]
    fn the_scrub_list_is_not_settable_from_a_tools_table() {
        // One place composes the list (`Config::scrub_keys`). A `tools.scrub` key would be a
        // second list, free to disagree with the one every vendor child gets.
        let cfg: Config = toml::from_str("[tools]\nworkspace = \"/tmp\"\nscrub = [\"MY_TOKEN\"]\n")
            .expect("the tools table parses");
        assert_eq!(cfg.tools.workspace.as_deref(), Some(Path::new("/tmp")));
        assert!(
            !cfg.tools.scrub.iter().any(|key| key == "MY_TOKEN"),
            "`tools.scrub` must not be deserializable; got {:?}",
            cfg.tools.scrub
        );
    }

    #[test]
    fn resolve_gives_the_tool_tier_the_users_extra_keys_and_ignores_the_escape_hatch() {
        // The real wiring, not the rule applied by hand: `Config::resolve` is the only production
        // path to a config carrying `extra_scrub_env`, so this is where the composition has to
        // hold. A scratch `PIE_HOME` keeps the developer's own config out of it.
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join("config.toml"),
            "[general]\nextra_scrub_env = [\"MY_CORP_TOKEN\"]\n",
        )
        .unwrap();
        let home_text = home.path().to_string_lossy().into_owned();
        let _env = EnvGuard::set(&[
            ("PIE_HOME", Some(home_text.as_str())),
            // The debugging escape hatch for vendor children, which the tool tier must ignore.
            ("PIE_STRIP_ENV", Some("0")),
        ]);

        let cfg = Config::resolve(&Overrides::default()).unwrap();
        for key in BUILTIN_SCRUB {
            assert!(
                cfg.tools.scrub.iter().any(|candidate| candidate == key),
                "`{key}` is missing from the tool tier's scrub list: {:?}",
                cfg.tools.scrub
            );
        }
        assert!(
            cfg.tools.scrub.iter().any(|key| key == "MY_CORP_TOKEN"),
            "the user's own deny-list key did not reach the tool tier (S21): {:?}",
            cfg.tools.scrub
        );
        // And the escape hatch still works where it is meant to: for a *vendor* child, whose
        // risk is a billing switch rather than a model asking a command to print the environment.
        assert!(
            !cfg.scrub_enabled(),
            "this test is only meaningful with PIE_STRIP_ENV=0 honoured"
        );
        assert!(
            cfg.scrub_keys(cfg.scrub_enabled()).is_empty(),
            "PIE_STRIP_ENV=0 must still disable scrubbing for vendor children"
        );
    }

    #[test]
    fn scrub_list_contains_the_billing_flippers() {
        let cfg = Config::default();
        let keys = cfg.scrub_keys(true);
        for k in BUILTIN_SCRUB {
            assert!(keys.contains(&k.to_string()), "{k} must be scrubbed");
        }
        // Deliberately NOT scrubbed: agy's own auth path may need them.
        // `CLOUD_CODE` is a mode selector, not a credential, so it is kept.
        assert!(!keys.contains(&"CLOUD_CODE".to_string()));
        assert!(cfg.scrub_keys(false).is_empty(), "PIE_STRIP_ENV=0 disables");
    }

    #[test]
    fn extra_scrub_env_is_merged_and_deduped() {
        let mut cfg = Config::default();
        cfg.general.extra_scrub_env = vec![
            "MY_KEY".into(),
            "openai_api_key".into(), // case-insensitive dupe of a builtin
            "   ".into(),            // ignored
        ];
        let keys = cfg.scrub_keys(true);
        assert!(keys.contains(&"MY_KEY".to_string()));
        assert_eq!(
            keys.iter()
                .filter(|k| k.eq_ignore_ascii_case("OPENAI_API_KEY"))
                .count(),
            1
        );
        assert!(!keys.iter().any(|k| k.trim().is_empty()));
    }

    #[test]
    fn argv_cap_takes_the_stricter_of_config_and_capability() {
        use crate::provider::Capabilities;
        let mut cfg = Config::default();
        cfg.general.prompt_argv_cap = 20_000;

        let agy_like = Capabilities {
            max_prompt_argv: 20_000,
            ..Capabilities::default()
        };
        assert_eq!(cfg.effective_argv_cap(&agy_like), 20_000);

        cfg.general.prompt_argv_cap = 5_000;
        assert_eq!(
            cfg.effective_argv_cap(&agy_like),
            5_000,
            "config is stricter"
        );

        let stdin_capable = Capabilities::default();
        assert_eq!(
            cfg.effective_argv_cap(&stdin_capable),
            5_000,
            "0 means unlimited from the capability side, so config applies"
        );
    }

    /// The cross-cutting requirement, as a property of the defaults rather than a note.
    ///
    /// `ROADMAP.md` requires background jobs to be budgeted and opt-in, and
    /// `RISKS.md` §13 says why. Asserting the *defaults* is the point: a job that runs
    /// because it was installed, or that has a budget it can spend, fails these even if
    /// every option is individually settable.
    #[test]
    fn background_jobs_are_off_and_unbudgeted_until_asked_for() {
        let jobs = Config::default().jobs;
        assert_eq!(
            jobs.learn_cadence,
            JobCadence::Never,
            "a job must not run because it happens to be installed"
        );
        assert_eq!(
            jobs.learn_budget_tokens, 0,
            "the default path must make no model call: no backend pie drives is cheap"
        );
        assert!(
            !jobs.read_vendor_history,
            "reading the user's private CLI history is opt-in (RISKS section 15)"
        );

        // …and a zero budget really does refuse, which ties the default to the
        // schedule module's gate rather than leaving "budgeted" as a comment.
        assert!(
            crate::schedule::check_budget(&crate::schedule::Budget::new(0), 1).is_err(),
            "a zero budget must refuse a one-token job, not merely allow none in theory"
        );
    }

    /// The two cadence enums must not drift: a cadence added to one and forgotten in
    /// the other would make `config.toml` spell something `due()` cannot mean. The
    /// mapping is an exhaustive match, so the compiler enforces totality; this asserts
    /// the mapping is *meaningful*, which totality alone does not.
    #[test]
    fn the_configured_cadence_means_what_the_scheduler_thinks_it_means() {
        assert_eq!(
            JobCadence::Never.to_schedule(),
            crate::schedule::Cadence::Never
        );
        assert_eq!(
            JobCadence::Daily.to_schedule(),
            crate::schedule::Cadence::Daily
        );
        assert_eq!(
            JobCadence::Monthly.to_schedule(),
            crate::schedule::Cadence::Monthly
        );

        // A cadence that mapped to `Never` would silently disable the job it names.
        assert!(
            JobCadence::Daily.to_schedule().interval_ms().is_some(),
            "`daily` must map to something the scheduler can schedule"
        );
        assert!(
            JobCadence::Monthly.to_schedule().interval_ms().is_some(),
            "`monthly` must map to something the scheduler can schedule"
        );
    }

    /// Each job setting can be turned on from the environment, and each is validated
    /// rather than guessed.
    #[test]
    fn the_job_settings_read_from_the_environment() {
        // Guards are scoped: `EnvGuard` is not re-entrant and panics if taken twice on
        // one thread (bug B24).
        {
            let _env = EnvGuard::set(&[
                ("PIE_JOBS_LEARN_CADENCE", Some("DAILY")),
                ("PIE_JOBS_LEARN_BUDGET_TOKENS", Some("42000")),
                ("PIE_JOBS_READ_VENDOR_HISTORY", Some("true")),
                (
                    "PIE_JOBS_NEWS_SOURCES",
                    Some("https://a.example/rss, https://b.example/feed"),
                ),
                ("PIE_JOBS_WATCH_STACK", Some("rust, codex")),
            ]);
            let mut cfg = Config::default();
            cfg.apply_env().unwrap();
            assert_eq!(cfg.jobs.learn_cadence, JobCadence::Daily);
            assert_eq!(cfg.jobs.learn_budget_tokens, 42_000);
            assert!(cfg.jobs.read_vendor_history);
            assert_eq!(
                cfg.jobs.news_sources,
                vec!["https://a.example/rss", "https://b.example/feed"],
                "the list is split and each entry trimmed"
            );
            assert_eq!(cfg.jobs.watch_stack, vec!["rust", "codex"]);
        }

        // An explicit false must mean off rather than "set, therefore on": this switch
        // is what exposes the user's private CLI history.
        {
            let _env = EnvGuard::set(&[("PIE_JOBS_READ_VENDOR_HISTORY", Some("0"))]);
            let mut cfg = Config::default();
            cfg.jobs.read_vendor_history = true;
            cfg.apply_env().unwrap();
            assert!(!cfg.jobs.read_vendor_history, "`0` must turn it off");
        }

        // **Set but empty must mean off, not "keep what was configured".** This is the
        // switch that makes the watchtower fetch nothing, so a user turning it off has to
        // be able to, and a trailing comma must not become a source with an empty URL.
        {
            let _env = EnvGuard::set(&[
                ("PIE_JOBS_NEWS_SOURCES", Some("")),
                ("PIE_JOBS_WATCH_STACK", Some("rust,")),
            ]);
            let mut cfg = Config::default();
            cfg.jobs.news_sources = vec!["https://was-configured.example/rss".into()];
            cfg.apply_env().unwrap();
            assert!(
                cfg.jobs.news_sources.is_empty(),
                "an empty value must clear the list, not keep it: {:?}",
                cfg.jobs.news_sources
            );
            assert_eq!(
                cfg.jobs.watch_stack,
                vec!["rust"],
                "a trailing comma is dropped"
            );
        }

        for (value, blamed) in [
            ("sometimes", "PIE_JOBS_LEARN_CADENCE"),
            ("maybe", "PIE_JOBS_READ_VENDOR_HISTORY"),
        ] {
            let _env = EnvGuard::set(&[(blamed, Some(value))]);
            let mut cfg = Config::default();
            let err = cfg.apply_env().unwrap_err();
            assert!(
                err.to_string().contains(blamed),
                "a bad `{blamed}` must name itself, got: {err}"
            );
        }
    }
}
