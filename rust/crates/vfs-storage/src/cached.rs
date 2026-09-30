//! [`CachedSource`]: pull-through caching of an immutable, slow source into the
//! block store.
//!
//! A cached file's store id is `b'C'` + BLAKE3-128 over, length-prefixed, the
//! [`SourceKey`], the root id, the normalized path, the size and a version tag
//! (the source's `mtime`: the `Provider` trait does not carry a remote
//! `file_id`). A changed file therefore gets a new id and can never be served
//! from the old one's blocks.
//!
//! A read goes RAM tier → block store → source, block by block. A miss
//! fetches the source's whole **fetch unit** — its `preferred_block`, rounded
//! up to whole store blocks, or one block without a hint — in one span of
//! source reads, stores the blocks of it the store lacked, and puts all of
//! them in the RAM tier. Concurrent misses anywhere in one `(file id, unit)`
//! wait on one fetch.
//!
//! Bookkeeping shared by every `CachedSource` of one [`Storage`] lives in
//! [`CacheState`]: open-handle counts (eviction skips a file with a live
//! handle), the batched access times, the logical byte count the budget is
//! checked against, and the counters behind [`CacheStats`].

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::thread::JoinHandle;
use std::time::{SystemTime, UNIX_EPOCH};

use vfs_provider::{
    bad_fh, map_io_err, Access, Capabilities, CaseMatch, DirEntry, Handle, Provider, RootId, Stat,
    VPath, KIND_FILE,
};

use crate::catalog::CacheRec;
use crate::ids::cache_file_id;
use crate::storage::{Storage, StorageError};

/// Names a source stably across runs: a remote source's endpoint, or the
/// `cache_key` a config sets on it. Part of every cached file's identity.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SourceKey(pub String);

/// Counters for the pull-through cache, summed over every cached source of one
/// [`Storage`] since it was opened.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CacheStats {
    /// Blocks served without the source: `ram hits + store_hits`.
    pub hits: u64,
    /// Blocks fetched from a source. Readers that waited on another reader's
    /// fetch of the same block count as neither a hit nor a miss.
    pub misses: u64,
    /// Blocks the RAM tier has evicted (the tier is shared with layers).
    pub ram_evicts: u64,
    /// Blocks served from the block store (decompressed) rather than RAM.
    pub store_hits: u64,
    /// Bytes handed to readers from RAM or the store.
    pub bytes_from_cache: u64,
    /// Bytes fetched from sources.
    pub bytes_from_source: u64,
    /// Bytes resident in the RAM tier (shared with layers).
    pub ram_bytes: u64,
    /// Logical (uncompressed, before dedup) bytes of the blocks cache files
    /// hold in the store: what `cache_max_bytes` is checked against. Opening
    /// a file stores nothing; each block a fetch stores adds its length.
    pub cached_logical_bytes: u64,
    /// Fetched blocks the store failed to write (they were still served).
    pub store_write_errors: u64,
    /// Store reads of cached blocks that failed (a damaged store); the
    /// source served those blocks instead.
    pub store_read_errors: u64,
    /// Opens served straight from the source, uncached, because their cache
    /// file could not be opened.
    pub bypassed_opens: u64,
}

/// One in-flight fetch of a fetch unit: the unit's blocks in order, and
/// whether they all came from the store after all (a fetch that finished just
/// before this one started).
type Fetch = Arc<OnceLock<Result<Unit, i32>>>;

/// A fetched unit's blocks in order, and whether all came from the store.
type Unit = (Arc<[Arc<[u8]>]>, bool);

/// A fetch's geometry: `(cache file id, the unit's first block, blocks per
/// unit)`. See [`CacheState::inflight`] for why an index alone is not enough.
type FetchKey = ([u8; 17], u64, u64);

/// The most bytes one fetch unit may span. A source's `preferred_block`
/// above this is clamped: a unit is one buffer, held whole in memory for the
/// length of a miss.
const MAX_UNIT_BYTES: u64 = 64 << 20;

/// Blocks per fetch unit for a source that prefers `preferred`-byte reads,
/// over a store of `bs`-byte blocks: `preferred` rounded up to whole blocks
/// and clamped to [`MAX_UNIT_BYTES`]. One block when there is no hint, or the
/// hint is no larger than a block.
pub(crate) fn unit_blocks(preferred: Option<u32>, bs: u64) -> u64 {
    match preferred {
        Some(p) if u64::from(p) > bs => u64::from(p).min(MAX_UNIT_BYTES).div_ceil(bs).max(1),
        _ => 1,
    }
}

/// Access times not yet committed to the catalog.
struct AccessLog {
    pending: HashMap<[u8; 16], CacheRec>,
    /// Session-local touch order: the tie-break within one access minute, so
    /// eviction is least-recently-used first even inside a minute.
    seq: HashMap<[u8; 16], u64>,
    /// Logical bytes stored per cache file opened this session: loaded from
    /// the catalog row at first open, grown by each block a fetch stores.
    logical: HashMap<[u8; 16], u64>,
    last_commit_min: u64,
}

/// Cache bookkeeping one [`Storage`] shares between all its cached sources.
pub(crate) struct CacheState {
    /// Open handles per cache file id. Also the lock under which a file is
    /// created in the store (by its first fetch) or evicted.
    open_counts: Mutex<HashMap<[u8; 17], usize>>,
    /// In-flight fetches by `(cache file id, the unit's first block, blocks
    /// per unit)`: the fetch's actual geometry, not just a unit index. Two
    /// `CachedSource`s can share one cache file (same `SourceKey`, root,
    /// path, size and mtime) while declaring different `preferred_block`s,
    /// so a unit *index* alone is ambiguous — `blocks 4..8` under a 4-block
    /// unit and `block 1` alone under a 1-block unit are both "unit 1" by
    /// index, but cover different bytes. Keying by the resolved geometry
    /// means two misses only ever join one fetch when they would read
    /// exactly the same span.
    inflight: Mutex<HashMap<FetchKey, Fetch>>,
    access: Mutex<AccessLog>,
    touch_seq: AtomicU64,
    pub(crate) cached_logical: AtomicU64,
    ram_hits: AtomicU64,
    store_hits: AtomicU64,
    misses: AtomicU64,
    bytes_from_cache: AtomicU64,
    bytes_from_source: AtomicU64,
    /// Readers that joined another reader's fetch instead of starting one.
    pub(crate) coalesced_waits: AtomicU64,
    store_write_errors: AtomicU64,
    store_read_errors: AtomicU64,
    bypassed_opens: AtomicU64,
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

/// Locks `m`, entering a poisoned lock: every critical section here leaves its
/// map consistent at each step, so a panic elsewhere is no reason to stop.
pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub(crate) fn now_minute() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() / 60)
        .unwrap_or(0)
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
fn normalize(case: CaseMatch, rel: &str) -> String {
    match case {
        CaseMatch::Insensitive => vfs_core::fold(rel),
        CaseMatch::Sensitive => rel.to_string(),
    }
}

/// The identity hash of a cached file (see the module docs).
fn identity(key: &SourceKey, root: RootId, path: &str, size: u64, version: &[u8]) -> [u8; 16] {
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
    fn cache_acquire(&self, hash: &[u8; 16]) -> Result<(), StorageError> {
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
    fn ensure_cache_file(&self, hash: &[u8; 16], size: u64) -> Result<(), StorageError> {
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
    fn cache_release(&self, hash: &[u8; 16]) {
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
    fn touch(&self, hash: &[u8; 16], stored: u64, size: u64) {
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

    fn commit_access_locked(&self, a: &mut AccessLog) -> Result<(), StorageError> {
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

fn release_count(counts: &mut HashMap<[u8; 17], usize>, id: &[u8; 17]) {
    if let Some(n) = counts.get_mut(id) {
        *n -= 1;
        if *n == 0 {
            counts.remove(id);
        }
    }
}

struct OpenRec {
    inner: Handle,
    /// `None` for a directory, or a file served uncached: both pass through.
    file: Option<CachedFile>,
}

#[derive(Clone, Copy)]
struct CachedFile {
    hash: [u8; 16],
    id: [u8; 17],
    size: u64,
}

/// A pull-through cache over one immutable, slow source. Built by
/// [`Storage::cached`].
struct CachedSource {
    /// Blocks per fetch unit: see [`unit_blocks`].
    unit: u64,
    storage: Arc<Storage>,
    inner: Arc<dyn Provider>,
    key: SourceKey,
    caps: Capabilities,
    next: AtomicU64,
    opens: Mutex<HashMap<Handle, OpenRec>>,
}

impl CachedSource {
    fn normalize(&self, rel: &str) -> String {
        normalize(self.caps.case, rel)
    }

    /// Block `b` of `f`: RAM tier, else store, else one (coalesced) fetch of
    /// the fetch unit holding it. The flag is true if the block came from RAM
    /// or the store.
    fn block(&self, f: &CachedFile, inner: Handle, b: u64) -> Result<(Arc<[u8]>, bool), i32> {
        let s = &*self.storage;
        if let Some(d) = s.ram.get(&f.id, b) {
            s.cache.ram_hits.fetch_add(1, Ordering::Relaxed);
            return Ok((d, true));
        }
        if let Some(d) = self.read_stored(f, b) {
            return Ok((d, true));
        }
        let u = b / self.unit;
        let first_block = u * self.unit;
        // Keyed by this fetch's actual geometry (first block + unit size),
        // not just the unit index `u`: an index alone collides across
        // `CachedSource`s with different units over the same cache file (see
        // `CacheState::inflight`), which would hand a joiner another unit's
        // bytes under the block index it asked for.
        let key = (f.id, first_block, self.unit);
        let cell = {
            let mut inflight = lock(&s.cache.inflight);
            match inflight.get(&key) {
                Some(c) => {
                    s.cache.coalesced_waits.fetch_add(1, Ordering::SeqCst);
                    Arc::clone(c)
                }
                None => {
                    let c: Fetch = Arc::new(OnceLock::new());
                    inflight.insert(key, Arc::clone(&c));
                    c
                }
            }
        };
        let got = cell.get_or_init(|| self.fetch(f, inner, u)).clone();
        let mut inflight = lock(&s.cache.inflight);
        if inflight.get(&key).is_some_and(|c| Arc::ptr_eq(c, &cell)) {
            inflight.remove(&key);
        }
        drop(inflight);
        let (blocks, hit) = got?;
        let idx = (b - first_block) as usize;
        debug_assert!(
            idx < blocks.len(),
            "block {b} outside the fetch unit starting at {first_block} ({} blocks)",
            blocks.len()
        );
        let d = blocks.get(idx).cloned().ok_or_else(map_io_err)?;
        Ok((d, hit))
    }

    /// Block `b` of `f` from the store, if it holds it. A store that fails
    /// the read (it is damaged) is a miss, not an error: the cache is a copy,
    /// and the source still has the block.
    fn read_stored(&self, f: &CachedFile, b: u64) -> Option<Arc<[u8]>> {
        let s = &*self.storage;
        let bs = s.block_size();
        let len = bs.min(f.size - b * bs) as usize;
        let mut buf = vec![0u8; len];
        #[cfg(test)]
        let r = if s.cache.fail_store_reads.load(Ordering::SeqCst) {
            Err(vfs_block_store::Error::Corrupt(
                "injected read failure".into(),
            ))
        } else {
            s.store.read(&f.id, b * bs, &mut buf)
        };
        #[cfg(not(test))]
        let r = s.store.read(&f.id, b * bs, &mut buf);
        let r = match r {
            Ok(r) => r,
            // Not stored yet: the first fetch creates it.
            Err(vfs_block_store::Error::NotFound) => return None,
            Err(e) => {
                s.cache.store_read_errors.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(
                    error = %e, block = b,
                    "reading a cached block failed; fetching it from the source"
                );
                return None;
            }
        };
        if !r.missing.is_empty() || r.bytes != len {
            return None;
        }
        let d: Arc<[u8]> = buf.into();
        s.cache.store_hits.fetch_add(1, Ordering::Relaxed);
        s.ram.put(&f.id, b, Arc::clone(&d));
        Some(d)
    }

    /// Reads `buf.len()` bytes of `f` at `start` out of the store into `buf`,
    /// and returns the byte ranges (whole blocks) it does not hold: all of
    /// them when the file is not stored yet or the store fails the read (a
    /// damaged store is a miss, as in [`Self::read_stored`]).
    fn stored_span(&self, f: &CachedFile, start: u64, buf: &mut [u8]) -> Vec<std::ops::Range<u64>> {
        let s = &*self.storage;
        let whole = start..start + buf.len() as u64;
        #[cfg(test)]
        let r = if s.cache.fail_store_reads.load(Ordering::SeqCst) {
            Err(vfs_block_store::Error::Corrupt(
                "injected read failure".into(),
            ))
        } else {
            s.store.read(&f.id, start, buf)
        };
        #[cfg(not(test))]
        let r = s.store.read(&f.id, start, buf);
        match r {
            Ok(r) if r.bytes == buf.len() => r.missing,
            Ok(_) | Err(vfs_block_store::Error::NotFound) => vec![whole],
            Err(e) => {
                s.cache.store_read_errors.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(
                    error = %e, offset = start,
                    "reading a cached span failed; fetching it from the source"
                );
                vec![whole]
            }
        }
    }

    /// Fetches fetch unit `u` of `f`: the blocks of it the store does not
    /// hold, from the source in one span (first missing byte to last), then
    /// stores exactly those and returns every block of the unit. Runs once
    /// per concurrent miss on the unit.
    fn fetch(
        &self,
        f: &CachedFile,
        inner: Handle,
        u: u64,
    ) -> Result<Unit, i32> {
        let s = &*self.storage;
        let bs = s.block_size();
        let start = u * self.unit * bs;
        let end = f.size.min(start + self.unit * bs);
        let mut buf = vec![0u8; (end - start) as usize];
        // A fetch that finished between our miss and our joining the
        // in-flight map has stored the unit already; an earlier reader with a
        // smaller unit (or none) may have stored part of it.
        let missing = self.stored_span(f, start, &mut buf);
        let from_store = missing.is_empty();
        if from_store {
            // One count per block, like every other block-granularity
            // counter, so `hits`/`misses` ratios stay meaningful regardless
            // of how large a unit is.
            let blocks_in_unit = (buf.len() as u64).div_ceil(bs);
            s.cache.store_hits.fetch_add(blocks_in_unit, Ordering::Relaxed);
        } else {
            let (lo, hi) = (missing[0].start, missing[missing.len() - 1].end);
            let mut filled = lo;
            while filled < hi {
                let n = self.inner.read_at(
                    inner,
                    filled,
                    &mut buf[(filled - start) as usize..(hi - start) as usize],
                )?;
                if n == 0 {
                    tracing::warn!(
                        offset = filled,
                        size = f.size,
                        "cached source ended before the size it reported at open"
                    );
                    return Err(map_io_err());
                }
                filled += n as u64;
            }
            let blocks: u64 = missing.iter().map(|r| (r.end - r.start).div_ceil(bs)).sum();
            s.cache.misses.fetch_add(blocks, Ordering::Relaxed);
            s.cache
                .bytes_from_source
                .fetch_add(hi - lo, Ordering::Relaxed);
            // Only the missing ranges are written, so a block already stored
            // is neither rewritten nor counted twice. The blocks and the
            // logical bytes a later row commit records for them go in under
            // one shared hold of the durability gate, so a durable commit
            // never counts a block its store flush did not cover.
            let _gate = s.gate_shared();
            let mut stored = 0u64;
            let mut failed = s.ensure_cache_file(&f.hash, f.size).err();
            if failed.is_none() {
                for r in &missing {
                    let bytes = &buf[(r.start - start) as usize..(r.end - start) as usize];
                    if let Err(e) = s.store.write_blocks(&f.id, r.start / bs, bytes) {
                        failed = Some(e.into());
                        break;
                    }
                    stored += r.end - r.start;
                }
            }
            if stored > 0 {
                s.touch(&f.hash, stored, f.size);
            }
            if let Some(e) = failed {
                // The source read succeeded, so a store failure costs only
                // caching: the unit is still served.
                s.cache.store_write_errors.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(error = %e, "writing a fetched unit to the store failed");
            }
        }
        let blocks: Vec<Arc<[u8]>> = buf.chunks(bs as usize).map(Arc::from).collect();
        for (i, d) in blocks.iter().enumerate() {
            s.ram.put(&f.id, u * self.unit + i as u64, Arc::clone(d));
        }
        if !from_store {
            crate::evict::maybe_evict(&self.storage);
        }
        Ok((blocks.into(), from_store))
    }

    fn rec(&self, h: Handle) -> Result<(Handle, Option<CachedFile>), i32> {
        let g = lock(&self.opens);
        let r = g.get(&h).ok_or_else(bad_fh)?;
        Ok((r.inner, r.file))
    }
}

impl Provider for CachedSource {
    fn capabilities(&self) -> Capabilities {
        self.caps.cached()
    }

    fn getattr(&self, p: VPath) -> Result<Option<Stat>, i32> {
        self.inner.getattr(p)
    }

    fn readdir(&self, p: VPath) -> Result<Vec<DirEntry>, i32> {
        self.inner.readdir(p)
    }

    fn open(&self, p: VPath, flags: u32) -> Result<(Handle, u64, bool), i32> {
        let st = self.inner.getattr(p)?;
        let (inner, size, is_dir) = self.inner.open(p, flags)?;
        // The version tag is `getattr`'s mtime, so it only identifies what
        // `open` returned if `getattr` saw the same file; a size that differs
        // says it did not (or the source is inconsistent), and that open is
        // served uncached rather than filed under a mismatched identity.
        let stat = st.filter(|st| st.size == size);
        let file = match stat {
            _ if is_dir => None,
            None => {
                tracing::debug!(
                    path = p.rel,
                    "getattr and open disagree; not caching this open"
                );
                None
            }
            Some(st) => {
                let hash = identity(
                    &self.key,
                    p.root,
                    &self.normalize(p.rel),
                    size,
                    &st.mtime.to_le_bytes(),
                );
                match self.storage.cache_acquire(&hash) {
                    Ok(()) => Some(CachedFile {
                        hash,
                        id: cache_file_id(&hash),
                        size,
                    }),
                    // A damaged cache must not fail an open the source can
                    // serve: this handle passes through, uncached.
                    Err(e) => {
                        self.storage
                            .cache
                            .bypassed_opens
                            .fetch_add(1, Ordering::Relaxed);
                        tracing::warn!(
                            error = %e, path = p.rel,
                            "opening a cache file failed; serving this open uncached"
                        );
                        None
                    }
                }
            }
        };
        let h = self.next.fetch_add(1, Ordering::Relaxed);
        lock(&self.opens).insert(h, OpenRec { inner, file });
        Ok((h, size, is_dir))
    }

    fn close(&self, h: Handle) -> Result<(), i32> {
        let rec = lock(&self.opens).remove(&h).ok_or_else(bad_fh)?;
        if let Some(f) = rec.file {
            self.storage.cache_release(&f.hash);
            crate::evict::maybe_evict(&self.storage);
        }
        self.inner.close(rec.inner)
    }

    fn read_at(&self, h: Handle, offset: u64, buf: &mut [u8]) -> Result<usize, i32> {
        let (inner, file) = self.rec(h)?;
        let Some(f) = file else {
            return self.inner.read_at(inner, offset, buf);
        };
        if offset >= f.size || buf.is_empty() {
            return Ok(0);
        }
        let end = f.size.min(offset + buf.len() as u64);
        let bs = self.storage.block_size();
        let mut off = offset;
        let mut from_cache = 0u64;
        while off < end {
            let b = off / bs;
            let (block, hit) = self.block(&f, inner, b)?;
            let from = (off - b * bs) as usize;
            let take = (end - off).min((block.len() - from) as u64) as usize;
            let dst = (off - offset) as usize;
            buf[dst..dst + take].copy_from_slice(&block[from..from + take]);
            off += take as u64;
            if hit {
                from_cache += take as u64;
            }
        }
        self.storage
            .cache
            .bytes_from_cache
            .fetch_add(from_cache, Ordering::Relaxed);
        Ok((end - offset) as usize)
    }
}

impl Drop for CachedSource {
    /// Handles left open would pin their files against eviction for the rest
    /// of the session; release them.
    fn drop(&mut self) {
        let opens = std::mem::take(&mut *lock(&self.opens));
        for (_, rec) in opens {
            if let Some(f) = rec.file {
                self.storage.cache_release(&f.hash);
            }
            let _ = self.inner.close(rec.inner);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Barrier, Condvar, Mutex};

    use vfs_block_store::StoreConfig;
    use vfs_provider::{
        bad_fh, map_io_err, not_found, Access, Capabilities, CaseMatch, DirEntry, Handle, Provider,
        RootId, Stat, VPath, KIND_DIR, KIND_FILE, OPEN_READ,
    };

    use super::*;
    use crate::config::StorageConfig;
    use crate::ids::layer_file_id;
    use crate::storage::Storage;

    const BS: usize = 4096;

    fn small_cfg() -> StorageConfig {
        StorageConfig {
            store: StoreConfig {
                block_size: BS as u32,
                ..StoreConfig::default()
            },
            ..StorageConfig::default()
        }
    }

    fn temp_storage() -> (Arc<Storage>, tempfile::TempDir) {
        temp_storage_with(small_cfg())
    }

    fn temp_storage_with(cfg: StorageConfig) -> (Arc<Storage>, tempfile::TempDir) {
        let d = tempfile::tempdir().unwrap();
        (Storage::open(d.path(), cfg).unwrap(), d)
    }

    /// A gate a source's `read_at` waits on until the test opens it.
    #[derive(Default)]
    struct Gate {
        open: Mutex<bool>,
        cv: Condvar,
    }

    impl Gate {
        fn wait(&self) {
            let mut g = self.open.lock().unwrap();
            while !*g {
                g = self.cv.wait(g).unwrap();
            }
        }
        fn release(&self) {
            *self.open.lock().unwrap() = true;
            self.cv.notify_all();
        }
    }

    /// Wraps any provider, declares it immutable and slow, and counts
    /// `read_at` calls. `max_read` caps each read to force short reads.
    struct Slow {
        inner: Arc<dyn Provider>,
        reads: AtomicU64,
        max_read: usize,
        gate: Option<Arc<Gate>>,
        /// Declared as `preferred_block`: `CachedSource`'s fetch unit.
        preferred_block: Option<u32>,
    }

    impl Slow {
        fn reads(&self) -> u64 {
            self.reads.load(Ordering::SeqCst)
        }
    }

    impl Provider for Slow {
        fn capabilities(&self) -> Capabilities {
            Capabilities {
                access: Access::Read,
                immutable: true,
                slow: true,
                preferred_block: self.preferred_block,
                case: self.inner.capabilities().case,
            }
        }
        fn getattr(&self, p: VPath) -> Result<Option<Stat>, i32> {
            self.inner.getattr(p)
        }
        fn readdir(&self, p: VPath) -> Result<Vec<DirEntry>, i32> {
            self.inner.readdir(p)
        }
        fn open(&self, p: VPath, flags: u32) -> Result<(Handle, u64, bool), i32> {
            self.inner.open(p, flags)
        }
        fn close(&self, h: Handle) -> Result<(), i32> {
            self.inner.close(h)
        }
        fn read_at(&self, h: Handle, offset: u64, buf: &mut [u8]) -> Result<usize, i32> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            if let Some(g) = &self.gate {
                g.wait();
            }
            let n = buf.len().min(self.max_read);
            self.inner.read_at(h, offset, &mut buf[..n])
        }
    }

    fn slow(inner: Arc<dyn Provider>) -> Arc<Slow> {
        Arc::new(Slow {
            inner,
            reads: AtomicU64::new(0),
            max_read: usize::MAX,
            gate: None,
            preferred_block: None,
        })
    }

    /// [`slow`], declaring a `preferred_block` of `unit` bytes.
    fn hinted(inner: Arc<dyn Provider>, unit: usize) -> Arc<Slow> {
        Arc::new(Slow {
            inner,
            reads: AtomicU64::new(0),
            max_read: usize::MAX,
            gate: None,
            preferred_block: Some(unit as u32),
        })
    }

    fn slow_fixture() -> Arc<Slow> {
        slow(Arc::new(vfs_provider::conformance::MemFixture::new()))
    }

    /// A flat, root-blind, mutable map of files: `(body, mtime)`.
    #[derive(Default)]
    struct MapSource {
        files: Mutex<HashMap<String, (Vec<u8>, i64)>>,
        next: AtomicU64,
        opens: Mutex<HashMap<Handle, Vec<u8>>>,
    }

    impl MapSource {
        fn with(files: &[(&str, Vec<u8>)]) -> Arc<Self> {
            let s = MapSource::default();
            for (n, b) in files {
                s.set(n, b.clone(), 1);
            }
            Arc::new(s)
        }
        fn set(&self, name: &str, body: Vec<u8>, mtime: i64) {
            self.files
                .lock()
                .unwrap()
                .insert(name.to_string(), (body, mtime));
        }
    }

    impl Provider for MapSource {
        fn capabilities(&self) -> Capabilities {
            Capabilities {
                case: CaseMatch::Sensitive,
                ..Capabilities::read_only()
            }
        }
        fn getattr(&self, p: VPath) -> Result<Option<Stat>, i32> {
            if p.rel.is_empty() {
                return Ok(Some(Stat {
                    kind: KIND_DIR,
                    size: 0,
                    mtime: 0,
                }));
            }
            Ok(self.files.lock().unwrap().get(p.rel).map(|(b, m)| Stat {
                kind: KIND_FILE,
                size: b.len() as u64,
                mtime: *m,
            }))
        }
        fn readdir(&self, _p: VPath) -> Result<Vec<DirEntry>, i32> {
            Ok(Vec::new())
        }
        fn open(&self, p: VPath, _flags: u32) -> Result<(Handle, u64, bool), i32> {
            let body = self
                .files
                .lock()
                .unwrap()
                .get(p.rel)
                .ok_or_else(not_found)?
                .0
                .clone();
            let size = body.len() as u64;
            let h = self.next.fetch_add(1, Ordering::SeqCst) + 1;
            self.opens.lock().map_err(|_| map_io_err())?.insert(h, body);
            Ok((h, size, false))
        }
        fn close(&self, h: Handle) -> Result<(), i32> {
            self.opens.lock().unwrap().remove(&h);
            Ok(())
        }
        fn read_at(&self, h: Handle, offset: u64, buf: &mut [u8]) -> Result<usize, i32> {
            let g = self.opens.lock().unwrap();
            let body = g.get(&h).ok_or_else(bad_fh)?;
            let start = (offset as usize).min(body.len());
            let n = (body.len() - start).min(buf.len());
            buf[..n].copy_from_slice(&body[start..start + n]);
            Ok(n)
        }
    }

    fn pattern(len: usize, seed: u8) -> Vec<u8> {
        (0..len)
            .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
            .collect()
    }

    fn read_all_at(p: &Arc<dyn Provider>, root: RootId, rel: &str) -> Vec<u8> {
        let (h, size, _) = p.open(VPath::new(root, rel), OPEN_READ).unwrap();
        let mut out = Vec::new();
        let mut buf = vec![0u8; 1000]; // not block-aligned on purpose
        loop {
            let n = p.read_at(h, out.len() as u64, &mut buf).unwrap();
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
        p.close(h).unwrap();
        assert_eq!(out.len() as u64, size);
        out
    }

    fn read_all(p: &Arc<dyn Provider>, rel: &str) -> Vec<u8> {
        read_all_at(p, RootId::DEFAULT, rel)
    }

    fn key() -> SourceKey {
        SourceKey("k".into())
    }

    #[test]
    fn conformance_through_the_cache() {
        let (s, _d) = temp_storage();
        vfs_provider::assert_conformance(s.cached(slow_fixture(), key()));
    }

    #[test]
    fn second_read_is_served_from_the_store() {
        let (s, _d) = temp_storage();
        let src = slow_fixture();
        let p = s.cached(src.clone(), key());
        assert_eq!(read_all(&p, "a.txt"), b"hello");
        let before = src.reads();
        assert!(before > 0);
        assert_eq!(read_all(&p, "a.txt"), b"hello");
        assert_eq!(src.reads(), before, "no source read on the second pass");
    }

    #[test]
    fn multi_block_file_with_short_reads_round_trips() {
        let (s, _d) = temp_storage();
        let body = pattern(3 * BS + 1234, 7);
        let map = MapSource::with(&[("big.bin", body.clone())]);
        let src = Arc::new(Slow {
            inner: map,
            reads: AtomicU64::new(0),
            max_read: 1000, // every block needs several source reads
            gate: None,
            preferred_block: None,
        });
        let p = s.cached(src.clone(), key());
        assert_eq!(read_all(&p, "big.bin"), body);
        let before = src.reads();

        // Reads that straddle block boundaries, served from RAM and store.
        let (h, _, _) = p.open(VPath::at_default("big.bin"), OPEN_READ).unwrap();
        let mut buf = vec![0u8; BS + 100];
        let n = p.read_at(h, BS as u64 - 50, &mut buf).unwrap();
        assert_eq!(&buf[..n], &body[BS - 50..BS - 50 + n]);
        assert!(n > 0);
        let n = p.read_at(h, (3 * BS + 1000) as u64, &mut buf).unwrap();
        assert_eq!(n, 234, "clamped at end of file");
        assert_eq!(&buf[..n], &body[3 * BS + 1000..]);
        assert_eq!(p.read_at(h, body.len() as u64 + 5, &mut buf).unwrap(), 0);
        p.close(h).unwrap();
        assert_eq!(src.reads(), before);

        let st = s.cache_stats();
        assert_eq!(st.misses, 4, "four blocks fetched once each");
        assert_eq!(st.bytes_from_source, body.len() as u64);
        assert!(st.hits > 0);
        assert!(st.bytes_from_cache > 0);
        assert_eq!(st.cached_logical_bytes, body.len() as u64);
        assert_eq!(st.store_write_errors, 0);
    }

    #[test]
    fn only_stored_blocks_count_against_the_budget() {
        let d = tempfile::tempdir().unwrap();
        let src = slow(MapSource::with(&[("big.bin", pattern(10 * BS, 5))]));
        {
            let s = Storage::open(d.path(), small_cfg()).unwrap();
            let p = s.cached(src.clone(), key());
            let (h, size, _) = p.open(VPath::at_default("big.bin"), OPEN_READ).unwrap();
            assert_eq!(size, 10 * BS as u64);
            assert_eq!(
                s.cache_stats().cached_logical_bytes,
                0,
                "opening stores nothing"
            );
            let mut probe = [0u8; 64];
            p.read_at(h, 0, &mut probe).unwrap();
            assert_eq!(
                s.cache_stats().cached_logical_bytes,
                BS as u64,
                "a header probe counts one block, not the whole file"
            );
            p.read_at(h, 0, &mut probe).unwrap();
            assert_eq!(s.cache_stats().cached_logical_bytes, BS as u64);
            p.close(h).unwrap();
            drop(p);
            s.close().unwrap();
        }
        let s = Storage::open(d.path(), small_cfg()).unwrap();
        assert_eq!(s.cache_stats().cached_logical_bytes, BS as u64);
        let rows = s.catalog.cache_all().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].1.logical_bytes, BS as u64);
        // A second block read after the reopen adds to the persisted count.
        let p = s.cached(src.clone(), key());
        let (h, _, _) = p.open(VPath::at_default("big.bin"), OPEN_READ).unwrap();
        p.read_at(h, BS as u64, &mut [0u8; 8]).unwrap();
        assert_eq!(s.cache_stats().cached_logical_bytes, 2 * BS as u64);
        p.close(h).unwrap();
    }

    #[test]
    fn eviction_backs_off_while_open_files_hold_the_cache_over_budget() {
        let (s, _d) = temp_storage_with(StorageConfig {
            cache_max_bytes: 3 * BS as u64,
            ..small_cfg()
        });
        let src = slow(MapSource::with(&[("big.bin", pattern(40 * BS, 9))]));
        let p = s.cached(src.clone(), key());
        let (h, _, _) = p.open(VPath::at_default("big.bin"), OPEN_READ).unwrap();
        let mut buf = vec![0u8; BS];
        for b in 0..40u64 {
            assert_eq!(p.read_at(h, b * BS as u64, &mut buf).unwrap(), BS);
            // Each background run finishes before the next miss, so a missing
            // back-off would show up as one run per miss past the budget.
            s.wait_for_eviction();
        }
        let runs = s.cache.eviction_runs.load(Ordering::SeqCst);
        // One run gets stuck on the open file; a second only if the clock's
        // minute turned during the loop.
        assert!((1..=2).contains(&runs), "eviction ran {runs} times");
        assert_eq!(s.cache_stats().cached_logical_bytes, 40 * BS as u64);

        // Closing the file makes it evictable, which ends the back-off.
        p.close(h).unwrap();
        s.wait_for_eviction();
        assert!(s.cache_stats().cached_logical_bytes <= 3 * BS as u64 * 9 / 10);
    }

    /// A source whose `getattr` size disagrees with what `open` returns.
    struct Liar(Arc<MapSource>);

    impl Provider for Liar {
        fn capabilities(&self) -> Capabilities {
            Capabilities {
                immutable: true,
                slow: true,
                ..self.0.capabilities()
            }
        }
        fn getattr(&self, p: VPath) -> Result<Option<Stat>, i32> {
            Ok(self.0.getattr(p)?.map(|st| Stat {
                size: st.size + 1,
                ..st
            }))
        }
        fn readdir(&self, p: VPath) -> Result<Vec<DirEntry>, i32> {
            self.0.readdir(p)
        }
        fn open(&self, p: VPath, flags: u32) -> Result<(Handle, u64, bool), i32> {
            self.0.open(p, flags)
        }
        fn close(&self, h: Handle) -> Result<(), i32> {
            self.0.close(h)
        }
        fn read_at(&self, h: Handle, offset: u64, buf: &mut [u8]) -> Result<usize, i32> {
            self.0.read_at(h, offset, buf)
        }
    }

    #[test]
    fn a_size_mismatch_between_getattr_and_open_is_served_uncached() {
        let (s, _d) = temp_storage();
        let src = slow(Arc::new(Liar(MapSource::with(&[(
            "a.txt",
            b"hello".to_vec(),
        )]))));
        let p = s.cached(src.clone(), key());
        assert_eq!(read_all(&p, "a.txt"), b"hello");
        let first = src.reads();
        assert_eq!(read_all(&p, "a.txt"), b"hello");
        assert!(src.reads() > first, "not cached, so read from the source");
        assert!(s.catalog.cache_all().unwrap().is_empty());
        assert_eq!(s.cache_stats().cached_logical_bytes, 0);
    }

    #[test]
    fn store_serves_after_the_ram_tier_is_off() {
        let (s, _d) = temp_storage_with(StorageConfig {
            ram_tier_bytes: 0,
            ..small_cfg()
        });
        let src = slow_fixture();
        let p = s.cached(src.clone(), key());
        read_all(&p, "sub/b.txt");
        let before = src.reads();
        assert_eq!(read_all(&p, "sub/b.txt"), b"world!");
        assert_eq!(src.reads(), before);
        assert!(s.cache_stats().store_hits > 0);
    }

    #[test]
    fn survives_reopen() {
        let d = tempfile::tempdir().unwrap();
        let src = slow_fixture();
        {
            let s = Storage::open(d.path(), small_cfg()).unwrap();
            let p = s.cached(src.clone(), key());
            assert_eq!(read_all(&p, "a.txt"), b"hello");
            drop(p);
            s.close().unwrap();
        }
        let before = src.reads();
        let s = Storage::open(d.path(), small_cfg()).unwrap();
        assert_eq!(s.cache_stats().cached_logical_bytes, 5);
        let rows = s.catalog.cache_all().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].1.logical_bytes, 5);
        let p = s.cached(src.clone(), key());
        assert_eq!(read_all(&p, "a.txt"), b"hello");
        assert_eq!(src.reads(), before, "zero source reads after a reopen");
    }

    /// A file opened and closed without a read stores nothing: no catalog
    /// row, no store file. Its first stored block creates both.
    #[test]
    fn opening_without_reading_stores_nothing() {
        let (s, _d) = temp_storage();
        let src = slow(MapSource::with(&[("a", pattern(3 * BS, 1))]));
        let p = s.cached(src, key());
        let (h, _, _) = p.open(VPath::at_default("a"), OPEN_READ).unwrap();
        p.close(h).unwrap();
        s.commit_access().unwrap();
        assert!(s.catalog.cache_all().unwrap().is_empty());
        assert!(s.store.file_ids().unwrap().is_empty());

        let (h, _, _) = p.open(VPath::at_default("a"), OPEN_READ).unwrap();
        let mut buf = [0u8; 10];
        p.read_at(h, BS as u64, &mut buf).unwrap();
        assert_eq!(buf[..], pattern(3 * BS, 1)[BS..BS + 10]);
        p.close(h).unwrap();
        s.commit_access().unwrap();
        let rows = s.catalog.cache_all().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].1.logical_bytes, BS as u64);
        assert_eq!(s.store.file_ids().unwrap().len(), 1);
        assert_eq!(s.cache_stats().cached_logical_bytes, BS as u64);
    }

    /// A damaged cache is bypassed, not fatal: a store read that fails, or a
    /// cache file that cannot be opened, is served by the (healthy) source
    /// and counted.
    #[test]
    fn a_damaged_cache_falls_back_to_the_source() {
        let (s, _d) = temp_storage_with(StorageConfig {
            ram_tier_bytes: 0, // every hit goes to the store
            ..small_cfg()
        });
        let body = pattern(3 * BS, 7);
        let src = slow(MapSource::with(&[("a", body.clone())]));
        let p = s.cached(src.clone(), key());
        assert_eq!(read_all(&p, "a"), body);

        s.cache.fail_store_reads.store(true, Ordering::SeqCst);
        let before = src.reads();
        assert_eq!(read_all(&p, "a"), body);
        assert!(src.reads() > before, "the source served the reads");
        assert!(
            s.cache_stats().store_read_errors >= 3,
            "{:?}",
            s.cache_stats()
        );
        s.cache.fail_store_reads.store(false, Ordering::SeqCst);

        s.cache.fail_acquire.store(true, Ordering::SeqCst);
        let before = src.reads();
        assert_eq!(read_all(&p, "a"), body);
        assert!(src.reads() > before, "served uncached");
        assert_eq!(s.cache_stats().bypassed_opens, 1);
    }

    #[test]
    fn fast_or_mutable_sources_are_not_wrapped() {
        let (s, _d) = temp_storage();
        let fast: Arc<dyn Provider> = Arc::new(vfs_provider::conformance::MemFixture::new());
        assert!(Arc::ptr_eq(&s.cached(fast.clone(), key()), &fast));

        struct Caps(Capabilities);
        impl Provider for Caps {
            fn capabilities(&self) -> Capabilities {
                self.0
            }
            fn getattr(&self, _p: VPath) -> Result<Option<Stat>, i32> {
                Ok(None)
            }
            fn readdir(&self, _p: VPath) -> Result<Vec<DirEntry>, i32> {
                Ok(Vec::new())
            }
            fn open(&self, _p: VPath, _f: u32) -> Result<(Handle, u64, bool), i32> {
                Err(not_found())
            }
            fn close(&self, _h: Handle) -> Result<(), i32> {
                Ok(())
            }
        }
        let base = Capabilities::read_only();
        for c in [
            Capabilities {
                immutable: true,
                ..base
            },
            Capabilities { slow: true, ..base },
            Capabilities {
                access: Access::SeqRead,
                immutable: true,
                slow: true,
                ..base
            },
        ] {
            let p: Arc<dyn Provider> = Arc::new(Caps(c));
            assert!(Arc::ptr_eq(&s.cached(p.clone(), key()), &p), "{c:?}");
        }
        let both: Arc<dyn Provider> = Arc::new(Caps(Capabilities {
            immutable: true,
            slow: true,
            ..base
        }));
        let wrapped = s.cached(both.clone(), key());
        assert!(!Arc::ptr_eq(&wrapped, &both));
        assert!(!wrapped.capabilities().slow, "the cache answers `slow`");
    }

    #[test]
    fn version_change_is_a_new_cache_file() {
        let (s, _d) = temp_storage();
        let map = MapSource::with(&[("a.txt", b"hello".to_vec())]);
        let p = s.cached(slow(map.clone()), key());
        assert_eq!(read_all(&p, "a.txt"), b"hello");

        map.set("a.txt", b"hello!!".to_vec(), 1); // new size
        assert_eq!(read_all(&p, "a.txt"), b"hello!!");

        map.set("a.txt", b"HELLO!!".to_vec(), 2); // same size, new mtime
        assert_eq!(read_all(&p, "a.txt"), b"HELLO!!");
        assert_eq!(s.catalog.cache_all().unwrap().len(), 3);
    }

    #[test]
    fn the_root_and_the_source_key_are_part_of_the_identity() {
        let (s, _d) = temp_storage();
        let src = slow(MapSource::with(&[("a.txt", b"hello".to_vec())]));
        let p = s.cached(src.clone(), key());
        read_all_at(&p, RootId(0), "a.txt");
        let one = src.reads();
        read_all_at(&p, RootId(1), "a.txt");
        assert!(src.reads() > one, "another root is another file");
        let two = src.reads();
        let q = s.cached(src.clone(), SourceKey("other".into()));
        read_all_at(&q, RootId(0), "a.txt");
        assert!(src.reads() > two, "another source key is another file");
        assert_eq!(s.catalog.cache_all().unwrap().len(), 3);
    }

    #[test]
    fn concurrent_misses_fetch_once() {
        const READERS: usize = 8;
        let (s, _d) = temp_storage();
        let body = pattern(BS, 3);
        let gate = Arc::new(Gate::default());
        let src = Arc::new(Slow {
            inner: MapSource::with(&[("f", body.clone())]),
            reads: AtomicU64::new(0),
            max_read: usize::MAX,
            gate: Some(gate.clone()),
            preferred_block: None,
        });
        let p = s.cached(src.clone(), key());
        let barrier = Arc::new(Barrier::new(READERS));
        let threads: Vec<_> = (0..READERS)
            .map(|_| {
                let (p, barrier) = (p.clone(), barrier.clone());
                std::thread::spawn(move || {
                    let (h, _, _) = p.open(VPath::at_default("f"), OPEN_READ).unwrap();
                    barrier.wait();
                    let mut buf = vec![0u8; BS];
                    let n = p.read_at(h, 0, &mut buf).unwrap();
                    p.close(h).unwrap();
                    buf.truncate(n);
                    buf
                })
            })
            .collect();
        // Released only once one reader is inside the source and the other
        // seven have joined its fetch: no timing, only these two conditions.
        while src.reads() < 1 || s.cache.coalesced_waits.load(Ordering::SeqCst) < 7 {
            std::thread::yield_now();
        }
        gate.release();
        for t in threads {
            assert_eq!(t.join().unwrap(), body);
        }
        assert_eq!(
            src.reads(),
            1,
            "one source fetch for eight concurrent misses"
        );
        assert_eq!(s.cache_stats().misses, 1);
    }

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
    fn unit_blocks_rounds_the_hint_up_to_whole_blocks_and_clamps_it() {
        assert_eq!(unit_blocks(None, 4096), 1);
        assert_eq!(unit_blocks(Some(1000), 4096), 1, "a hint below a block is one block");
        assert_eq!(unit_blocks(Some(4096), 4096), 1);
        assert_eq!(unit_blocks(Some(4 * 4096), 4096), 4);
        assert_eq!(unit_blocks(Some(4 * 4096 + 1), 4096), 5, "rounded up, never down");
        assert_eq!(unit_blocks(Some(4 << 20), 64 << 10), 64, "a 4 MiB frame over 64 KiB blocks");
        assert_eq!(unit_blocks(Some(u32::MAX), 64 << 10), 1024, "clamped to 64 MiB");
    }

    #[test]
    fn conformance_through_a_hinted_cache() {
        let (s, _d) = temp_storage();
        let src = hinted(Arc::new(vfs_provider::conformance::MemFixture::new()), 4 * BS);
        vfs_provider::assert_conformance(s.cached(src, key()));
    }

    /// One miss fills the whole unit the source prefers, in one source read,
    /// and every block of it is then served without the source — including
    /// the short tail unit at the end of the file.
    #[test]
    fn a_miss_fetches_the_whole_preferred_unit_in_one_source_read() {
        let (s, _d) = temp_storage();
        let body = pattern(10 * BS + 100, 4);
        let src = hinted(MapSource::with(&[("f", body.clone())]), 4 * BS);
        let p = s.cached(src.clone(), key());
        let (h, _, _) = p.open(VPath::at_default("f"), OPEN_READ).unwrap();
        let mut buf = [0u8; 10];

        p.read_at(h, BS as u64 + 5, &mut buf).unwrap();
        assert_eq!(buf[..], body[BS + 5..BS + 15]);
        assert_eq!(src.reads(), 1, "one source read for the unit");
        let st = s.cache_stats();
        assert_eq!(st.misses, 4, "four blocks fetched");
        assert_eq!(st.bytes_from_source, 4 * BS as u64);
        assert_eq!(st.cached_logical_bytes, 4 * BS as u64, "all four stored");

        for b in [0u64, 2, 3] {
            p.read_at(h, b * BS as u64, &mut buf).unwrap();
            assert_eq!(buf[..], body[b as usize * BS..b as usize * BS + 10]);
        }
        assert_eq!(src.reads(), 1, "the rest of the unit came from the cache");

        p.read_at(h, 4 * BS as u64, &mut buf).unwrap();
        assert_eq!(src.reads(), 2, "the next unit is its own fetch");
        let mut tail = [0u8; 100];
        assert_eq!(p.read_at(h, 10 * BS as u64, &mut tail).unwrap(), 100);
        assert_eq!(tail[..], body[10 * BS..]);
        assert_eq!(src.reads(), 3, "the tail unit: blocks 8, 9 and the short 10");
        assert_eq!(s.cache_stats().cached_logical_bytes, 8 * BS as u64 + 2 * BS as u64 + 100);
        p.close(h).unwrap();
        assert_eq!(read_all(&p, "f"), body);
        assert_eq!(src.reads(), 3, "the whole file is cached");
    }

    /// A unit partly stored by an earlier reader (here one with no hint) is
    /// completed with one source read spanning its missing blocks, and a
    /// block stored twice is counted once against the budget.
    #[test]
    fn a_partly_stored_unit_fetches_only_its_missing_span_and_counts_each_block_once() {
        let (s, _d) = temp_storage();
        let body = pattern(4 * BS, 6);
        let map = MapSource::with(&[("f", body.clone())]);
        let plain = s.cached(slow(map.clone()), key());
        let (h, _, _) = plain.open(VPath::at_default("f"), OPEN_READ).unwrap();
        plain.read_at(h, 0, &mut [0u8; 8]).unwrap();
        plain.read_at(h, BS as u64, &mut [0u8; 8]).unwrap();
        plain.close(h).unwrap();
        assert_eq!(s.cache_stats().cached_logical_bytes, 2 * BS as u64);
        let before = s.cache_stats().bytes_from_source;

        // Same key and path, so the same cache file; a 4-block unit.
        let src = hinted(map, 4 * BS);
        let p = s.cached(src.clone(), key());
        let (h, _, _) = p.open(VPath::at_default("f"), OPEN_READ).unwrap();
        let mut buf = [0u8; 10];
        p.read_at(h, 3 * BS as u64, &mut buf).unwrap();
        assert_eq!(buf[..], body[3 * BS..3 * BS + 10]);
        assert_eq!(src.reads(), 1);
        assert_eq!(
            s.cache_stats().bytes_from_source - before,
            2 * BS as u64,
            "only blocks 2 and 3 were read from the source"
        );
        assert_eq!(s.cache_stats().cached_logical_bytes, 4 * BS as u64);
        p.close(h).unwrap();
        assert_eq!(read_all(&p, "f"), body);
        assert_eq!(src.reads(), 1);
    }

    /// Readers of *different* blocks of one unit share one fetch.
    #[test]
    fn concurrent_misses_on_one_unit_fetch_once() {
        const READERS: usize = 8;
        let (s, _d) = temp_storage();
        let body = pattern(READERS * BS, 8);
        let gate = Arc::new(Gate::default());
        let src = Arc::new(Slow {
            inner: MapSource::with(&[("f", body.clone())]),
            reads: AtomicU64::new(0),
            max_read: usize::MAX,
            gate: Some(gate.clone()),
            preferred_block: Some((READERS * BS) as u32),
        });
        let p = s.cached(src.clone(), key());
        let barrier = Arc::new(Barrier::new(READERS));
        let threads: Vec<_> = (0..READERS)
            .map(|i| {
                let (p, barrier) = (p.clone(), barrier.clone());
                std::thread::spawn(move || {
                    let (h, _, _) = p.open(VPath::at_default("f"), OPEN_READ).unwrap();
                    barrier.wait();
                    let mut buf = vec![0u8; BS];
                    let n = p.read_at(h, (i * BS) as u64, &mut buf).unwrap();
                    p.close(h).unwrap();
                    buf.truncate(n);
                    (i, buf)
                })
            })
            .collect();
        while src.reads() < 1 || s.cache.coalesced_waits.load(Ordering::SeqCst) < 7 {
            std::thread::yield_now();
        }
        gate.release();
        for t in threads {
            let (i, got) = t.join().unwrap();
            assert_eq!(got, body[i * BS..(i + 1) * BS]);
        }
        assert_eq!(src.reads(), 1, "one source fetch for eight blocks of one unit");
        assert_eq!(s.cache_stats().misses, READERS as u64);
    }

    /// Two `CachedSource`s over one `Storage` and cache file (same
    /// `SourceKey`, path, size and mtime — see `identity`) but different
    /// `preferred_block` units must never cross wires, even though their
    /// naive unit index `b / unit` can coincide: X's 4-block unit puts block
    /// 5 in "unit 1" (blocks 4..8); Y has no hint (a 1-block unit), and its
    /// miss on block 1 is also "unit 1" by index alone. Racing them exercises
    /// the in-flight map keyed by actual fetch geometry, not just that index.
    #[test]
    fn concurrent_misses_with_different_units_never_cross_wires() {
        let (s, _d) = temp_storage();
        let body = pattern(8 * BS, 9);
        let map = MapSource::with(&[("f", body.clone())]);
        let gate = Arc::new(Gate::default());
        let src_x = Arc::new(Slow {
            inner: map.clone(),
            reads: AtomicU64::new(0),
            max_read: usize::MAX,
            gate: Some(gate.clone()),
            preferred_block: Some((4 * BS) as u32),
        });
        let src_y = Arc::new(Slow {
            inner: map,
            reads: AtomicU64::new(0),
            max_read: usize::MAX,
            gate: Some(gate.clone()),
            preferred_block: None,
        });
        let x = s.cached(src_x.clone(), key());
        let y = s.cached(src_y.clone(), key());
        let (hx, _, _) = x.open(VPath::at_default("f"), OPEN_READ).unwrap();
        let (hy, _, _) = y.open(VPath::at_default("f"), OPEN_READ).unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let tx = {
            let (x, barrier) = (x.clone(), barrier.clone());
            std::thread::spawn(move || {
                barrier.wait();
                let mut buf = vec![0u8; BS];
                x.read_at(hx, 5 * BS as u64, &mut buf).unwrap();
                x.close(hx).unwrap();
                buf
            })
        };
        let ty = {
            let (y, barrier) = (y.clone(), barrier.clone());
            std::thread::spawn(move || {
                barrier.wait();
                let mut buf = vec![0u8; BS];
                y.read_at(hy, BS as u64, &mut buf).unwrap();
                y.close(hy).unwrap();
                buf
            })
        };
        // Released only once both misses are inside their source, so the
        // in-flight map holds both cells at once.
        while src_x.reads() < 1 || src_y.reads() < 1 {
            std::thread::yield_now();
        }
        gate.release();
        let got_x = tx.join().unwrap();
        let got_y = ty.join().unwrap();
        assert_eq!(got_x[..], body[5 * BS..6 * BS], "X got its own block 5");
        assert_eq!(got_y[..], body[BS..2 * BS], "Y got its own block 1, not X's block 4");
        assert_eq!(src_x.reads(), 1);
        assert_eq!(src_y.reads(), 1);
    }

    #[test]
    fn cached_coverage_reports_what_the_cache_holds_of_a_file() {
        let (s, _d) = temp_storage();
        let body = pattern(3 * BS + 10, 2);
        let src = slow(MapSource::with(&[("f", body.clone())]));
        let p = s.cached(src.clone(), key());
        let at = VPath::at_default("f");
        assert!(s.cached_coverage(&*src, &key(), at).unwrap().is_empty(), "nothing yet");

        let (h, _, _) = p.open(at, OPEN_READ).unwrap();
        p.read_at(h, BS as u64 + 1, &mut [0u8; 4]).unwrap();
        p.close(h).unwrap();
        assert_eq!(
            s.cached_coverage(&*src, &key(), at).unwrap(),
            vec![BS as u64..2 * BS as u64]
        );

        read_all(&p, "f");
        assert_eq!(
            s.cached_coverage(&*src, &key(), at).unwrap(),
            vec![0..body.len() as u64],
            "merged into one range, the short tail block included"
        );
        let reads = src.reads();
        assert!(s.cached_coverage(&*src, &SourceKey("other".into()), at).unwrap().is_empty());
        assert!(s.cached_coverage(&*src, &key(), VPath::at_default("missing")).unwrap().is_empty());
        assert!(s.cached_coverage(&*src, &key(), VPath::at_default("")).unwrap().is_empty(), "a directory");
        assert_eq!(src.reads(), reads, "coverage never reads the source");
    }
}
