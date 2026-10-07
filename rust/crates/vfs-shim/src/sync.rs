//! The one lock policy of the close path.
//!
//! `NtClose` reclaims tracking records (file handle tables, registry key tables, enumeration
//! and notification state). It must never block for good: a thread killed while holding a
//! `std::sync::Mutex` leaves it locked and not poisoned, an exiting process closes handles from
//! the one thread left, and a blocking `lock` there hangs the process. So the close path only
//! ever `try_lock`s, a bounded number of times, and gives up. A record lost that way belongs to
//! a handle that is going away. See docs/shim-invariants.md, "Close-path locking".

use std::sync::{Mutex, MutexGuard, TryLockError};

/// How many times the close path tries a table, and what it counts when it gives up.
pub(crate) struct CloseLock {
    /// `try_lock` attempts, with a `yield_now` between two attempts that found the lock held.
    pub attempts: u32,
    /// Called when the lock was not taken.
    pub given_up: Option<fn()>,
}

impl CloseLock {
    /// The file handle table: one attempt, silently. Every `NtClose` of every handle pays for
    /// it, and a lost reclamation is harmless.
    pub(crate) const FILE: CloseLock = CloseLock {
        attempts: 1,
        given_up: None,
    };

    /// The registry tables: a bounded spin, each give-up counted in `hookstats`.
    pub(crate) const REGISTRY: CloseLock = CloseLock {
        attempts: 10_000,
        given_up: Some(crate::hookstats::note_reg_close_lock_given_up),
    };
}

/// Lock `m` for a removal on the close path, under `policy`. `None` when it was not taken: held
/// by someone for the whole budget, or poisoned.
pub(crate) fn lock_for_close<'a, T>(
    m: &'a Mutex<T>,
    policy: &CloseLock,
) -> Option<MutexGuard<'a, T>> {
    for attempt in 0..policy.attempts {
        match m.try_lock() {
            Ok(g) => return Some(g),
            Err(TryLockError::Poisoned(_)) => break,
            Err(TryLockError::WouldBlock) => {
                if attempt + 1 < policy.attempts {
                    std::thread::yield_now();
                }
            }
        }
    }
    if let Some(given_up) = policy.given_up {
        given_up();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static GIVEN_UP: AtomicU32 = AtomicU32::new(0);

    fn count() {
        GIVEN_UP.fetch_add(1, Ordering::Relaxed);
    }

    #[test]
    fn a_free_lock_is_taken_and_a_held_one_is_given_up_not_waited_for() {
        let m = Mutex::new(1);
        let counting = CloseLock {
            attempts: 5,
            given_up: Some(count),
        };
        assert!(lock_for_close(&m, &counting).is_some());
        assert_eq!(GIVEN_UP.load(Ordering::Relaxed), 0);
        let _held = m.lock().unwrap();
        assert!(lock_for_close(&m, &counting).is_none());
        assert_eq!(GIVEN_UP.load(Ordering::Relaxed), 1);
        assert!(lock_for_close(&m, &CloseLock::FILE).is_none());
        assert_eq!(GIVEN_UP.load(Ordering::Relaxed), 1, "FILE counts nothing");
    }
}
