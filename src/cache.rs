//! Small TTL'd JSON caches under `$PIE_HOME/cache`.
//!
//! Model catalogs and version snapshots are cached on disk so startup stays
//! lazy and offline-friendly (`PLAN.md` §9: no auth or model probe at startup).
//!
//! Every read is **tolerant**: a missing, truncated, or corrupt file is a cache
//! miss, never an error. Callers then refresh from the CLI. A cache that can
//! break startup would be worse than no cache.

use std::path::Path;
use std::time::Duration;

use serde::{de::DeserializeOwned, Deserialize, Serialize};

use crate::error::Result;
use crate::session_log::now_millis;

/// A cached value plus when it was fetched.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CacheEntry<T> {
    /// Epoch milliseconds when the value was fetched.
    pub fetched_at: u64,
    /// The cached payload.
    pub value: T,
}

impl<T> CacheEntry<T> {
    /// Wrap a fresh value.
    pub fn new(value: T, now_ms: u64) -> Self {
        Self {
            fetched_at: now_ms,
            value,
        }
    }

    /// Age in milliseconds, saturating at zero for a clock that moved backwards.
    pub fn age_ms(&self, now_ms: u64) -> u64 {
        now_ms.saturating_sub(self.fetched_at)
    }

    /// Whether the entry is still within `ttl`.
    pub fn is_fresh(&self, ttl: Duration, now_ms: u64) -> bool {
        self.age_ms(now_ms) < ttl.as_millis() as u64
    }
}

/// Read an entry. `None` on missing, unreadable, or corrupt content.
pub fn read<T: DeserializeOwned>(path: &Path) -> Option<CacheEntry<T>> {
    let text = std::fs::read_to_string(path).ok()?;
    match serde_json::from_str::<CacheEntry<T>>(&text) {
        Ok(entry) => Some(entry),
        Err(e) => {
            tracing::debug!(
                path = %path.display(),
                error = %e,
                "ignoring unreadable cache entry"
            );
            None
        }
    }
}

/// Read a value only if it is still fresh.
pub fn read_fresh<T: DeserializeOwned>(path: &Path, ttl: Duration, now_ms: u64) -> Option<T> {
    read::<T>(path)
        .filter(|e| e.is_fresh(ttl, now_ms))
        .map(|e| e.value)
}

/// Timestamp a fresh value with the current time and write it.
pub fn write_now<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    write(path, &CacheEntry::new(value, now_millis()))
}

/// Write an entry atomically-ish: a corrupt half-write is still a miss, but we
/// create the parent directory so a first run works.
pub fn write<T: Serialize>(path: &Path, entry: &CacheEntry<T>) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let mut text = serde_json::to_string_pretty(entry)?;
    text.push('\n');
    std::fs::write(path, text)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    struct Payload {
        models: Vec<String>,
    }

    fn payload() -> Payload {
        Payload {
            models: vec!["gemini-3.8-flash".into()],
        }
    }

    #[test]
    fn round_trips_a_value() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("agy-models.json");

        write_now(&path, &payload()).unwrap();
        let entry: CacheEntry<Payload> = read(&path).unwrap();
        assert_eq!(entry.value, payload());
    }

    #[test]
    fn freshness_respects_the_ttl() {
        let entry = CacheEntry::new(payload(), 1_000);
        let ttl = Duration::from_secs(60);

        assert!(entry.is_fresh(ttl, 1_000), "age 0");
        assert!(entry.is_fresh(ttl, 60_000), "age 59s");
        assert!(!entry.is_fresh(ttl, 61_000), "age 61s");
        assert_eq!(entry.age_ms(500), 0, "clock skew saturates at zero");
    }

    #[test]
    fn read_fresh_returns_none_for_a_stale_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("m.json");
        let ttl = Duration::from_secs(60);

        write(&path, &CacheEntry::new(payload(), 1_000)).unwrap();
        assert!(read_fresh::<Payload>(&path, ttl, 30_000).is_some());
        assert!(
            read_fresh::<Payload>(&path, ttl, 300_000).is_none(),
            "stale entry must not be served"
        );
    }

    #[test]
    fn missing_and_corrupt_files_are_cache_misses_not_errors() {
        let dir = tempfile::tempdir().unwrap();

        assert!(read::<Payload>(&dir.path().join("absent.json")).is_none());

        let broken = dir.path().join("broken.json");
        std::fs::write(&broken, "{ this is not json").unwrap();
        assert!(read::<Payload>(&broken).is_none());

        // Valid JSON of the wrong shape is also a miss, not a panic.
        let wrong = dir.path().join("wrong.json");
        std::fs::write(&wrong, r#"{"fetched_at":1,"value":{"other":true}}"#).unwrap();
        assert!(read::<Payload>(&wrong).is_none());
    }

    #[test]
    fn expired_entries_are_ignored_after_a_schema_change() {
        // A cache written by an older pie must not crash a newer one.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy.json");
        std::fs::write(&path, r#"{"fetched_at":1,"value":"an old scalar"}"#).unwrap();
        assert!(read::<Payload>(&path).is_none());
    }
}
