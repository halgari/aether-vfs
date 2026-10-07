//! Small helpers shared across the crate.
//!
//! **Poison policy.** The crate has two, on purpose:
//!
//! - [`lock`] enters a poisoned mutex. It is for the cache, the storage's
//!   registries and the durability bookkeeping, whose critical sections leave
//!   their maps consistent at every step: a panic elsewhere is no reason to
//!   stop serving. (A write pair that a panic leaves half done is handled by
//!   marking the session dirty, see the durability doc, not by poisoning.)
//! - [`lock_status`] turns poison into `ST_IO_ERROR`. It is for the layer
//!   provider, whose locks guard state that is reported to the guest: after a
//!   panic in a namespace or file operation that state may disagree with the
//!   catalog, so the operation fails instead of carrying on.

use std::sync::{Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use vfs_provider::map_io_err;

/// Locks `m`, entering it if poisoned (see the module docs).
pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Minutes since the Unix epoch (0 if the clock is before it).
pub(crate) fn now_minute() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() / 60)
        .unwrap_or(0)
}

/// Locks `m`; a poisoned lock is `ST_IO_ERROR` (see the module docs).
pub(crate) fn lock_status<T>(m: &Mutex<T>) -> Result<MutexGuard<'_, T>, i32> {
    m.lock().map_err(|_| map_io_err())
}
