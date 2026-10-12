//! Filesystem layout.
//!
//! Everything pie owns lives under `PIE_HOME`, defaulting to `~/.pie`. Nothing
//! here is cached in a static, so tests can point `PIE_HOME` at a temp dir.

use std::path::PathBuf;

/// The structured persona file, relative to `PIE_HOME` (M3).
pub const PROFILE_FILE: &str = "profile.toml";

/// The freeform rules file, relative to `PIE_HOME` (M3).
pub const AGENTS_FILE: &str = "AGENTS.md";

/// The durable-memory directory, relative to `PIE_HOME` (M3).
pub const MEMORY_DIR: &str = "memory";

/// The session-store directory, relative to `PIE_HOME` (M5).
pub const SESSIONS_DIR: &str = "sessions";

/// Root of pie's state directory. Honours `PIE_HOME`, else `~/.pie`.
pub fn pie_home() -> PathBuf {
    if let Some(v) = non_empty_env("PIE_HOME") {
        return PathBuf::from(v);
    }
    match home_dir() {
        Some(h) => h.join(".pie"),
        None => PathBuf::from(".pie"),
    }
}

/// The user's home directory, Windows or Unix.
pub fn home_dir() -> Option<PathBuf> {
    non_empty_env("HOME")
        .or_else(|| non_empty_env("USERPROFILE"))
        .map(PathBuf::from)
}

/// `$PIE_HOME/config.toml`
pub fn config_file() -> PathBuf {
    pie_home().join("config.toml")
}

/// `$PIE_HOME/cache/` — model catalogs and version snapshots.
pub fn cache_dir() -> PathBuf {
    pie_home().join("cache")
}

/// `$PIE_HOME/usage.jsonl` — append-only session log (frozen schema).
pub fn usage_log() -> PathBuf {
    pie_home().join("usage.jsonl")
}

/// `$PIE_HOME/cache/agy-models.json`
pub fn agy_model_cache() -> PathBuf {
    cache_dir().join("agy-models.json")
}

/// `$PIE_HOME/cache/agy-usage.json`
///
/// The quota probe costs agy about ten seconds of its own initialisation for a
/// free, zero-token answer, and quota moves slowly. Caching it is the single
/// largest latency win available to pie on the agy path (measured 2026-10-12:
/// 10.3 s per uncached probe).
pub fn agy_usage_cache() -> PathBuf {
    cache_dir().join("agy-usage.json")
}

/// `$PIE_HOME/cache/codex-models.json`
pub fn codex_model_cache() -> PathBuf {
    cache_dir().join("codex-models.json")
}

/// `$PIE_HOME/cache/versions.json` — CLI versions for drift detection.
pub fn version_snapshot() -> PathBuf {
    cache_dir().join("versions.json")
}

/// `$PIE_HOME/reports/` — watchtower output (M8).
pub fn reports_dir() -> PathBuf {
    pie_home().join("reports")
}

/// `$PIE_HOME/profile.toml` — structured persona (M3).
pub fn profile_file() -> PathBuf {
    pie_home().join(PROFILE_FILE)
}

/// `$PIE_HOME/AGENTS.md` — freeform rules, appended after the persona (M3).
pub fn agents_file() -> PathBuf {
    pie_home().join(AGENTS_FILE)
}

/// `$PIE_HOME/memory/` — durable memory (M3).
pub fn memory_dir() -> PathBuf {
    pie_home().join(MEMORY_DIR)
}

/// `$PIE_HOME/sessions/` — transcripts and their index (M5).
///
/// Beside `usage.jsonl`, never inside it: that log is frozen and shared with the
/// earlier JS implementation (`DESIGN.md` §11).
pub fn sessions_dir() -> PathBuf {
    pie_home().join(SESSIONS_DIR)
}

/// The legacy `~/.pie/auth.json` written by the earlier JS implementation.
///
/// pie deliberately never reads or writes this: it holds extracted OAuth
/// tokens, which is the pattern `PLAN.md` §5 forbids. Exposed only so tests and
/// `doctor` can report that we leave it alone.
pub fn legacy_auth_file() -> PathBuf {
    pie_home().join("auth.json")
}

fn non_empty_env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::testing::EnvGuard;

    #[test]
    fn pie_home_honours_override() {
        let _env = EnvGuard::set(&[("PIE_HOME", Some("/tmp/pie-test-home"))]);
        assert_eq!(pie_home(), PathBuf::from("/tmp/pie-test-home"));
        assert_eq!(
            config_file(),
            PathBuf::from("/tmp/pie-test-home").join("config.toml")
        );
        assert_eq!(
            usage_log(),
            PathBuf::from("/tmp/pie-test-home").join("usage.jsonl")
        );
        assert_eq!(
            sessions_dir(),
            PathBuf::from("/tmp/pie-test-home").join("sessions")
        );
    }

    /// A whitespace-only override is not a path — fall back to `~/.pie`.
    #[test]
    fn whitespace_only_override_is_ignored() {
        let _env = EnvGuard::set(&[
            ("PIE_HOME", Some("   ")),
            ("HOME", Some("/home/probe")),
            ("USERPROFILE", Some("/home/probe")),
        ]);
        assert_eq!(pie_home(), PathBuf::from("/home/probe").join(".pie"));
    }

    #[test]
    fn home_falls_back_to_userprofile_when_home_is_unset() {
        let _env = EnvGuard::set(&[
            ("HOME", None),
            ("USERPROFILE", Some("/home/win")),
            ("PIE_HOME", None),
        ]);
        assert_eq!(home_dir(), Some(PathBuf::from("/home/win")));
        assert_eq!(pie_home(), PathBuf::from("/home/win").join(".pie"));
    }

    /// M3's three sources, and the names `profile.rs` joins under an arbitrary
    /// root. One source of truth, in the module that owns the layout.
    #[test]
    fn profile_sources_are_named_by_their_layout_constants() {
        let _env = EnvGuard::set(&[("PIE_HOME", Some("/tmp/pie-home"))]);
        let home = PathBuf::from("/tmp/pie-home");
        assert_eq!(profile_file(), home.join("profile.toml"));
        assert_eq!(agents_file(), home.join("AGENTS.md"));
        assert_eq!(memory_dir(), home.join("memory"));
        assert_eq!(PROFILE_FILE, "profile.toml");
        assert_eq!(AGENTS_FILE, "AGENTS.md");
        assert_eq!(MEMORY_DIR, "memory");
    }

    /// M5's store lives beside the frozen log, never inside it.
    #[test]
    fn the_session_store_sits_beside_the_frozen_log() {
        let _env = EnvGuard::set(&[("PIE_HOME", Some("/tmp/pie-home"))]);
        let home = PathBuf::from("/tmp/pie-home");
        assert_eq!(sessions_dir(), home.join("sessions"));
        assert_eq!(SESSIONS_DIR, "sessions");
        assert!(
            !usage_log().starts_with(sessions_dir()),
            "transcripts must not be written into the frozen log's namespace"
        );
    }

    #[test]
    fn legacy_auth_file_is_in_the_state_dir() {
        let _env = EnvGuard::set(&[("PIE_HOME", Some("/tmp/pie-home"))]);
        assert_eq!(
            legacy_auth_file(),
            PathBuf::from("/tmp/pie-home").join("auth.json")
        );
    }
}
