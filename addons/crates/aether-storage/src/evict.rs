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

use std::sync::Arc;
use std::sync::atomic::Ordering;

use aether_block_store::CompactOptions;

use crate::cached::sub_logical;
use crate::ids::{StoreIdKind, cache_file_id, classify_store_id};
use crate::storage::{Storage, StorageError};
use crate::util::{lock, now_minute};

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
        let max = self.cfg.cache_max_bytes;
        let (recs, order, mut total) = self.budget_snapshot(max)?;
        #[cfg(test)]
        if let Some(hook) = lock(&self.cache.after_snapshot).take() {
            hook();
        }
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
            // What the file holds now: it may have grown since the snapshot.
            let held = match self.remove_cache_row(&hash) {
                Ok(held) => held.unwrap_or(rec.logical_bytes),
                Err(e) => {
                    tracing::warn!(error = %e, "evicting a cache file: catalog row not removed");
                    first_err.get_or_insert(e);
                    continue;
                }
            };
            self.ram.invalidate_file(&id);
            // The row is gone, so the file no longer counts, whatever the
            // store says; a store file left behind is an orphan that
            // reconciliation at the next open deletes.
            match self.store.delete(&id) {
                Ok(()) | Err(aether_block_store::Error::NotFound) => {}
                Err(e) => {
                    tracing::warn!(error = %e, "evicting a cache file: store delete failed");
                    self.needs_reconcile("an evicted cache file's store delete failed");
                    first_err.get_or_insert(e.into());
                }
            }
            drop(counts);
            total = total.saturating_sub(rec.logical_bytes);
            sub_logical(&self.cache.cached_logical, held);
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
        if evicted > 0
            && let Err(e) = self.store.compact(CompactOptions::default())
        {
            tracing::warn!(error = %e, "compacting after eviction failed");
            first_err.get_or_insert(e.into());
        }
        match first_err {
            Some(e) if evicted == 0 => Err(e),
            _ => Ok(evicted),
        }
    }

    /// Drops every pull-through cache file that has no open handle: its
    /// catalog row, then its store data (the order eviction uses), and any
    /// cache file the store holds without a row. Layers are untouched. Then
    /// compacts the store and makes it all durable. The next read of a
    /// dropped file fetches it from its source again.
    pub fn clear_cache(&self) -> Result<ClearReport, StorageError> {
        let _serial = lock(&self.cache.evict_lock);
        let (recs, _, _) = self.budget_snapshot(u64::MAX)?;
        let mut report = ClearReport::default();
        for (hash, rec) in recs {
            let id = cache_file_id(&hash);
            let counts = self.open_counts();
            if counts.get(&id).is_some_and(|&n| n > 0) {
                report.open_skipped += 1;
                continue;
            }
            let held = self.remove_cache_row(&hash)?.unwrap_or(rec.logical_bytes);
            self.ram.invalidate_file(&id);
            match self.store.delete(&id) {
                Ok(()) | Err(aether_block_store::Error::NotFound) => {}
                Err(e) => {
                    // The row is gone and the file stays: an orphan for reconcile.
                    self.needs_reconcile(
                        "clear_cache: store delete failed after the row was removed",
                    );
                    return Err(e.into());
                }
            }
            drop(counts);
            sub_logical(&self.cache.cached_logical, held);
            report.files += 1;
            report.logical_bytes += held;
        }
        // Store files of the cache with no row (a crash between a row's
        // removal and the store delete; reconciliation at open drops these
        // too).
        for id in self.store.file_ids()? {
            let StoreIdKind::Cache(hash) = classify_store_id(&id) else {
                continue;
            };
            let id = cache_file_id(&hash);
            let counts = self.open_counts();
            if counts.get(&id).is_some_and(|&n| n > 0) {
                continue;
            }
            self.ram.invalidate_file(&id);
            match self.store.delete(&id) {
                Ok(()) => report.orphans += 1,
                Err(aether_block_store::Error::NotFound) => {}
                Err(e) => {
                    self.needs_reconcile("deleting an orphan cache file failed");
                    return Err(e.into());
                }
            }
        }
        if report.files + report.orphans > 0 {
            self.store.compact(CompactOptions::default())?;
        }
        self.sync()?;
        Ok(report)
    }

    /// Waits for every background eviction started so far.
    pub(crate) fn wait_for_eviction(&self) {
        let threads = std::mem::take(&mut *lock(&self.cache.evict_threads));
        for h in threads {
            let _ = h.join();
        }
    }
}

/// What [`Storage::clear_cache`] dropped.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClearReport {
    /// Cache files dropped, and the logical bytes they held.
    pub files: u64,
    pub logical_bytes: u64,
    /// Cache files the store held without a catalog row, dropped too.
    pub orphans: u64,
    /// Cache files left alone because a handle has them open.
    pub open_skipped: u64,
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
        .name("aether-storage-evict".into())
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use vfs_provider::{OPEN_READ, VPath};

    use super::*;
    use crate::cached::tests::{
        BS, MapSource, key, pattern, read_all, slow, slow_fixture, small_cfg, temp_storage,
        temp_storage_with, write_layer_file,
    };
    use crate::config::StorageConfig;
    use crate::ids::layer_file_id;

    /// `n` files of 1000 bytes each, named `f1..=fn`.
    fn thousand_byte_files(n: u8) -> Arc<MapSource> {
        let files: Vec<(String, Vec<u8>)> = (1..=n)
            .map(|i| (format!("f{i}"), pattern(1000, i)))
            .collect();
        let refs: Vec<(&str, Vec<u8>)> =
            files.iter().map(|(n, b)| (n.as_str(), b.clone())).collect();
        MapSource::with(&refs)
    }

    #[test]
    fn eviction_keeps_the_budget_and_evicts_least_recent_first() {
        let (s, _d) = temp_storage_with(StorageConfig {
            cache_max_bytes: 3000,
            ..small_cfg()
        });
        let src = slow(thousand_byte_files(4));
        let p = s.cached(src.clone(), key());
        for f in ["f1", "f2", "f3", "f1", "f4"] {
            read_all(&p, f);
        }
        s.wait_for_eviction();
        let st = s.cache_stats();
        assert!(
            st.cached_logical_bytes <= 2700,
            "evicted to 90% of the budget: {st:?}"
        );

        let before = src.reads();
        read_all(&p, "f1");
        read_all(&p, "f4");
        assert_eq!(src.reads(), before, "f1 (touched again) and f4 are kept");
        read_all(&p, "f2");
        assert!(
            src.reads() > before,
            "f2, the least recently used, was evicted"
        );
    }

    /// A file read further while an eviction runs (after it took its
    /// snapshot) is subtracted with what it holds when it is evicted, not
    /// what the snapshot said, so the running total cannot drift upward.
    #[test]
    fn eviction_subtracts_what_a_file_holds_when_it_goes() {
        let (s, _d) = temp_storage_with(StorageConfig {
            cache_max_bytes: 4 * BS as u64,
            ..small_cfg()
        });
        let src = slow(MapSource::with(&[
            ("a", pattern(4 * BS, 1)),
            ("b", pattern(4 * BS, 2)),
        ]));
        let p = s.cached(src, key());
        s.cache.evicting.store(true, Ordering::SeqCst); // no background runs
        let (h, _, _) = p.open(VPath::at_default("a"), OPEN_READ).unwrap();
        p.read_at(h, 0, &mut [0u8; 10]).unwrap(); // one block of a
        p.close(h).unwrap();
        read_all(&p, "b");
        assert_eq!(s.cache_stats().cached_logical_bytes, 5 * BS as u64);

        let p2 = Arc::clone(&p);
        *s.cache.after_snapshot.lock().unwrap() = Some(Box::new(move || {
            read_all(&p2, "a"); // a now holds four blocks
        }));
        assert_eq!(s.enforce_cache_budget().unwrap(), 2);
        assert_eq!(s.cache_stats().cached_logical_bytes, 0);
        assert!(s.catalog.cache_all().unwrap().is_empty());
        s.cache.evicting.store(false, Ordering::SeqCst);
    }

    /// A run that finds the cache within budget sets the running total to
    /// what the catalog and the access log say, so a drifted count cannot
    /// keep starting no-op runs.
    #[test]
    fn a_run_within_budget_resyncs_the_total() {
        let (s, _d) = temp_storage_with(StorageConfig {
            cache_max_bytes: 8 * BS as u64,
            ..small_cfg()
        });
        let src = slow(MapSource::with(&[("b", pattern(4 * BS, 2))]));
        let p = s.cached(src, key());
        read_all(&p, "b");
        s.wait_for_eviction();
        s.cache.cached_logical.fetch_add(1 << 40, Ordering::SeqCst);
        assert_eq!(s.enforce_cache_budget().unwrap(), 0);
        assert_eq!(s.cache_stats().cached_logical_bytes, 4 * BS as u64);
    }

    #[test]
    fn eviction_skips_open_files_and_never_touches_layer_files() {
        let (s, _d) = temp_storage_with(StorageConfig {
            cache_max_bytes: 3000,
            ..small_cfg()
        });
        let layer = layer_file_id(&[7; 16]);
        s.store.set_len(&layer, 10).unwrap();
        s.store.write_blocks(&layer, 0, &[1u8; 10]).unwrap();

        let src = slow(thousand_byte_files(4));
        let p = s.cached(src.clone(), key());
        read_all(&p, "f1");
        // f1 is the least recently used, but a handle holds it open.
        let (h, _, _) = p.open(VPath::at_default("f1"), OPEN_READ).unwrap();
        for f in ["f2", "f3", "f4"] {
            read_all(&p, f);
        }
        s.wait_for_eviction();
        assert!(s.enforce_cache_budget().is_ok());

        let before = src.reads();
        let mut buf = [0u8; 1000];
        assert_eq!(p.read_at(h, 0, &mut buf).unwrap(), 1000);
        assert_eq!(&buf[..], &pattern(1000, 1)[..]);
        assert_eq!(src.reads(), before, "the open file was not evicted");
        p.close(h).unwrap();
        assert!(s.cache_stats().cached_logical_bytes <= 2700);

        let mut lb = [0u8; 10];
        let r = s.store.read(&layer, 0, &mut lb).unwrap();
        assert!(r.missing.is_empty());
        assert_eq!(lb, [1u8; 10], "layer files are never evicted");
    }

    #[test]
    fn enforce_under_budget_evicts_nothing() {
        let (s, _d) = temp_storage();
        let p = s.cached(slow_fixture(), key());
        read_all(&p, "a.txt");
        assert_eq!(s.enforce_cache_budget().unwrap(), 0);
        let before = s.cache_stats();
        assert_eq!(before.cached_logical_bytes, 5);
    }

    #[test]
    fn clear_cache_drops_cache_files_and_keeps_layers() {
        let (s, _d) = temp_storage();
        let (a, b, c) = (pattern(5 * BS + 7, 1), pattern(3 * BS, 2), pattern(BS, 3));
        let src = slow(MapSource::with(&[
            ("a", a.clone()),
            ("b", b.clone()),
            ("c", c.clone()),
        ]));
        let p = s.cached(src.clone(), key());
        assert_eq!(read_all(&p, "a"), a);
        assert_eq!(read_all(&p, "b"), b);
        // "c" stays open across the clear: it is left alone.
        let (hc, _, _) = p.open(VPath::at_default("c"), OPEN_READ).unwrap();
        let mut buf = vec![0u8; c.len()];
        assert_eq!(p.read_at(hc, 0, &mut buf).unwrap(), c.len());
        let body = pattern(4 * BS + 1, 9);
        write_layer_file(&s, "saves", "save1.ess", &body);
        let before = s.space_usage().unwrap();
        assert_eq!(before.cache.files, 3);
        assert_eq!(
            before.cache.logical_bytes,
            (a.len() + b.len() + c.len()) as u64
        );
        assert_eq!(before.layers["saves"].logical_bytes, body.len() as u64);

        let r = s.clear_cache().unwrap();
        assert_eq!((r.files, r.open_skipped), (2, 1));
        assert_eq!(r.logical_bytes, (a.len() + b.len()) as u64);
        assert_eq!(s.cache_stats().cached_logical_bytes, c.len() as u64);
        let after = s.space_usage().unwrap();
        assert_eq!(after.cache.files, 1);
        assert_eq!(after.cache.logical_bytes, c.len() as u64);
        assert_eq!(after.layers["saves"], before.layers["saves"]);
        p.close(hc).unwrap();

        // The next read fetches from the source again; the layer is intact.
        let reads = src.reads();
        assert_eq!(read_all(&p, "a"), a);
        assert!(src.reads() > reads);
        let l = s.layer("saves").unwrap();
        assert_eq!(read_all(&l, "save1.ess"), body);
        drop((p, l));
        s.close().unwrap();
    }

    #[test]
    fn clear_cache_survives_a_reopen() {
        let d = vfs_testkit::tempdir().unwrap();
        let a = pattern(6 * BS, 4);
        let body = pattern(2 * BS + 3, 5);
        {
            let s = Storage::open(d.path(), small_cfg()).unwrap();
            let p = s.cached(slow(MapSource::with(&[("a", a.clone())])), key());
            assert_eq!(read_all(&p, "a"), a);
            write_layer_file(&s, "content", "c/1", &body);
            drop(p);
            s.close().unwrap();
        }
        {
            let s = Storage::open(d.path(), small_cfg()).unwrap();
            assert_eq!(s.clear_cache().unwrap().files, 1);
            s.close().unwrap();
        }
        let s = Storage::open(d.path(), small_cfg()).unwrap();
        let u = s.space_usage().unwrap();
        assert_eq!(u.cache, Default::default());
        assert_eq!(u.layers["content"].logical_bytes, body.len() as u64);
        assert_eq!(s.cache_stats().cached_logical_bytes, 0);
        let src = slow(MapSource::with(&[("a", a.clone())]));
        let p = s.cached(src.clone(), key());
        assert_eq!(read_all(&p, "a"), a);
        assert!(src.reads() > 0, "fetched again");
    }
}
