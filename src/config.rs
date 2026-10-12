//! Configuration: `~/.pie/config.toml`, environment, then flags.
//!
//! Precedence, highest first (`planning/DESIGN.md` §8):
//! CLI flag → `PIE_*` env → `config.toml` → built-in default.

use std::path::Path;
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
}

/// Settings that are not backend-specific.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct General {
    /// `provider/model`, used when `--model` is omitted.
    pub default_model: Option<String>,
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
}

impl Default for Agy {
    fn default() -> Self {
        Self {
            bin: "agy".to_string(),
            default_effort: Some("medium".to_string()),
            print_timeout_secs: 0,
            model_cache_ttl_secs: 86_400,
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
}

impl Default for Codex {
    fn default() -> Self {
        Self {
            bin: "codex".to_string(),
            use_node_shim: true,
            model_cache_ttl_secs: 86_400,
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    /// Serialises the env-mutating tests in this module.
    /// (Rust runs tests in threads; `PIE_*` vars are process-global.)
    use crate::testenv::EnvGuard;

    #[test]
    fn defaults_are_documented_values() {
        let g = General::default();
        assert_eq!(g.timeout_secs, 600);
        assert_eq!(g.prompt_argv_cap, 20_000);
        assert_eq!(g.color, ColorMode::Auto);
        assert_eq!(Agy::default().default_effort.as_deref(), Some("medium"));
        assert_eq!(Agy::default().print_timeout_secs, 0);
        assert!(!Claude::default().enabled, "claude stays off by default");
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
    fn scrub_list_contains_the_billing_flippers() {
        let cfg = Config::default();
        let keys = cfg.scrub_keys(true);
        for k in BUILTIN_SCRUB {
            assert!(keys.contains(&k.to_string()), "{k} must be scrubbed");
        }
        // Deliberately NOT scrubbed: agy's own auth path may need them.
        assert!(!keys.contains(&"GOOGLE_APPLICATION_CREDENTIALS".to_string()));
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
}
