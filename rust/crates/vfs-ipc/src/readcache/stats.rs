//! Counters, the stats snapshot and per-file diagnostics.

use std::sync::atomic::{AtomicU64, Ordering};

/// Counters, as of one [`ReadCache::stats`] call.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CacheStats {
    /// Small reads served wholly from blocks already held.
    pub hits: u64,
    /// Small reads served after fetching (or waiting for) a block.
    pub misses: u64,
    /// Of those, the ones whose block the process-wide cap had evicted from
    /// this file: a capacity cost, not the file's access pattern.
    pub pressure_misses: u64,
    /// Small reads on a cacheable file the cache declined (file gone cold,
    /// changed, no room, or a fetch that failed), and so read uncached.
    pub declined: u64,
    /// Block fetches (one round trip each), and the bytes they brought in.
    pub fetches: u64,
    pub bytes_fetched: u64,
    /// Block fetches that failed or came back short (each followed by an
    /// uncached read), and fetches given up on because they outlived
    /// [`CacheConfig::wait`].
    pub fetch_failures: u64,
    pub fetches_abandoned: u64,
    /// Units dropped to make room (per file or process-wide).
    pub evictions: u64,
    /// Files dropped because they changed or might have, and the units that
    /// held between them.
    pub invalidations: u64,
    pub blocks_invalidated: u64,
    /// Times a file went cold: read too randomly to be worth caching, or
    /// its fetches kept failing.
    pub cold: u64,
    /// Bytes held now (in-flight fetches included), the cap, and files known.
    pub resident_bytes: u64,
    pub max_bytes: u64,
    pub files: u64,
}

/// What the cache did for one file, for the stats report's per-file table.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FileDiag {
    /// Small reads offered (served or declined).
    pub reads: u64,
    pub hits: u64,
    pub misses: u64,
    /// Misses on a unit the process-wide cap had evicted.
    pub pressure_misses: u64,
    pub declined: u64,
    pub fetches: u64,
    pub bytes_fetched: u64,
    /// Times it went cold because its reads missed too often (`cold_guard`)
    /// or because its fetches failed (`cold_failures`).
    pub cold_guard: u32,
    pub cold_failures: u32,
}

impl FileDiag {
    pub(super) fn add(&mut self, o: &FileDiag) {
        self.reads += o.reads;
        self.hits += o.hits;
        self.misses += o.misses;
        self.pressure_misses += o.pressure_misses;
        self.declined += o.declined;
        self.fetches += o.fetches;
        self.bytes_fetched += o.bytes_fetched;
        self.cold_guard += o.cold_guard;
        self.cold_failures += o.cold_failures;
    }
}

/// One row of [`ReadCache::top_files`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileReport {
    pub root: u32,
    pub path: String,
    pub diag: FileDiag,
    /// Cold now (being read uncached), and poisoned (never cached again).
    pub cold_now: bool,
    pub poisoned: bool,
}

#[derive(Default)]
pub(super) struct Counters {
    pub(super) hits: AtomicU64,
    pub(super) misses: AtomicU64,
    pub(super) pressure_misses: AtomicU64,
    pub(super) declined: AtomicU64,
    pub(super) fetches: AtomicU64,
    pub(super) bytes_fetched: AtomicU64,
    pub(super) evictions: AtomicU64,
    pub(super) invalidations: AtomicU64,
    pub(super) blocks_invalidated: AtomicU64,
    pub(super) cold: AtomicU64,
    pub(super) fetch_failures: AtomicU64,
    pub(super) fetches_abandoned: AtomicU64,
}

pub(super) fn bump(c: &AtomicU64, n: u64) {
    c.fetch_add(n, Ordering::Relaxed);
}
