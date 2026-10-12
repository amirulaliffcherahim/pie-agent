//! Error taxonomy and process exit codes.
//!
//! Every failure a user can hit has a variant with a hint, and every variant
//! maps to a stable exit code so scripts and the QC gates can assert on it
//! (`planning/ACCEPTANCE.md` §1).

use std::path::PathBuf;

/// Result alias used across the crate.
pub type Result<T> = std::result::Result<T, PieError>;

/// Everything that can go wrong, with the backend named where relevant.
#[derive(Debug, thiserror::Error)]
pub enum PieError {
    /// Bad or unreadable configuration (bad TOML, contradictory flags).
    #[error("config: {0}")]
    Config(String),

    /// `--model` named a provider that does not exist.
    #[error("unknown backend `{backend}` (known: {})", .known.join(", "))]
    UnknownBackend {
        /// The offending provider id.
        backend: String,
        /// Provider ids that do exist.
        known: Vec<String>,
    },

    /// A `--model` spec was not `provider/model`.
    #[error("invalid model spec `{spec}` — expected `provider/model`, e.g. agy/gemini-3.8-flash")]
    BadModelSpec {
        /// The offending spec.
        spec: String,
    },

    /// The vendor CLI is not on PATH / at the configured location.
    #[error("`{backend}` binary not found at {path}")]
    BinaryMissing {
        /// Provider id.
        backend: String,
        /// Path we looked at.
        path: String,
    },

    /// The vendor CLI is installed but not logged in.
    #[error("`{backend}` needs login: {hint}")]
    AuthRequired {
        /// Provider id.
        backend: String,
        /// What the user should run.
        hint: String,
    },

    /// A credential exists, but it is a pay-as-you-go API key. We refuse.
    #[error(
        "`{backend}` has no subscription credential — refusing to run in API-key mode \
         (would silently switch billing). {hint}"
    )]
    NoSubscription {
        /// Provider id.
        backend: String,
        /// How to fix it.
        hint: String,
    },

    /// The process could not be started.
    #[error("failed to spawn `{backend}`: {source}")]
    Spawn {
        /// Provider id.
        backend: String,
        /// Underlying OS error.
        source: std::io::Error,
    },

    /// The turn exceeded its deadline; the process tree was killed.
    #[error("`{backend}` timed out after {secs}s (process tree killed)")]
    Timeout {
        /// Provider id.
        backend: String,
        /// Deadline that fired.
        secs: u64,
    },

    /// The stream did not look like anything we can parse.
    #[error("`{backend}` protocol error: {detail}")]
    Protocol {
        /// Provider id.
        backend: String,
        /// What we saw, truncated.
        detail: String,
    },

    /// The prompt does not fit in the child's argv and the CLI has no stdin path.
    #[error("prompt too long for `{backend}`: {len} chars > {max} char argv cap. {hint}")]
    PromptTooLong {
        /// Provider id.
        backend: String,
        /// Actual length.
        len: usize,
        /// Configured cap.
        max: usize,
        /// How to work around it.
        hint: String,
    },

    /// The CLI ran, but reported failure.
    #[error("`{backend}` failed ({status}): {detail}")]
    Backend {
        /// Provider id.
        backend: String,
        /// Status string from the terminal event, or the exit status.
        status: String,
        /// Bounded diagnostic text.
        detail: String,
    },

    /// A feature that exists in the trait but is not built yet.
    #[error("`{backend}` does not support {feature} yet")]
    Unsupported {
        /// Provider id.
        backend: String,
        /// Feature name.
        feature: String,
    },

    /// Something on disk is missing where we need it.
    #[error("missing expected file: {0}")]
    MissingFile(PathBuf),

    /// A path could not be reached at all.
    #[error("not reachable: {0}")]
    NotReachable(String),

    /// I/O failure.
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// JSON failure.
    #[error(transparent)]
    Json(#[from] serde_json::Error),

    /// HTTP failure (quota probe only; requires the `http` feature).
    #[cfg(feature = "http")]
    #[error(transparent)]
    Http(#[from] reqwest::Error),
}

impl PieError {
    /// Stable process exit code for this failure.
    pub fn exit_code(&self) -> i32 {
        match self {
            // usage / configuration problems
            PieError::Config(_)
            | PieError::UnknownBackend { .. }
            | PieError::BadModelSpec { .. }
            | PieError::PromptTooLong { .. } => 2,
            // auth
            PieError::AuthRequired { .. } | PieError::NoSubscription { .. } => 3,
            // protocol
            PieError::Protocol { .. } => 4,
            PieError::Timeout { .. } => 5,
            PieError::BinaryMissing { .. } => 6,
            // everything else: generic backend failure
            _ => 1,
        }
    }

    /// Convenience constructor for `Config` from anything printable.
    pub fn config(msg: impl Into<String>) -> Self {
        PieError::Config(msg.into())
    }

    /// Convenience constructor for `Protocol`.
    pub fn protocol(backend: impl Into<String>, detail: impl Into<String>) -> Self {
        PieError::Protocol {
            backend: backend.into(),
            detail: truncate(&detail.into(), 600),
        }
    }

    /// Stable machine-readable class, used as `error_kind` in the session log.
    pub fn kind(&self) -> &'static str {
        match self {
            PieError::Config(_) => "config",
            PieError::UnknownBackend { .. } => "unknown_backend",
            PieError::BadModelSpec { .. } => "bad_model_spec",
            PieError::BinaryMissing { .. } => "binary_missing",
            PieError::AuthRequired { .. } => "auth_required",
            PieError::NoSubscription { .. } => "no_subscription",
            PieError::Spawn { .. } => "spawn",
            PieError::Timeout { .. } => "timeout",
            PieError::Protocol { .. } => "protocol",
            PieError::PromptTooLong { .. } => "prompt_too_long",
            PieError::Backend { .. } => "backend",
            PieError::Unsupported { .. } => "unsupported",
            PieError::MissingFile(_) => "missing_file",
            PieError::NotReachable(_) => "not_reachable",
            PieError::Io(_) => "io",
            PieError::Json(_) => "json",
            #[cfg(feature = "http")]
            PieError::Http(_) => "http",
        }
    }
}

/// Truncate to `max` chars on a char boundary, marking the cut.
pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push_str("…[truncated]");
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn exit_codes_are_stable() {
        assert_eq!(PieError::config("x").exit_code(), 2);
        assert_eq!(
            PieError::PromptTooLong {
                backend: "agy".into(),
                len: 10,
                max: 5,
                hint: "x".into()
            }
            .exit_code(),
            2
        );
        assert_eq!(
            PieError::AuthRequired {
                backend: "codex".into(),
                hint: "codex login".into()
            }
            .exit_code(),
            3
        );
        assert_eq!(
            PieError::NoSubscription {
                backend: "claude".into(),
                hint: "log in".into()
            }
            .exit_code(),
            3
        );
        assert_eq!(PieError::protocol("agy", "junk").exit_code(), 4);
        assert_eq!(
            PieError::Timeout {
                backend: "agy".into(),
                secs: 5
            }
            .exit_code(),
            5
        );
        assert_eq!(
            PieError::BinaryMissing {
                backend: "agy".into(),
                path: "agy".into()
            }
            .exit_code(),
            6
        );
    }

    #[test]
    fn truncate_is_char_safe() {
        assert_eq!(truncate("abc", 5), "abc");
        // multi-byte input must not panic
        let out = truncate("日本語テキスト", 3);
        assert!(out.starts_with("日本語"));
        assert!(out.ends_with("…[truncated]"));
    }
}
