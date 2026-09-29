//! Eviction of pull-through cache files, least recently used first.
//!
//! Only cache files are candidates: the candidates are the catalog's
//! `cache_files` rows, so a layer file (`b'L'` id) is never considered. A file
//! with a live open handle is skipped. Eviction runs on a background thread
//! (at most one at a time) when a fetched block, or a handle's close, finds
//! the cached logical bytes (the bytes cache files hold in the store) over
//! `cache_max_bytes`, and can be run directly with
//! [`Storage::enforce_cache_budget`].
//!
//! **Back-off.** A run that cannot reach its target because the rest of the
//! cache is open would otherwise be retried on every miss. After such a run no
//! background run starts until a cache file's last handle closes (a file
//! became evictable) or the minute changes, and the warning fires once per
//! episode (until a run reaches its target).

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use vfs_block_store::CompactOptions;

use crate::cached::{lock, now_minute, sub_logical};
use crate::catalog::CacheRec;
use crate::ids::cache_file_id;
use crate::storage::{Storage, StorageError};

/// Eviction stops once cached logical bytes are at or below this share of the
/// budget, so it does not run again on the very next miss.
const TARGET_PERCENT: u128 = 90;

impl Storage {
    /// If the cache is over budget, deletes least-recently-used cache files
    /// (skipping any with an open handle) until it is at or below 90% of
    /// `cache_max_bytes`, then compacts the store. Returns the number of files
    /// evicted. Runs one at a time, with each other and with the background
    /// eviction.
    pub fn enforce_cache_budget(&self) -> Result<u64, StorageError> {
        let _serial = lock(&self.cache.evict_lock);
        self.cache.eviction_runs.fetch_add(1, Ordering::SeqCst);
        // The catalog's rows, overlaid with the access log's newer ones. The
        // log is read, not committed: access times reach the catalog at most
        // once a minute, however often eviction runs.
        let (pending, order) = self.access_snapshot();
        let mut recs: HashMap<[u8; 16], CacheRec> = self.catalog.cache_all()?.into_iter().collect();
        recs.extend(pending);
        let mut total: u64 = recs.values().map(|r| r.logical_bytes).sum();
        let max = self.cfg.cache_max_bytes;
        if total <= max {
            self.cache.warned_stuck.store(false, Ordering::Relaxed);
            return Ok(0);
        }
        let target = (u128::from(max) * TARGET_PERCENT / 100) as u64;
        let mut recs: Vec<_> = recs.into_iter().collect();
        recs.sort_by_key(|(h, r)| (r.last_access_min, order.get(h).copied().unwrap_or(0)));

        let mut evicted = 0u64;
        let mut first_err: Option<StorageError> = None;
        for (hash, rec) in recs {
            if total <= target {
                break;
            }
            let id = cache_file_id(&hash);
            let counts = self.open_counts();
            if counts.get(&id).is_some_and(|&n| n > 0) {
                continue;
            }
            // Catalog row first, store file second (spec §6). If the row
            // cannot be removed, nothing has changed: try the next file.
            if let Err(e) = self.catalog.cache_remove(&hash) {
                tracing::warn!(error = %e, "evicting a cache file: catalog row not removed");
                first_err.get_or_insert(e);
                continue;
            }
            self.forget_access(&hash);
            self.ram.invalidate_file(&id);
            // The row is gone, so the file no longer counts, whatever the
            // store says; a store file left behind is an orphan that
            // reconciliation at the next open deletes.
            match self.store.delete(&id) {
                Ok(()) | Err(vfs_block_store::Error::NotFound) => {}
                Err(e) => {
                    tracing::warn!(error = %e, "evicting a cache file: store delete failed");
                    first_err.get_or_insert(e.into());
                }
            }
            drop(counts);
            total = total.saturating_sub(rec.logical_bytes);
            sub_logical(&self.cache.cached_logical, rec.logical_bytes);
            evicted += 1;
        }
        if total > target {
            *lock(&self.cache.stuck_since) = Some(now_minute());
            if !self.cache.warned_stuck.swap(true, Ordering::Relaxed) {
                tracing::warn!(
                    total,
                    target,
                    "cache stays over its eviction target: the rest is open"
                );
            }
        } else {
            *lock(&self.cache.stuck_since) = None;
            self.cache.warned_stuck.store(false, Ordering::Relaxed);
        }
        if evicted > 0 {
            if let Err(e) = self.store.compact(CompactOptions::default()) {
                tracing::warn!(error = %e, "compacting after eviction failed");
                first_err.get_or_insert(e.into());
            }
        }
        match first_err {
            Some(e) if evicted == 0 => Err(e),
            _ => Ok(evicted),
        }
    }

    /// Waits for every background eviction started so far.
    pub(crate) fn wait_for_eviction(&self) {
        let threads = std::mem::take(&mut *lock(&self.cache.evict_threads));
        for h in threads {
            let _ = h.join();
        }
    }
}

/// Starts a background eviction if the cache is over budget, none is running,
/// and the last run is not backing off (see the module docs).
pub(crate) fn maybe_evict(s: &Arc<Storage>) {
    if s.cache.cached_logical.load(Ordering::Relaxed) <= s.cfg.cache_max_bytes {
        return;
    }
    if lock(&s.cache.stuck_since).is_some_and(|m| now_minute() <= m) {
        return;
    }
    if s.cache.evicting.swap(true, Ordering::AcqRel) {
        return;
    }
    let bg = Arc::clone(s);
    let spawned = std::thread::Builder::new()
        .name("vfs-cache-evict".into())
        .spawn(move || {
            if let Err(e) = bg.enforce_cache_budget() {
                tracing::warn!(error = %e, "cache eviction failed");
            }
            bg.cache.evicting.store(false, Ordering::Release);
        });
    match spawned {
        Ok(h) => {
            // Never join here: a reader must not wait on someone's eviction.
            // Finished threads are dropped; the rest wait for `close`.
            let mut threads = lock(&s.cache.evict_threads);
            threads.retain(|t| !t.is_finished());
            threads.push(h);
        }
        Err(e) => {
            tracing::warn!(error = %e, "could not start cache eviction");
            s.cache.evicting.store(false, Ordering::Release);
        }
    }
}
