//! Tolerant JSONL/NDJSON helpers.
//!
//! The vendor CLIs print banners, deprecation warnings, and new event types we
//! have never seen. Rule (`RISKS.md` §3): never let a line we do not understand
//! turn a healthy run into a failure. Parsers built on these helpers emit
//! `StreamEvent::Unparsed`/`Other` and keep going.

use serde_json::Value;

/// Outcome of looking at one raw stdout line.
#[derive(Debug, Clone, PartialEq)]
pub enum LineRead {
    /// Whitespace only — skip silently.
    Blank,
    /// A JSON object/array/value.
    Value(Value),
    /// Not JSON at all (banner noise, a warning printed to stdout).
    Unparsed(String),
}

/// Classify one stdout line. Never fails.
pub fn parse_line(line: &str) -> LineRead {
    // A UTF-8 BOM or stray CR must not defeat parsing.
    let t = line.trim_start_matches('\u{feff}').trim();
    if t.is_empty() {
        return LineRead::Blank;
    }
    match serde_json::from_str::<Value>(t) {
        Ok(v) => LineRead::Value(v),
        Err(_) => LineRead::Unparsed(t.to_string()),
    }
}

/// Walk a dotted path, tolerating arrays (`usage.0.total`) and misses.
pub fn at<'a>(v: &'a Value, path: &str) -> Option<&'a Value> {
    let mut cur = v;
    for seg in path.split('.') {
        cur = match cur {
            Value::Object(map) => map.get(seg)?,
            Value::Array(items) => items.get(seg.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(cur)
}

/// String at `path`; numbers and bools are stringified rather than dropped.
pub fn s(v: &Value, path: &str) -> Option<String> {
    match at(v, path)? {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// Non-empty string at `path`.
pub fn s_nonempty(v: &Value, path: &str) -> Option<String> {
    s(v, path).filter(|x| !x.trim().is_empty())
}

/// Integer at `path`. Accepts a float that is integral (some CLIs emit `2.0`).
pub fn u(v: &Value, path: &str) -> Option<u64> {
    match at(v, path)? {
        Value::Number(n) => n
            .as_u64()
            .or_else(|| n.as_f64().filter(|f| *f >= 0.0).map(|f| f as u64)),
        // some CLIs send token counts as strings
        Value::String(s) => s.parse::<u64>().ok(),
        _ => None,
    }
}

/// Boolean at `path`.
pub fn b(v: &Value, path: &str) -> Option<bool> {
    match at(v, path)? {
        Value::Bool(x) => Some(*x),
        _ => None,
    }
}

/// Every string element of the array at `path`.
pub fn strings(v: &Value, path: &str) -> Vec<String> {
    match at(v, path) {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|i| i.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    }
}

/// Read `path` as an array of objects, returning an empty slice if absent.
pub fn objects(v: &Value, path: &str) -> Vec<Value> {
    match at(v, path) {
        Some(Value::Array(items)) => items.clone(),
        _ => Vec::new(),
    }
}

/// Short, redaction-safe rendering of a value for diagnostics.
pub fn brief(v: &Value, max: usize) -> String {
    let s = v.to_string();
    crate::error::truncate(&s, max)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use serde_json::json;

    #[test]
    fn classifies_lines() {
        assert_eq!(parse_line("   "), LineRead::Blank);
        assert_eq!(
            parse_line("\u{feff}{\"a\":1}"),
            LineRead::Value(json!({"a":1}))
        );
        assert_eq!(parse_line("  {\"a\":1}\r"), LineRead::Value(json!({"a":1})));
        assert_eq!(
            parse_line("Warning: something"),
            LineRead::Unparsed("Warning: something".into())
        );
        // half-written line
        assert!(matches!(parse_line("{\"a\":"), LineRead::Unparsed(_)));
    }

    #[test]
    fn dotted_paths() {
        let v = json!({"a": {"b": [{"c": "x"}]}, "n": 7, "f": 2.0, "fl": 1.5, "s": "12"});
        assert_eq!(s(&v, "a.b.0.c").as_deref(), Some("x"));
        assert_eq!(s(&v, "a.missing"), None);
        assert_eq!(u(&v, "n"), Some(7));
        assert_eq!(u(&v, "f"), Some(2));
        assert_eq!(u(&v, "fl"), Some(1));
        assert_eq!(u(&v, "s"), Some(12));
        assert_eq!(u(&v, "a.missing"), None);
    }

    #[test]
    fn arrays_and_flags() {
        let v = json!({"tools": ["view_file", "grep_search"], "ok": true});
        assert_eq!(strings(&v, "tools"), vec!["view_file", "grep_search"]);
        assert!(strings(&v, "nope").is_empty());
        assert_eq!(b(&v, "ok"), Some(true));
        assert_eq!(b(&v, "tools"), None);
    }

    #[test]
    fn brief_is_bounded() {
        let v = json!({"big": "x".repeat(500)});
        assert!(brief(&v, 20).len() < 60);
    }
}
