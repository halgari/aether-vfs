//! The cache bookkeeping one `Storage` shares between all its cached sources:
//! [`CacheState`], and the `Storage` methods that open, touch, evict from and
//! account for cache files.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use vfs_provider::{CaseMatch, Provider, RootId};

use super::*;

/// Access times not yet committed to the catalog.
pub(super) struct AccessLog {
    pub(super) pending: HashMap<[u8; 16], CacheRec>,
    /// Session-local touch order: the tie-break within one access minute, so
    /// eviction is least-recently-used first even inside a minute.
    pub(super) seq: HashMap<[u8; 16], u64>,
    /// Logical bytes stored per cache file opened this session: loaded from
    /// the catalog row at first open, grown by each block a fetch stores.
    pub(super) logical: HashMap<[u8; 16], u64>,
    pub(super) last_commit_min: u64,
}

/// Cache bookkeeping one [`Storage`] shares between all its cached sources.
pub(crate) struct CacheState {
    /// Open handles per cache file id. Also the lock under which a file is
    /// created in the store (by its first fetch) or evicted.
    pub(super) open_counts: Mutex<HashMap<[u8; 17], usize>>,
    /// In-flight fetches by `(cache file id, the unit's first block, blocks
    /// per unit)`: the fetch's actual geometry, not just a unit index. Two
    /// `CachedSource`s can share one cache file (same `SourceKey`, root,
    /// path, size and mtime) while declaring different `preferred_block`s,
    /// so a unit *index* alone is ambiguous — `blocks 4..8` under a 4-block
    /// unit and `block 1` alone under a 1-block unit are both "unit 1" by
    /// index, but cover different bytes. Keying by the resolved geometry
    /// means two misses only ever join one fetch when they would read
    /// exactly the same span.
    pub(super) inflight: Mutex<HashMap<FetchKey, Fetch>>,
    pub(super) access: Mutex<AccessLog>,
    pub(super) touch_seq: AtomicU64,
    pub(crate) cached_logical: AtomicU64,
    pub(super) ram_hits: AtomicU64,
    pub(super) store_hits: AtomicU64,
    pub(super) misses: AtomicU64,
    pub(super) bytes_from_cache: AtomicU64,
    pub(super) bytes_from_source: AtomicU64,
    /// Readers that joined another reader's fetch instead of starting one.
    pub(crate) coalesced_waits: AtomicU64,
    pub(super) store_write_errors: AtomicU64,
    pub(super) store_read_errors: AtomicU64,
    pub(super) bypassed_opens: AtomicU64,
    /// A background eviction is running (or about to).
    pub(crate) evicting: AtomicBool,
    /// Eviction runs started (a test counter for the back-off).
    pub(crate) eviction_runs: AtomicU64,
    /// Background eviction threads not yet joined.
    pub(crate) evict_threads: Mutex<Vec<JoinHandle<()>>>,
    /// Serialises eviction runs, background or explicit.
    pub(crate) evict_lock: Mutex<()>,
    /// Set to the minute of a run that could not reach its target because the
    /// rest of the cache is open. No background run starts again until a
    /// handle on a cache file is released or a minute has passed.
    pub(crate) stuck_since: Mutex<Option<u64>>,
    /// The "stays over target" warning fired in the current stuck episode
    /// (cleared by a run that reaches its target).
    pub(crate) warned_stuck: AtomicBool,
    /// Test hook: every store read of a cached block fails.
    #[cfg(test)]
    pub(crate) fail_store_reads: AtomicBool,
    /// Test hook: the next `cache_acquire` fails.
    #[cfg(test)]
    pub(crate) fail_acquire: AtomicBool,
    /// Test hook: run by the next eviction right after it has read the
    /// catalog and the access log.
    #[cfg(test)]
    #[allow(clippy::type_complexity)]
    pub(crate) after_snapshot: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl CacheState {
    /// Fresh state; `cached_logical` is the catalog's sum at open.
    pub(crate) fn new(cached_logical: u64) -> Self {
        CacheState {
            open_counts: Mutex::new(HashMap::new()),
            inflight: Mutex::new(HashMap::new()),
            access: Mutex::new(AccessLog {
                pending: HashMap::new(),
                seq: HashMap::new(),
                logical: HashMap::new(),
                last_commit_min: now_minute(),
            }),
            touch_seq: AtomicU64::new(0),
            cached_logical: AtomicU64::new(cached_logical),
            ram_hits: AtomicU64::new(0),
            store_hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            bytes_from_cache: AtomicU64::new(0),
            bytes_from_source: AtomicU64::new(0),
            coalesced_waits: AtomicU64::new(0),
            store_write_errors: AtomicU64::new(0),
            store_read_errors: AtomicU64::new(0),
            bypassed_opens: AtomicU64::new(0),
            evicting: AtomicBool::new(false),
            eviction_runs: AtomicU64::new(0),
            evict_threads: Mutex::new(Vec::new()),
            evict_lock: Mutex::new(()),
            stuck_since: Mutex::new(None),
            warned_stuck: AtomicBool::new(false),
            #[cfg(test)]
            fail_store_reads: AtomicBool::new(false),
            #[cfg(test)]
            fail_acquire: AtomicBool::new(false),
            #[cfg(test)]
            after_snapshot: Mutex::new(None),
        }
    }
}

/// `rel` as a cached file's identity spells it: case-folded only if the
/// source matches names case-insensitively.
pub(super) fn normalize(case: CaseMatch, rel: &str) -> String {
    match case {
        CaseMatch::Insensitive => vfs_core::fold(rel),
        CaseMatch::Sensitive => rel.to_string(),
    }
}

/// The identity hash of a cached file (see the module docs).
pub(super) fn identity(
    key: &SourceKey,
    root: RootId,
    path: &str,
    size: u64,
    version: &[u8],
) -> [u8; 16] {
    let mut h = blake3::Hasher::new();
    let root = root.0.to_le_bytes();
    let size = size.to_le_bytes();
    for field in [key.0.as_bytes(), &root, path.as_bytes(), &size, version] {
        h.update(&(field.len() as u64).to_le_bytes());
        h.update(field);
    }
    let mut out = [0u8; 16];
    out.copy_from_slice(&h.finalize().as_bytes()[..16]);
    out
}

impl Storage {
    /// Wraps `source` in a pull-through cache backed by this storage, if and
    /// only if it declares itself `immutable`, `slow` and positionally
    /// readable. Any other source is returned unchanged: a mutable source would
    /// go stale, and a fast one is better served by the OS page cache.
    pub fn cached(
        self: &Arc<Self>,
        source: Arc<dyn Provider>,
        key: SourceKey,
    ) -> Arc<dyn Provider> {
        let caps = source.capabilities();
        if !(caps.immutable && caps.slow && caps.access >= Access::Read) {
            return source;
        }
        Arc::new(CachedSource {
            unit: unit_blocks(caps.preferred_block, self.block_size()),
            storage: Arc::clone(self),
            inner: source,
            key,
            caps,
            next: AtomicU64::new(1),
            opens: Mutex::new(HashMap::new()),
        })
    }

    /// The byte ranges of `p` that the pull-through cache of `source` under
    /// `key` holds, merged and in order: what [`Storage::cached`]`(source,
    /// key)` would serve without reading `source`. Empty when nothing is
    /// cached, when `source` has no file at `p`, or when `p` is a directory.
    ///
    /// The file is named exactly as a cached open names it — `key`, the root,
    /// the path (folded if `source` is case-insensitive), and `source`'s
    /// `getattr` size and mtime — so `source.getattr` is called once, and a
    /// file whose size or mtime changed reports the (empty) coverage of its
    /// new identity. Read-only: nothing is fetched, touched or counted.
    pub fn cached_coverage(
        &self,
        source: &dyn Provider,
        key: &SourceKey,
        p: VPath,
    ) -> Result<Vec<std::ops::Range<u64>>, StorageError> {
        let st = source.getattr(p).map_err(|status| {
            StorageError::Io(std::io::Error::other(format!(
                "getattr {:?} for cache coverage failed with status {status}",
                p.rel
            )))
        })?;
        let Some(st) = st.filter(|st| st.kind == KIND_FILE) else {
            return Ok(Vec::new());
        };
        let rel = normalize(source.capabilities().case, p.rel);
        let hash = identity(key, p.root, &rel, st.size, &st.mtime.to_le_bytes());
        match self.store.cached_ranges(&cache_file_id(&hash)) {
            Ok(r) => Ok(r),
            Err(vfs_block_store::Error::NotFound) => Ok(Vec::new()),
            Err(e) => Err(e.into()),
        }
    }

    /// The pull-through cache's counters.
    pub fn cache_stats(&self) -> CacheStats {
        let c = &self.cache;
        let ram = self.ram.stats();
        let ram_hits = c.ram_hits.load(Ordering::Relaxed);
        let store_hits = c.store_hits.load(Ordering::Relaxed);
        CacheStats {
            hits: ram_hits + store_hits,
            misses: c.misses.load(Ordering::Relaxed),
            ram_evicts: ram.evicts,
            store_hits,
            bytes_from_cache: c.bytes_from_cache.load(Ordering::Relaxed),
            bytes_from_source: c.bytes_from_source.load(Ordering::Relaxed),
            ram_bytes: ram.bytes,
            cached_logical_bytes: c.cached_logical.load(Ordering::Relaxed),
            store_write_errors: c.store_write_errors.load(Ordering::Relaxed),
            store_read_errors: c.store_read_errors.load(Ordering::Relaxed),
            bypassed_opens: c.bypassed_opens.load(Ordering::Relaxed),
        }
    }

    /// Registers an open handle on cache file `hash` and loads what the store
    /// holds of it into the access log. A file the store lacks is not
    /// created here: its catalog row and store file wait for the first block
    /// a fetch stores ([`Storage::ensure_cache_file`]), so a file opened and
    /// never read leaves nothing behind.
    pub(super) fn cache_acquire(&self, hash: &[u8; 16]) -> Result<(), StorageError> {
        let id = cache_file_id(hash);
        #[cfg(test)]
        if self.cache.fail_acquire.swap(false, Ordering::SeqCst) {
            return Err(StorageError::Io(std::io::Error::other(
                "injected acquire failure",
            )));
        }
        let mut counts = lock(&self.cache.open_counts);
        *counts.entry(id).or_insert(0) += 1;
        let r = (|| {
            let present = self.store.stat(&id)?.is_some();
            if present && lock(&self.cache.access).logical.contains_key(hash) {
                return Ok(());
            }
            let stored = if present {
                self.catalog.cache_get(hash)?.map_or(0, |r| r.logical_bytes)
            } else {
                // A row left from a store file that is gone was counted in
                // the total; it goes, with any batched access for it.
                if let Some(row) = self.catalog.cache_get(hash)? {
                    let held = self.remove_cache_row(hash)?;
                    sub_logical(
                        &self.cache.cached_logical,
                        held.unwrap_or(row.logical_bytes),
                    );
                }
                0
            };
            lock(&self.cache.access).logical.insert(*hash, stored);
            Ok(())
        })();
        if r.is_err() {
            release_count(&mut counts, &id);
        }
        drop(counts);
        if r.is_ok() {
            self.touch(hash, 0, 0);
        }
        r
    }

    /// Creates cache file `hash` of `size` bytes if the store lacks it: its
    /// catalog row first, then the store file (spec §6). Called by a fetch,
    /// under the durability gate, before it stores the file's first block;
    /// the handle it fetches for keeps the file from eviction.
    pub(super) fn ensure_cache_file(&self, hash: &[u8; 16], size: u64) -> Result<(), StorageError> {
        let id = cache_file_id(hash);
        // Once it exists it stays while the handle is open: checked without
        // the lock first, so misses on created files do not queue on it.
        if self.store.stat(&id)?.is_some() {
            return Ok(());
        }
        let _counts = lock(&self.cache.open_counts);
        if self.store.stat(&id)?.is_some() {
            return Ok(());
        }
        let rec = CacheRec {
            last_access_min: now_minute(),
            logical_bytes: 0,
        };
        self.catalog.cache_put_many(&[(*hash, rec)])?;
        Ok(self.store.set_len(&id, size)?)
    }

    /// Drops an open handle on cache file `hash`, recording the access first
    /// (so an eviction that sees the count reach zero also sees this access).
    /// A file becoming evictable ends an eviction back-off.
    pub(super) fn cache_release(&self, hash: &[u8; 16]) {
        self.touch(hash, 0, 0);
        let id = cache_file_id(hash);
        let mut counts = lock(&self.cache.open_counts);
        release_count(&mut counts, &id);
        if !counts.contains_key(&id) {
            *lock(&self.cache.stuck_since) = None;
        }
    }

    /// Open-handle counts per cache file id. Eviction holds this lock while
    /// it checks a file and deletes it, so no open can slip in between.
    pub(crate) fn open_counts(&self) -> MutexGuard<'_, HashMap<[u8; 17], usize>> {
        lock(&self.cache.open_counts)
    }

    /// Records an access to `hash`, adding `stored` newly stored bytes to its
    /// logical size (capped at the file's `size`) and to the cache total;
    /// commits the batch if a minute has passed since the last commit. A
    /// file that holds nothing gets no batched row (it has no row to update:
    /// see [`Storage::cache_acquire`]), only its place in the touch order.
    pub(super) fn touch(&self, hash: &[u8; 16], stored: u64, size: u64) {
        let now = now_minute();
        let seq = self.cache.touch_seq.fetch_add(1, Ordering::Relaxed) + 1;
        let mut a = lock(&self.cache.access);
        let logical = a.logical.entry(*hash).or_insert(0);
        let before = *logical;
        *logical = before.saturating_add(stored).min(size.max(before));
        let now_logical = *logical;
        self.cache
            .cached_logical
            .fetch_add(now_logical - before, Ordering::Relaxed);
        a.seq.insert(*hash, seq);
        if now_logical == 0 {
            return;
        }
        a.pending.insert(
            *hash,
            CacheRec {
                last_access_min: now,
                logical_bytes: now_logical,
            },
        );
        if now > a.last_commit_min {
            if let Err(e) = self.commit_access_locked(&mut a) {
                tracing::warn!(error = %e, "committing cache access times failed; will retry");
            }
        }
    }

    pub(super) fn commit_access_locked(&self, a: &mut AccessLog) -> Result<(), StorageError> {
        if !a.pending.is_empty() {
            let recs: Vec<_> = a.pending.iter().map(|(h, r)| (*h, *r)).collect();
            self.catalog.cache_put_many(&recs)?;
            a.pending.clear();
        }
        a.last_commit_min = now_minute();
        Ok(())
    }

    /// Commits batched access times to the catalog now.
    pub(crate) fn commit_access(&self) -> Result<(), StorageError> {
        self.commit_access_locked(&mut lock(&self.cache.access))
    }

    /// The catalog's cache rows overlaid with the uncommitted access log, the
    /// session touch order, and their logical total. Read under
    /// `open_counts` and `access`, the locks every change to the running
    /// total is made under, so the sum is exactly what the running total
    /// should hold; when it is within `max`, the running total is set to it,
    /// so drift cannot keep starting eviction runs that find nothing to do.
    #[allow(clippy::type_complexity)]
    pub(crate) fn budget_snapshot(
        &self,
        max: u64,
    ) -> Result<(HashMap<[u8; 16], CacheRec>, HashMap<[u8; 16], u64>, u64), StorageError> {
        let _counts = self.open_counts();
        let a = lock(&self.cache.access);
        let mut recs: HashMap<[u8; 16], CacheRec> = self.catalog.cache_all()?.into_iter().collect();
        recs.extend(a.pending.iter().map(|(h, r)| (*h, *r)));
        let total: u64 = recs.values().map(|r| r.logical_bytes).sum();
        if total <= max {
            self.cache.cached_logical.store(total, Ordering::Relaxed);
        }
        Ok((recs, a.seq.clone(), total))
    }

    /// Removes `hash`'s catalog row and forgets its uncommitted access, both
    /// under the `access` lock, so no batched commit can land in between and
    /// bring the row back. Returns the logical bytes the file held in memory
    /// (`None` if it was not opened this session: its row's count is exact).
    /// The caller holds `open_counts`.
    pub(crate) fn remove_cache_row(&self, hash: &[u8; 16]) -> Result<Option<u64>, StorageError> {
        let mut a = lock(&self.cache.access);
        self.catalog.cache_remove(hash)?;
        a.pending.remove(hash);
        a.seq.remove(hash);
        Ok(a.logical.remove(hash))
    }
}

/// Subtracts from the cache total without wrapping.
pub(crate) fn sub_logical(total: &AtomicU64, n: u64) {
    let _ = total.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
        Some(v.saturating_sub(n))
    });
}

pub(super) fn release_count(counts: &mut HashMap<[u8; 17], usize>, id: &[u8; 17]) {
    if let Some(n) = counts.get_mut(id) {
        *n -= 1;
        if *n == 0 {
            counts.remove(id);
        }
    }
}
