//! Credential probes.
//!
//! pie never holds a token. It asks each vendor CLI's *own* store whether a
//! **subscription** credential exists, and refuses to run when the only thing
//! present is a pay-as-you-go API key (`RISKS.md` §10).
//!
//! Forbidden, permanently: reading or writing `~/.pie/auth.json`, the legacy
//! file from the earlier JS implementation that holds extracted codex OAuth
//! tokens. See [`crate::paths::legacy_auth_file`].

use std::path::PathBuf;

use crate::paths;

/// `~/.codex/`
pub fn codex_dir() -> PathBuf {
    paths::home_dir().unwrap_or_default().join(".codex")
}

/// `~/.codex/auth.json` — codex's own credential store.
pub fn codex_auth_file() -> PathBuf {
    codex_dir().join("auth.json")
}

/// `~/.gemini/` — agy's own state root.
pub fn agy_dir() -> PathBuf {
    paths::home_dir().unwrap_or_default().join(".gemini")
}

/// Claude's config dir, honouring `ANTHROPIC_CONFIG_DIR`.
pub fn claude_dir() -> PathBuf {
    std::env::var("ANTHROPIC_CONFIG_DIR")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| paths::home_dir().unwrap_or_default().join(".claude"))
}

/// `~/.claude/.credentials.json`
pub fn claude_credentials_file() -> PathBuf {
    claude_dir().join(".credentials.json")
}

/// What kind of credential a backend has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Credential {
    /// A subscription (email-locked) credential — the only acceptable kind.
    Subscription {
        /// How we determined it, for the `doctor` table.
        evidence: String,
    },
    /// Only a pay-as-you-go API key was found. Refuse.
    ApiKeyOnly {
        /// What we saw.
        evidence: String,
    },
    /// Nothing usable.
    Absent,
    /// The store exists but we could not classify it.
    Unreadable(String),
}

impl Credential {
    /// True only for [`Credential::Subscription`].
    pub fn is_subscription(&self) -> bool {
        matches!(self, Credential::Subscription { .. })
    }

    /// Short label for tables.
    pub fn label(&self) -> &'static str {
        match self {
            Credential::Subscription { .. } => "subscription",
            Credential::ApiKeyOnly { .. } => "api-key-only",
            Credential::Absent => "absent",
            Credential::Unreadable(_) => "unreadable",
        }
    }
}

/// Classify a codex `auth_mode` string.
///
/// Tolerant by design: codex has used both `chatgpt`/`apikey` and
/// `subscription`/`api_key` shapes. An unknown value is classified as
/// unreadable rather than assumed safe.
pub fn classify_codex_mode(mode: &str) -> Credential {
    let m = mode.trim().to_ascii_lowercase();
    if m.is_empty() {
        return Credential::Unreadable("auth_mode is empty".to_string());
    }
    if m.contains("subscript") || m.contains("chatgpt") || m.contains("account") {
        return Credential::Subscription {
            evidence: format!("auth_mode={m}"),
        };
    }
    if m.contains("api") {
        return Credential::ApiKeyOnly {
            evidence: format!("auth_mode={m}"),
        };
    }
    Credential::Unreadable(format!("unrecognised auth_mode `{m}`"))
}

/// Inspect codex's credential store.
///
/// Reads `auth_mode` and the *presence* of a token. Never reads, retains, or
/// returns a token value.
pub fn codex_credential() -> Credential {
    let path = codex_auth_file();
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Credential::Absent,
        Err(e) => return Credential::Unreadable(format!("{}: {e}", path.display())),
    };

    let value: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => return Credential::Unreadable(format!("{}: {e}", path.display())),
    };

    let mode = crate::jsonl::s(&value, "auth_mode").unwrap_or_default();
    let classified = classify_codex_mode(&mode);
    if classified != Credential::Unreadable(format!("unrecognised auth_mode `{mode}`")) {
        return classified;
    }

    // Fall back to the strongest structural evidence: a stored access token
    // means the CLI is logged into an account. We check presence only.
    let has_token = crate::jsonl::s_nonempty(&value, "tokens.access_token").is_some();
    if has_token {
        return Credential::Subscription {
            evidence: "tokens.access_token present".to_string(),
        };
    }

    if classified_is_unreadable(&classified) {
        classified
    } else {
        Credential::Absent
    }
}

fn classified_is_unreadable(c: &Credential) -> bool {
    matches!(c, Credential::Unreadable(_))
}

/// Inspect claude's credential store.
pub fn claude_credential() -> Credential {
    let path = claude_credentials_file();
    if !path.is_file() {
        // `~/.claude.json` is machine state, not credentials — explicitly not
        // treated as a login (`knowledge/internals.md`).
        return Credential::Absent;
    }
    match std::fs::metadata(&path) {
        Ok(m) if m.len() > 0 => Credential::Subscription {
            evidence: "credentials.json present".to_string(),
        },
        Ok(_) => Credential::Unreadable("credentials.json is empty".to_string()),
        Err(e) => Credential::Unreadable(format!("{}: {e}", path.display())),
    }
}

/// Whether agy appears to be logged in, judged from its own state directory.
///
/// agy stores auth internally with no token file to read
/// (`knowledge/agy.md`), so this is a presence heuristic. The authoritative
/// check is a live `agy --print`.
pub fn agy_credential() -> Credential {
    let dir = agy_dir();
    if !dir.is_dir() {
        return Credential::Absent;
    }
    let config = dir.join("config").join("config.json");
    if config.is_file() {
        return Credential::Subscription {
            evidence: "~/.gemini/config/config.json present".to_string(),
        };
    }
    Credential::Unreadable("~/.gemini exists but has no config/config.json".to_string())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::testing::EnvGuard;

    #[test]
    fn codex_modes_are_classified_by_meaning_not_spelling() {
        for s in ["subscription", "Subscription", "chatgpt", "ChatGPT-Account"] {
            assert!(
                classify_codex_mode(s).is_subscription(),
                "{s} should be a subscription"
            );
        }
        for s in ["api_key", "apikey", "API-key"] {
            assert!(
                matches!(classify_codex_mode(s), Credential::ApiKeyOnly { .. }),
                "{s} should be api-key-only"
            );
        }
        for s in ["", "   ", "mystery"] {
            assert!(
                matches!(classify_codex_mode(s), Credential::Unreadable(_)),
                "{s:?} must not be assumed safe"
            );
        }
    }

    #[test]
    fn subscription_is_never_derived_from_an_api_key() {
        // The dangerous inverse: a credential that is *only* an API key must
        // never classify as a subscription.
        assert!(!classify_codex_mode("api_key").is_subscription());
    }

    #[test]
    fn missing_codex_store_is_absent_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_string_lossy().to_string();
        let _env = EnvGuard::set(&[("HOME", Some(&home)), ("USERPROFILE", Some(&home))]);
        assert_eq!(codex_credential(), Credential::Absent);
        assert_eq!(claude_credential(), Credential::Absent);
        assert_eq!(agy_credential(), Credential::Absent);
    }

    #[test]
    fn a_token_value_never_appears_in_the_classification() {
        let dir = tempfile::tempdir().unwrap();
        let codex = dir.path().join(".codex");
        std::fs::create_dir_all(&codex).unwrap();
        let secret = "sk-this-must-never-be-echoed-0123456789";
        std::fs::write(
            codex.join("auth.json"),
            format!(r#"{{"auth_mode":"mystery","tokens":{{"access_token":"{secret}"}}}}"#),
        )
        .unwrap();

        let home = dir.path().to_string_lossy().to_string();
        let _env = EnvGuard::set(&[("HOME", Some(&home)), ("USERPROFILE", Some(&home))]);

        let cred = codex_credential();
        let rendered = format!("{cred:?} {} ", cred.label());
        assert!(
            !rendered.contains(secret),
            "credential debug output leaked the token: {rendered}"
        );
        // Unknown auth_mode + a token present => subscription by structural evidence.
        assert!(cred.is_subscription());
    }

    #[test]
    fn api_key_only_auth_mode_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let codex = dir.path().join(".codex");
        std::fs::create_dir_all(&codex).unwrap();
        std::fs::write(
            codex.join("auth.json"),
            r#"{"auth_mode":"apikey","tokens":{"access_token":"short"}}"#,
        )
        .unwrap();

        let home = dir.path().to_string_lossy().to_string();
        let _env = EnvGuard::set(&[("HOME", Some(&home)), ("USERPROFILE", Some(&home))]);

        let cred = codex_credential();
        assert!(
            matches!(cred, Credential::ApiKeyOnly { .. }),
            "api-key mode must never be treated as a subscription: {cred:?}"
        );
        assert!(!cred.is_subscription());
    }

    #[test]
    fn claude_config_dir_honours_the_env_override() {
        let _env = EnvGuard::set(&[("ANTHROPIC_CONFIG_DIR", Some("/custom/claude"))]);
        assert_eq!(claude_dir(), PathBuf::from("/custom/claude"));
        assert_eq!(
            claude_credentials_file(),
            PathBuf::from("/custom/claude").join(".credentials.json")
        );
    }
}
