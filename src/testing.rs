//! Test-only helpers for process-global environment mutation.
//!
//! `std::env::set_var` is process-wide while Rust runs tests on parallel
//! threads, so two tests that both touch `HOME` will corrupt each other. Every
//! test that mutates the environment must take [`EnvGuard`], which serialises
//! the mutation and restores the previous values on drop.
//!
//! This was a real bug: `paths`/`auth`/`config` tests raced on `HOME` and
//! `PIE_HOME` and produced a nonsense failure in
//! `paths::tests::empty_override_is_ignored`.

#![cfg(test)]

use std::sync::{Mutex, MutexGuard, OnceLock};

fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Holds the global env lock and restores prior values when dropped.
#[derive(Debug)]
pub struct EnvGuard {
    _guard: MutexGuard<'static, ()>,
    saved: Vec<(String, Option<String>)>,
}

impl EnvGuard {
    /// Take the lock and apply `pairs`. A `None` value removes the variable.
    pub fn set(pairs: &[(&str, Option<&str>)]) -> Self {
        // A poisoned lock means another test panicked while holding it; the
        // environment itself is still fine, so recover rather than cascade.
        let guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());

        let saved = pairs
            .iter()
            .map(|(k, _)| ((*k).to_string(), std::env::var(k).ok()))
            .collect();

        for (k, v) in pairs {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }

        Self {
            _guard: guard,
            saved,
        }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (k, v) in &self.saved {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
    }
}
