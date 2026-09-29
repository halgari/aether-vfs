//! Eviction of pull-through cache files, least recently used first.
//!
//! Only cache files are candidates: the candidates are the catalog's
//! `cache_files` rows, so a layer file (`b'L'` id) is never considered. A file
//! with a live open handle is skipped. Eviction runs on a background thread
//! (at most one at a time) when a new file or a fetched block takes the
//! cached logical bytes over `cache_max_bytes`, and can be run directly with
//! [`Storage::enforce_cache_budget`].

use std::sync::atomic::Ordering;
use std::sync::Arc;

use vfs_block_store::CompactOptions;

use crate::cached::lock;
use crate::ids::cache_file_id;
use crate::storage::{Storage, StorageError};

/// Eviction stops once cached logical bytes are at or below this share of the
/// budget, so it does not run again on the very next miss.
const TARGET_PERCENT: u128 = 90;

impl Storage {
    /// If the cache is over budget, deletes least-recently-used cache files
    /// (skipping any with an open handle) until it is at or below 90% of
    /// `cache_max_bytes`, then compacts the store. Returns the number of files
    /// evicted.
    pub fn enforce_cache_budget(&self) -> Result<u64, StorageError> {
        // Committed first, so the catalog's access times are current.
        self.commit_access()?;
        let mut rows = self.catalog.cache_all()?;
        let mut total: u64 = rows.iter().map(|(_, r)| r.logical_bytes).sum();
        let max = self.cfg.cache_max_bytes;
        if total <= max {
            return Ok(0);
        }
        let target = (u128::from(max) * TARGET_PERCENT / 100) as u64;
        let order = self.touch_order();
        rows.sort_by_key(|(h, r)| (r.last_access_min, order.get(h).copied().unwrap_or(0)));

        let mut evicted = 0u64;
        for (hash, rec) in rows {
            if total <= target {
                break;
            }
            let id = cache_file_id(&hash);
            let counts = self.open_counts();
            if counts.get(&id).is_some_and(|&n| n > 0) {
                continue;
            }
            self.forget_access(&hash);
            // Catalog row first, store file second (spec §6).
            self.catalog.cache_remove(&hash)?;
            match self.store.delete(&id) {
                Ok(()) | Err(vfs_block_store::Error::NotFound) => {}
                Err(e) => return Err(e.into()),
            }
            self.ram.invalidate_file(&id);
            drop(counts);
            total -= rec.logical_bytes;
            let _ =
                self.cache
                    .cached_logical
                    .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                        Some(v.saturating_sub(rec.logical_bytes))
                    });
            evicted += 1;
        }
        if total > target {
            tracing::warn!(
                total,
                target,
                "cache stays over its eviction target: the rest is open"
            );
        }
        if evicted > 0 {
            self.store.compact(CompactOptions::default())?;
        }
        Ok(evicted)
    }

    /// Waits for a background eviction, if one is running.
    pub(crate) fn wait_for_eviction(&self) {
        let h = lock(&self.cache.evict_thread).take();
        if let Some(h) = h {
            let _ = h.join();
        }
    }
}

/// Starts a background eviction if the cache is over budget and none is
/// running.
pub(crate) fn maybe_evict(s: &Arc<Storage>) {
    if s.cache.cached_logical.load(Ordering::Relaxed) <= s.cfg.cache_max_bytes {
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
            // A previous thread here has already cleared `evicting`, so it is
            // done or about to be.
            let old = lock(&s.cache.evict_thread).replace(h);
            if let Some(old) = old {
                let _ = old.join();
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "could not start cache eviction");
            s.cache.evicting.store(false, Ordering::Release);
        }
    }
}
