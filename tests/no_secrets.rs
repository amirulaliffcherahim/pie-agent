//! Guard tests for the "no hardcoded keys, nothing persisted" rule.
//!
//! Two independent checks, because they fail in different ways:
//!
//! 1. **Static** — no credential-shaped literal may exist in the tree. Catches a
//!    key pasted into source during development, which is the failure mode that
//!    survives review.
//! 2. **Behavioural** — run the real binary with API keys present in the
//!    environment, then assert the values appear in no output and in no file pie
//!    writes. Catches the opposite mistake: reading a key and echoing or
//!    caching it.
//!
//! `PLAN.md` §5 and `RISKS.md` §10 are the requirements these enforce.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::process::Command;

/// A value distinctive enough that finding it anywhere else proves a leak.
const PROBE: &str = "pie-compliance-probe-A1b2C3d4E5f6G7h8";

/// Credential prefixes that are never legitimate in this codebase.
const FORBIDDEN_PREFIXES: &[(&str, &str)] = &[
    ("AIza", "Google API key"),
    ("sk-ant-", "Anthropic API key"),
    ("sk-proj-", "OpenAI project key"),
    ("ghp_", "GitHub personal access token"),
    ("gho_", "GitHub OAuth token"),
    ("xoxb-", "Slack bot token"),
    ("-----BEGIN", "PEM-encoded private key"),
];

/// Environment variables whose *value* must never be assigned in source.
const SCRUBBED_KEYS: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "OPENAI_API_KEY",
    "GOOGLE_API_KEY",
];

/// Synthetic samples that exist so redaction can be *tested*. None is a
/// credential; each is listed with its purpose.
///
/// `PROBE` is included because this file legitimately declares it.
const ALLOWED_SAMPLES: &[&str] = &[
    // Public, well-known JWT *header* for HS256 — used as a redaction probe.
    "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9",
    // Proves a stored token never reaches debug output (auth.rs).
    "sk-this-must-never-be-echoed-0123456789",
    // This file's own leak probe.
    PROBE,
];

/// The guard scans everything except itself: it necessarily contains the
/// forbidden-prefix table and the probe. The trade-off is that a key could hide
/// here — so this file stays small, and the behavioural checks below cover it.
const SELF: &str = "tests/no_secrets.rs";

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn relative(path: &Path) -> String {
    path.strip_prefix(repo_root())
        .unwrap_or(path)
        .display()
        .to_string()
        .replace('\\', "/")
}

fn rust_sources(dir: &Path, into: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_sources(&path, into);
        } else if path.extension().is_some_and(|e| e == "rs") && relative(&path) != SELF {
            into.push(path);
        }
    }
}

fn all_sources() -> Vec<PathBuf> {
    let root = repo_root();
    let mut found = Vec::new();
    for sub in ["src", "tests"] {
        rust_sources(&root.join(sub), &mut found);
    }
    assert!(
        found.len() > 5,
        "only {} sources found — the scan is broken",
        found.len()
    );
    found
}

/// Contents of every double-quoted string literal on a line.
fn quoted_literals(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = line.chars().peekable();

    while let Some(c) = chars.next() {
        if c != '"' {
            continue;
        }
        let mut literal = String::new();
        while let Some(next) = chars.next() {
            match next {
                '\\' => {
                    // Keep escapes verbatim; they only ever shorten a literal.
                    literal.push('\\');
                    if let Some(escaped) = chars.next() {
                        literal.push(escaped);
                    }
                }
                '"' => break,
                other => literal.push(other),
            }
        }
        out.push(literal);
    }

    out
}

/// Heuristic for "this literal looks like a key rather than prose".
///
/// A real credential is a long unbroken run containing a digit, an uppercase
/// letter and a lowercase letter. Prose, paths and test phrases are not.
fn looks_like_key_material(literal: &str) -> bool {
    if literal.chars().count() < 32 {
        return false;
    }
    let opaque = |c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.';
    if !literal.chars().all(|c| opaque(c) || c == '/') {
        return false;
    }
    literal.contains(|c: char| c.is_ascii_digit())
        && literal.contains(|c: char| c.is_ascii_uppercase())
        && literal.contains(|c: char| c.is_ascii_lowercase())
}

#[test]
fn no_credential_shaped_literal_exists_in_the_tree() {
    let mut findings = Vec::new();

    for path in all_sources() {
        let text = std::fs::read_to_string(&path).unwrap();

        for (index, line) in text.lines().enumerate() {
            let number = index + 1;

            for literal in quoted_literals(line) {
                if ALLOWED_SAMPLES.iter().any(|s| literal.contains(s)) {
                    continue;
                }

                // Case 1: a known credential prefix inside a real literal.
                // The length guard keeps the redaction prefix table in
                // `process.rs` (`"sk-"`, `"ghp_"`, `"eyJ"`) from tripping this.
                if literal.chars().count() > 8 {
                    if let Some((needle, what)) =
                        FORBIDDEN_PREFIXES.iter().find(|(n, _)| literal.contains(n))
                    {
                        findings.push(format!(
                            "{file}:{line}: `{needle}` ({what}) inside a literal",
                            file = relative(&path),
                            line = number
                        ));
                        continue;
                    }
                }

                // Case 2: an opaque literal that looks like key material.
                if looks_like_key_material(&literal) {
                    findings.push(format!(
                        "{file}:{line}: literal looks like key material — {}",
                        &literal[..literal.len().min(60)],
                        file = relative(&path)
                    ));
                }
            }
        }
    }

    assert!(
        findings.is_empty(),
        "credential-shaped literals found:\n  {}",
        findings.join("\n  ")
    );
}

#[test]
fn api_key_variables_are_never_assigned_a_value_in_source() {
    let mut findings = Vec::new();

    for path in all_sources() {
        let text = std::fs::read_to_string(&path).unwrap();

        for (index, line) in text.lines().enumerate() {
            for key in SCRUBBED_KEYS {
                let Some(at) = line.find(key) else { continue };

                // `KEY = "…"` or `KEY: "…"` would be a hardcoded value.
                let rest = line[at + key.len()..].trim_start();
                let is_assignment = rest.starts_with('=') || rest.starts_with(":=");
                if is_assignment && rest.contains('"') {
                    findings.push(format!(
                        "{}:{}: {key} is assigned a literal",
                        relative(&path),
                        index + 1
                    ));
                }
            }
        }
    }

    assert!(
        findings.is_empty(),
        "hardcoded credentials found:\n  {}",
        findings.join("\n  ")
    );
}

fn files_under(dir: &Path, into: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            files_under(&path, into);
        } else {
            into.push(path);
        }
    }
}

/// Run the real binary with API keys set, and assert nothing captures them.
fn assert_keys_never_surface(args: &[&str]) {
    let home = tempfile::tempdir().unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_pie"))
        .args(args)
        // A throwaway state dir, so we can also prove nothing was written there.
        .env("PIE_HOME", home.path())
        .env("ANTHROPIC_API_KEY", PROBE)
        .env("ANTHROPIC_AUTH_TOKEN", PROBE)
        .env("OPENAI_API_KEY", PROBE)
        .env("GOOGLE_API_KEY", PROBE)
        .output()
        .expect("failed to run the pie binary");

    let invoked = args.join(" ");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !stdout.contains(PROBE),
        "`pie {invoked}` echoed an API key on stdout:\n{stdout}"
    );
    assert!(
        !stderr.contains(PROBE),
        "`pie {invoked}` echoed an API key on stderr:\n{stderr}"
    );

    let mut written = Vec::new();
    files_under(home.path(), &mut written);
    for file in written {
        let bytes = std::fs::read(&file).unwrap_or_default();
        assert!(
            !String::from_utf8_lossy(&bytes).contains(PROBE),
            "`pie {invoked}` wrote an API key into {}",
            file.display()
        );
    }
}

#[test]
fn auth_never_surfaces_or_persists_an_api_key() {
    assert_keys_never_surface(&["auth"]);
}

#[test]
fn doctor_never_surfaces_or_persists_an_api_key() {
    assert_keys_never_surface(&["doctor"]);
}

#[test]
fn usage_never_surfaces_or_persists_an_api_key() {
    assert_keys_never_surface(&["usage"]);
}

/// `~/.pie/auth.json` from the earlier JS implementation holds extracted OAuth
/// tokens. pie must leave it strictly alone: not read it, not rewrite it.
#[test]
fn the_legacy_auth_file_is_left_untouched() {
    let home = tempfile::tempdir().unwrap();
    let legacy = home.path().join("auth.json");
    let original = r#"{"providers":{"codex":{"access_token":"legacy-do-not-touch"}}}"#;
    std::fs::write(&legacy, original).unwrap();

    for args in [["auth"], ["usage"], ["doctor"]] {
        let _ = Command::new(env!("CARGO_BIN_EXE_pie"))
            .args(args)
            .env("PIE_HOME", home.path())
            .output()
            .expect("failed to run the pie binary");
    }

    let after = std::fs::read_to_string(&legacy).unwrap();
    assert_eq!(after, original, "pie modified the legacy auth.json");
}
