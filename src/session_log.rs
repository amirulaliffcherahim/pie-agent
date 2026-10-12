//! The append-only session log at `~/.pie/usage.jsonl`.
//!
//! The row schema is **frozen** to match the earlier JS implementation on this
//! machine, so M4's totals and the JS pi stay cross-readable
//! (`planning/DESIGN.md` §7). One JSON object per line, append-only, never
//! rewritten.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::provider::Usage;

/// One turn's accounting. Field names and types are frozen.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionLogRow {
    /// Epoch milliseconds.
    pub ts: u64,
    /// Provider id.
    pub provider: String,
    /// Model id.
    pub model: String,
    /// Prompt tokens.
    pub input_tokens: u64,
    /// Prompt tokens served from cache.
    pub cached_input_tokens: u64,
    /// Completion tokens.
    pub output_tokens: u64,
    /// Reasoning/thinking tokens.
    pub reasoning_tokens: u64,
    /// Cost in micro-USD; subscriptions report 0.
    pub cost_usd_micros: u64,
    /// Plan credits consumed, when the backend reports them.
    pub credits_consumed: u64,
    /// Plan credits remaining, when the backend reports them.
    pub credits_remaining: u64,
    /// Wall-clock duration.
    pub duration_ms: u64,
    /// Whether the turn succeeded.
    pub ok: bool,
    /// Error class when `ok` is false.
    #[serde(default)]
    pub error_kind: Option<String>,
    /// Vendor stop/status reason.
    #[serde(default)]
    pub stop_reason: String,
}

impl SessionLogRow {
    /// Build a row from a turn outcome.
    pub fn from_turn(
        provider: impl Into<String>,
        model: impl Into<String>,
        usage: &Usage,
        duration_ms: u64,
        stop_reason: impl Into<String>,
    ) -> Self {
        Self {
            ts: now_millis(),
            provider: provider.into(),
            model: model.into(),
            input_tokens: usage.input_tokens,
            cached_input_tokens: usage.cache_read_tokens,
            output_tokens: usage.output_tokens,
            reasoning_tokens: usage.thinking_tokens,
            cost_usd_micros: 0,
            credits_consumed: 0,
            credits_remaining: 0,
            duration_ms,
            ok: true,
            error_kind: None,
            stop_reason: stop_reason.into(),
        }
    }

    /// Build a failure row: no tokens consumed, but the attempt is recorded.
    pub fn from_error(
        provider: impl Into<String>,
        model: impl Into<String>,
        error_kind: impl Into<String>,
        duration_ms: u64,
    ) -> Self {
        Self {
            ts: now_millis(),
            provider: provider.into(),
            model: model.into(),
            input_tokens: 0,
            cached_input_tokens: 0,
            output_tokens: 0,
            reasoning_tokens: 0,
            cost_usd_micros: 0,
            credits_consumed: 0,
            credits_remaining: 0,
            duration_ms,
            ok: false,
            error_kind: Some(error_kind.into()),
            stop_reason: String::new(),
        }
    }
}

/// Current time in epoch milliseconds.
pub fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Append a row to the default log.
pub fn append(row: &SessionLogRow) -> Result<()> {
    append_to(&crate::paths::usage_log(), row)
}

/// Append a row to `path`, creating the parent directory if needed.
///
/// The caller is responsible for treating a failure as non-fatal: a turn that
/// produced an answer must never be reported as failed because logging broke.
pub fn append_to(path: &Path, row: &SessionLogRow) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }

    let mut line = serde_json::to_string(row)?;
    line.push('\n');

    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    file.write_all(line.as_bytes())?;
    file.flush()?;
    Ok(())
}

/// Append, logging (not propagating) any failure.
pub fn append_best_effort(row: &SessionLogRow) {
    if let Err(e) = append(row) {
        tracing::warn!(error = %e, "could not append to the session log");
    }
}

/// Read every parseable row. Unparseable lines are skipped, never fatal.
pub fn read_all(path: &Path) -> Result<Vec<SessionLogRow>> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };

    let mut rows = Vec::new();
    let mut skipped = 0usize;
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<SessionLogRow>(line) {
            Ok(row) => rows.push(row),
            Err(_) => skipped += 1,
        }
    }
    if skipped > 0 {
        tracing::warn!(skipped, path = %path.display(), "skipped unreadable session-log rows");
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::provider::Usage;

    fn usage() -> Usage {
        Usage {
            input_tokens: 20_704,
            output_tokens: 20,
            thinking_tokens: 19,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            total_tokens: 20_724,
        }
    }

    #[test]
    fn row_schema_matches_the_frozen_shape() {
        let row = SessionLogRow::from_turn("agy", "gemini-3.8-flash", &usage(), 35_900, "SUCCESS");
        let v = serde_json::to_value(&row).unwrap();
        for key in [
            "ts",
            "provider",
            "model",
            "input_tokens",
            "cached_input_tokens",
            "output_tokens",
            "reasoning_tokens",
            "cost_usd_micros",
            "credits_consumed",
            "credits_remaining",
            "duration_ms",
            "ok",
            "error_kind",
            "stop_reason",
        ] {
            assert!(v.get(key).is_some(), "frozen schema is missing `{key}`");
        }
        assert_eq!(v["input_tokens"], 20_704);
        assert_eq!(
            v["reasoning_tokens"], 19,
            "thinking maps to reasoning_tokens"
        );
        assert_eq!(v["ok"], true);
        assert!(v["error_kind"].is_null());
    }

    #[test]
    fn appends_one_line_per_turn_and_reads_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("usage.jsonl");

        append_to(
            &path,
            &SessionLogRow::from_turn("agy", "m", &usage(), 1, "SUCCESS"),
        )
        .unwrap();
        append_to(
            &path,
            &SessionLogRow::from_turn("codex", "m2", &usage(), 2, "SUCCESS"),
        )
        .unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 2, "one line per turn, appended");
        assert!(text.ends_with('\n'));

        let rows = read_all(&path).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].provider, "agy");
        assert_eq!(rows[1].provider, "codex");
    }

    #[test]
    fn appending_never_truncates_earlier_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage.jsonl");
        for i in 0..5 {
            append_to(
                &path,
                &SessionLogRow::from_turn("agy", format!("m{i}"), &usage(), 1, "S"),
            )
            .unwrap();
        }
        let rows = read_all(&path).unwrap();
        assert_eq!(rows.len(), 5);
        assert_eq!(rows[0].model, "m0");
        assert_eq!(rows[4].model, "m4");
    }

    #[test]
    fn unreadable_rows_are_skipped_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage.jsonl");
        let good =
            serde_json::to_string(&SessionLogRow::from_turn("agy", "m", &usage(), 1, "S")).unwrap();
        std::fs::write(&path, format!("not json\n{good}\n{{}}\n")).unwrap();
        let rows = read_all(&path).unwrap();
        assert_eq!(rows.len(), 1, "only the valid row survives");
    }

    #[test]
    fn missing_log_reads_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_all(&dir.path().join("nope.jsonl")).unwrap().is_empty());
    }

    #[test]
    fn error_rows_record_the_attempt_without_tokens() {
        let row = SessionLogRow::from_error("codex", "gpt-6-luna", "timeout", 600_000);
        assert!(!row.ok);
        assert_eq!(row.input_tokens, 0);
        assert_eq!(row.error_kind.as_deref(), Some("timeout"));
        assert_eq!(row.stop_reason, "");
    }

    /// The legacy `~/.pie/auth.json` holds extracted OAuth tokens. Nothing in
    /// this module may read, write, or recreate it (`PLAN.md` §5).
    #[test]
    fn logging_never_touches_the_legacy_auth_file() {
        let dir = tempfile::tempdir().unwrap();
        let auth = dir.path().join("auth.json");
        std::fs::write(
            &auth,
            r#"{"providers":{"codex":{"access_token":"do-not-touch"}}}"#,
        )
        .unwrap();
        let before = std::fs::read_to_string(&auth).unwrap();

        let log = dir.path().join("usage.jsonl");
        append_to(
            &log,
            &SessionLogRow::from_turn("codex", "m", &usage(), 1, "S"),
        )
        .unwrap();
        let _ = read_all(&log).unwrap();

        assert_eq!(
            std::fs::read_to_string(&auth).unwrap(),
            before,
            "auth.json changed"
        );
    }
}
