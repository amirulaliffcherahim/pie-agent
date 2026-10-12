//! Quota reports and session-log totals.
//!
//! `PLAN.md` §6: `pie usage` is a unified table. Two independent sources are
//! shown — the backend's own quota buckets (network, no model tokens) and pie's
//! own session log (already on disk, never re-read from session transcripts).

use serde::{Deserialize, Serialize};

use crate::provider::UsageReport;
use crate::session_log::SessionLogRow;

/// Aggregated session-log totals.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Totals {
    /// Turns attempted.
    pub turns: u64,
    /// Turns that succeeded.
    pub ok_turns: u64,
    /// Turns that failed.
    pub failed_turns: u64,
    /// Prompt tokens.
    pub input_tokens: u64,
    /// Prompt tokens served from cache.
    pub cached_input_tokens: u64,
    /// Completion tokens.
    pub output_tokens: u64,
    /// Reasoning tokens.
    pub reasoning_tokens: u64,
    /// Total wall-clock across turns.
    pub duration_ms: u64,
}

impl Totals {
    /// Every token counted, including reasoning.
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens + self.output_tokens + self.reasoning_tokens
    }

    /// Share of prompt tokens that came from cache, 0.0–1.0.
    pub fn cache_hit_rate(&self) -> Option<f64> {
        if self.input_tokens == 0 {
            return None;
        }
        Some(self.cached_input_tokens as f64 / self.input_tokens as f64)
    }
}

/// Sum a slice of rows.
pub fn totals(rows: &[SessionLogRow]) -> Totals {
    let mut t = Totals::default();
    for r in rows {
        t.turns += 1;
        if r.ok {
            t.ok_turns += 1;
        } else {
            t.failed_turns += 1;
        }
        t.input_tokens += r.input_tokens;
        t.cached_input_tokens += r.cached_input_tokens;
        t.output_tokens += r.output_tokens;
        t.reasoning_tokens += r.reasoning_tokens;
        t.duration_ms += r.duration_ms;
    }
    t
}

/// Only rows at or after `since_ms`.
pub fn rows_since(rows: &[SessionLogRow], since_ms: u64) -> Vec<SessionLogRow> {
    rows.iter().filter(|r| r.ts >= since_ms).cloned().collect()
}

/// With `backend`, all rows are included.
pub fn rows_for_backend(rows: &[SessionLogRow], backend: Option<&str>) -> Vec<SessionLogRow> {
    match backend {
        None => rows.to_vec(),
        Some(b) => rows
            .iter()
            .filter(|r| r.provider.eq_ignore_ascii_case(b))
            .cloned()
            .collect(),
    }
}

/// Human-readable quota table.
pub fn render_report(report: &UsageReport) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "{} quota  (source: {})\n",
        report.backend, report.source
    ));

    // The bucket column must fit a full `"<group> / <window>"` label. It was 14
    // chars, which truncated `Gemini Models / weekly` to `Gemini Models…` and
    // threw away the window — the one field that tells two buckets in the same
    // group apart. Observed on real `pie usage agy` output, where two Gemini rows
    // rendered identically apart from their percentages.
    const BUCKET_WIDTH: usize = 32;

    if report.buckets.is_empty() {
        out.push_str("  no buckets reported\n");
    } else {
        out.push_str(&format!(
            "  {:<BUCKET_WIDTH$} {:>10} {:>22} {:>12}\n",
            "BUCKET", "REMAINING", "RESET", "USED/LIMIT"
        ));
        for b in &report.buckets {
            let remaining = b
                .remaining_fraction
                .map(|f| format!("{:.1}%", (f * 100.0).clamp(0.0, 100.0)))
                .unwrap_or_else(|| "—".to_string());
            let reset = b.reset_at.clone().unwrap_or_else(|| "—".to_string());
            let used = match (b.used, b.limit) {
                (Some(u), Some(l)) => format!("{u}/{l}"),
                (Some(u), None) => format!("{u}/—"),
                _ => "—".to_string(),
            };
            out.push_str(&format!(
                "  {:<BUCKET_WIDTH$} {:>10} {:>22} {:>12}\n",
                truncate_cell(&b.label, BUCKET_WIDTH),
                remaining,
                truncate_cell(&reset, 22),
                used
            ));
        }
    }

    for note in &report.notes {
        out.push_str(&format!("  note: {note}\n"));
    }
    out
}

/// Human-readable session-log totals.
pub fn render_totals(label: &str, t: &Totals) -> String {
    let mut out = String::new();
    out.push_str(&format!("{label}\n"));
    out.push_str(&format!(
        "  turns            {} ({} ok, {} failed)\n",
        t.turns, t.ok_turns, t.failed_turns
    ));
    out.push_str(&format!("  input tokens     {}\n", t.input_tokens));
    out.push_str(&format!("  cached input     {}\n", t.cached_input_tokens));
    out.push_str(&format!("  output tokens    {}\n", t.output_tokens));
    out.push_str(&format!("  reasoning tokens {}\n", t.reasoning_tokens));
    out.push_str(&format!("  total tokens     {}\n", t.total_tokens()));
    match t.cache_hit_rate() {
        Some(rate) => out.push_str(&format!("  cache hit rate   {:.1}%\n", rate * 100.0)),
        None => out.push_str("  cache hit rate   —\n"),
    }
    out.push_str(&format!(
        "  wall clock       {:.1}s\n",
        t.duration_ms as f64 / 1000.0
    ));
    out
}

fn truncate_cell(s: &str, width: usize) -> String {
    if s.chars().count() <= width {
        return s.to_string();
    }
    let kept: String = s.chars().take(width.saturating_sub(1)).collect();
    format!("{kept}…")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::provider::{Bucket, Usage};

    fn row(provider: &str, ok: bool, ts: u64) -> SessionLogRow {
        let mut r = SessionLogRow::from_turn(provider, "m", &Usage::default(), 1_000, "SUCCESS");
        r.ts = ts;
        r.ok = ok;
        r.input_tokens = 100;
        r.cached_input_tokens = 40;
        r.output_tokens = 10;
        r.reasoning_tokens = 5;
        r
    }

    #[test]
    fn totals_sum_every_field() {
        let rows = vec![
            row("agy", true, 1),
            row("codex", true, 2),
            row("agy", false, 3),
        ];
        let t = totals(&rows);
        assert_eq!(t.turns, 3);
        assert_eq!(t.ok_turns, 2);
        assert_eq!(t.failed_turns, 1);
        assert_eq!(t.input_tokens, 300);
        assert_eq!(t.cached_input_tokens, 120);
        assert_eq!(t.total_tokens(), 345, "input + output + reasoning");
        assert_eq!(t.duration_ms, 3_000);
    }

    #[test]
    fn cache_hit_rate_is_none_without_input() {
        assert_eq!(Totals::default().cache_hit_rate(), None);
        let t = totals(&[row("agy", true, 1)]);
        assert!((t.cache_hit_rate().unwrap() - 0.4).abs() < f64::EPSILON);
    }

    #[test]
    fn filters_by_backend_and_time() {
        let rows = vec![
            row("agy", true, 10),
            row("codex", true, 20),
            row("agy", true, 30),
        ];
        assert_eq!(rows_for_backend(&rows, None).len(), 3);
        assert_eq!(rows_for_backend(&rows, Some("agy")).len(), 2);
        assert_eq!(
            rows_for_backend(&rows, Some("AGY")).len(),
            2,
            "case-insensitive"
        );
        assert_eq!(rows_since(&rows, 20).len(), 2);
    }

    #[test]
    fn report_renders_without_buckets_and_with_them() {
        let empty = UsageReport {
            backend: "agy".into(),
            buckets: vec![],
            source: "agy /usage".into(),
            notes: vec!["none".into()],
        };
        let s = render_report(&empty);
        assert!(s.contains("no buckets reported"));
        assert!(s.contains("note: none"));

        let full = UsageReport {
            backend: "codex".into(),
            buckets: vec![
                Bucket {
                    label: "5h".into(),
                    remaining_fraction: Some(0.87),
                    reset_at: Some("2026-05-01T00:00:00Z".into()),
                    used: Some(13),
                    limit: Some(100),
                },
                Bucket {
                    label: "weekly".into(),
                    remaining_fraction: None,
                    reset_at: None,
                    used: None,
                    limit: None,
                },
            ],
            source: "wham/usage".into(),
            notes: vec![],
        };
        let s = render_report(&full);
        assert!(s.contains("87.0%"));
        assert!(s.contains("13/100"));
        assert!(s.contains('—'), "missing values render as a dash");
    }

    /// Regression: the bucket column was 14 chars, which truncated
    /// `Gemini Models / weekly` to `Gemini Models…` and dropped the window.
    /// Two buckets from the same group then rendered identically apart from
    /// their percentages, defeating the reason the group is in the label at all.
    ///
    /// Found by running `pie usage agy` against the real CLI, not by a test: the
    /// suite asserted values and never that a label survived rendering.
    #[test]
    fn a_real_agy_bucket_label_survives_rendering() {
        let bucket = |label: &str| Bucket {
            label: label.to_string(),
            remaining_fraction: Some(0.885),
            reset_at: Some("2026-10-15T01:32:32Z".to_string()),
            used: None,
            limit: None,
        };

        let rendered = render_report(&UsageReport {
            backend: "agy".into(),
            // The exact labels the real probe produces (`knowledge/agy.md`).
            buckets: vec![
                bucket("Gemini Models / weekly"),
                bucket("Gemini Models / 5h"),
                bucket("Claude and GPT models / weekly"),
                bucket("Claude and GPT models / 5h"),
            ],
            source: "agy --print /usage".into(),
            notes: vec![],
        });

        for window in ["weekly", "5h"] {
            assert!(
                rendered.contains(window),
                "the `{window}` window must survive rendering:\n{rendered}"
            );
        }
        for group in ["Gemini Models", "Claude and GPT models"] {
            assert!(
                rendered.contains(group),
                "group `{group}` missing:\n{rendered}"
            );
        }
        assert!(
            !rendered.contains('…'),
            "a real bucket label must fit the column; truncation hides the window:\n{rendered}"
        );
    }

    #[test]
    fn remaining_fraction_is_clamped() {
        let report = UsageReport {
            backend: "agy".into(),
            buckets: vec![Bucket {
                label: "5h".into(),
                remaining_fraction: Some(3.5), // backend sent something odd
                reset_at: None,
                used: None,
                limit: None,
            }],
            source: "s".into(),
            notes: vec![],
        };
        assert!(render_report(&report).contains("100.0%"));
    }

    #[test]
    fn totals_render_is_stable() {
        let s = render_totals("all time", &totals(&[row("agy", true, 1)]));
        assert!(s.contains("all time"));
        assert!(s.contains("total tokens     115"));
        assert!(s.contains("cache hit rate   40.0%"));
    }
}
