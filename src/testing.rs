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
//!
//! ## `EnvGuard` is not re-entrant, and says so
//!
//! The lock is a plain `Mutex`, so a test that takes a second `EnvGuard` while
//! the first is still in scope would block on its *own* thread forever. That is
//! a deadlock, not a slow test: `cargo test` has no per-test timeout, so the
//! whole suite stops producing output and every other env-touching test queues
//! behind the same lock. It happened — a two-`EnvGuard` config test stalled the
//! suite until it was killed by hand, and the visible symptom was a dozen
//! unrelated tests "running for over 60 seconds".
//!
//! A deadlock cannot be caught by a test, so it is caught at the point of use
//! instead: [`EnvGuard::set`] panics immediately with the reason when the same
//! thread already holds the lock. A named, instant failure is worth far more
//! than a silent hang, which is the same reasoning behind gates that fail loudly
//! (`ACCEPTANCE.md` §1). Scope the guards instead:
//!
//! ```ignore
//! { let _env = EnvGuard::set(&[("PIE_HOME", Some("a"))]); /* … */ }
//! { let _env = EnvGuard::set(&[("PIE_HOME", Some("b"))]); /* … */ }
//! ```
//!
//! Note that `tests/parse_codex.rs` has its own `HomeGuard` with a *separate*
//! lock, so nesting one inside the other is also a deadlock and is not covered
//! by this check.

#![cfg(test)]

use std::cell::Cell;
use std::sync::{Mutex, MutexGuard, OnceLock};

fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

thread_local! {
    /// Whether *this thread* already holds [`env_lock`].
    ///
    /// Thread-local rather than global on purpose: another thread holding the
    /// lock is ordinary contention and must wait, while this thread holding it
    /// is a deadlock that can only end in a hang.
    static HELD_BY_THIS_THREAD: Cell<bool> = const { Cell::new(false) };
}

/// Holds the global env lock and restores prior values when dropped.
#[derive(Debug)]
pub struct EnvGuard {
    _guard: MutexGuard<'static, ()>,
    saved: Vec<(String, Option<String>)>,
}

impl EnvGuard {
    /// Take the lock and apply `pairs`. A `None` value removes the variable.
    ///
    /// # Panics
    ///
    /// If this thread already holds the lock, i.e. a second `EnvGuard` was
    /// taken while the first is still in scope. That would deadlock the whole
    /// test binary, so it fails here instead, naming the fix.
    pub fn set(pairs: &[(&str, Option<&str>)]) -> Self {
        if HELD_BY_THIS_THREAD.with(Cell::get) {
            panic!(
                "EnvGuard is not re-entrant: this thread already holds it, so a second \
                 EnvGuard::set would deadlock the entire test binary (no test timeout \
                 exists to break it). Drop the first guard before taking another by \
                 wrapping each in its own block."
            );
        }

        // A poisoned lock means another test panicked while holding it; the
        // environment itself is still fine, so recover rather than cascade.
        let guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());
        HELD_BY_THIS_THREAD.with(|held| held.set(true));

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

        // Cleared before the lock is released by dropping `_guard`, so the next
        // `set` on this thread cannot race this flag.
        HELD_BY_THIS_THREAD.with(|held| held.set(false));
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    /// The guard's whole purpose is to turn a hang into a named failure, so the
    /// guard itself is the thing that most needs a test.
    ///
    /// A deadlock cannot be asserted (it would hang the harness), but the panic
    /// that replaces it can. Without this, the check could be deleted and the
    /// only symptom would be, once again, a test binary that stops producing
    /// output.
    #[test]
    #[should_panic(expected = "EnvGuard is not re-entrant")]
    fn taking_a_second_guard_on_the_same_thread_panics_instead_of_deadlocking() {
        let _first = EnvGuard::set(&[("PIE_QC_REENTRANCY_PROBE", Some("1"))]);

        // The first guard is deliberately still alive here. Before the check
        // this line blocked forever; now it fails with the reason and the fix.
        let _second = EnvGuard::set(&[("PIE_QC_REENTRANCY_PROBE", Some("2"))]);
    }

    /// The lock is released properly, so a *sequential* pair of guards is fine.
    ///
    /// This is the half that a naive "panic whenever a second guard exists"
    /// implementation would break, and it is what every legitimate test does.
    #[test]
    fn scoped_guards_are_reusable_in_sequence() {
        {
            let _env = EnvGuard::set(&[("PIE_QC_REENTRANCY_PROBE", Some("1"))]);
            assert_eq!(std::env::var("PIE_QC_REENTRANCY_PROBE").unwrap(), "1");
        }
        {
            let _env = EnvGuard::set(&[("PIE_QC_REENTRANCY_PROBE", Some("2"))]);
            assert_eq!(std::env::var("PIE_QC_REENTRANCY_PROBE").unwrap(), "2");
        }

        // And the variable is restored to what the process actually had, rather
        // than left behind for another test to trip over.
        assert!(
            std::env::var("PIE_QC_REENTRANCY_PROBE").is_err(),
            "the probe variable must not survive its guards"
        );
    }
}
