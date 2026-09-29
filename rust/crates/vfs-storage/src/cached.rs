//! [`CachedSource`]: pull-through caching of an immutable, slow source into the
//! block store.
//!
//! A cached file's store id is `b'C'` + BLAKE3-128 over, length-prefixed, the
//! [`SourceKey`], the root id, the normalized path, the size and a version tag
//! (the source's `mtime`: the `Provider` trait does not carry a remote
//! `file_id`). A changed file therefore gets a new id and can never be served
//! from the old one's blocks.
//!
//! A read goes RAM tier → block store → source, block by block. A block fetched
//! from the source is written to the store and the RAM tier, and concurrent
//! misses on the same `(file id, block)` wait on one fetch.
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
    VPath,
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
    /// Logical bytes (file lengths) of the cache files the store holds: what
    /// `cache_max_bytes` is checked against.
    pub cached_logical_bytes: u64,
}

/// One in-flight fetch: the block, and whether it came from the store after
/// all (a fetch that finished just before this one started).
type Fetch = Arc<OnceLock<Result<(Arc<[u8]>, bool), i32>>>;

/// Access times not yet committed to the catalog.
struct AccessLog {
    pending: HashMap<[u8; 16], CacheRec>,
    /// Session-local touch order: the tie-break within one access minute, so
    /// eviction is least-recently-used first even inside a minute.
    seq: HashMap<[u8; 16], u64>,
    last_commit_min: u64,
}

/// Cache bookkeeping one [`Storage`] shares between all its cached sources.
pub(crate) struct CacheState {
    /// Open handles per cache file id. Also the lock under which a file is
    /// registered (opened for the first time) or evicted.
    open_counts: Mutex<HashMap<[u8; 17], usize>>,
    inflight: Mutex<HashMap<([u8; 17], u64), Fetch>>,
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
    pub(crate) evicting: AtomicBool,
    pub(crate) evict_thread: Mutex<Option<JoinHandle<()>>>,
}

/// Locks `m`, entering a poisoned lock: every critical section here leaves its
/// map consistent at each step, so a panic elsewhere is no reason to stop.
pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn now_minute() -> u64 {
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
            evicting: AtomicBool::new(false),
            evict_thread: Mutex::new(None),
        }
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
            storage: Arc::clone(self),
            inner: source,
            key,
            caps,
            next: AtomicU64::new(1),
            opens: Mutex::new(HashMap::new()),
        })
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
        }
    }

    /// Registers an open handle on cache file `hash`. Creates the store file
    /// (and its catalog row, first) if the store lacks it. Returns whether the
    /// file was new to the cache.
    fn cache_acquire(&self, hash: &[u8; 16], size: u64) -> Result<bool, StorageError> {
        let id = cache_file_id(hash);
        let mut counts = lock(&self.cache.open_counts);
        *counts.entry(id).or_insert(0) += 1;
        let registered = (|| {
            if self.store.stat(&id)?.is_some() {
                return Ok(false);
            }
            // A row without its store file (a crash lost the file) is already
            // counted in `cached_logical`.
            let known = self.catalog.cache_get(hash)?.is_some();
            let rec = CacheRec {
                last_access_min: now_minute(),
                logical_bytes: size,
            };
            // Catalog row first, store file second (spec §6).
            self.catalog.cache_put_many(&[(*hash, rec)])?;
            self.store.set_len(&id, size)?;
            if !known {
                self.cache.cached_logical.fetch_add(size, Ordering::Relaxed);
            }
            Ok(!known)
        })();
        if registered.is_err() {
            release_count(&mut counts, &id);
        }
        drop(counts);
        if registered.is_ok() {
            self.touch(hash, size);
        }
        registered
    }

    /// Drops an open handle on cache file `hash`, recording the access first
    /// (so an eviction that sees the count reach zero also sees this access).
    fn cache_release(&self, hash: &[u8; 16], size: u64) {
        self.touch(hash, size);
        release_count(&mut lock(&self.cache.open_counts), &cache_file_id(hash));
    }

    /// Open-handle counts per cache file id. Eviction holds this lock while
    /// it checks a file and deletes it, so no open can slip in between.
    pub(crate) fn open_counts(&self) -> MutexGuard<'_, HashMap<[u8; 17], usize>> {
        lock(&self.cache.open_counts)
    }

    /// Records an access; commits the batch if a minute has passed since the
    /// last commit.
    fn touch(&self, hash: &[u8; 16], size: u64) {
        let now = now_minute();
        let seq = self.cache.touch_seq.fetch_add(1, Ordering::Relaxed) + 1;
        let mut a = lock(&self.cache.access);
        a.pending.insert(
            *hash,
            CacheRec {
                last_access_min: now,
                logical_bytes: size,
            },
        );
        a.seq.insert(*hash, seq);
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

    /// Session touch order of each file touched since open, for eviction's
    /// tie-break.
    pub(crate) fn touch_order(&self) -> HashMap<[u8; 16], u64> {
        lock(&self.cache.access).seq.clone()
    }

    /// Forgets `hash`'s uncommitted access, so a later commit cannot bring
    /// back the catalog row of an evicted file.
    pub(crate) fn forget_access(&self, hash: &[u8; 16]) {
        let mut a = lock(&self.cache.access);
        a.pending.remove(hash);
        a.seq.remove(hash);
    }
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
    /// `None` for a directory, which passes through uncached.
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
    storage: Arc<Storage>,
    inner: Arc<dyn Provider>,
    key: SourceKey,
    caps: Capabilities,
    next: AtomicU64,
    opens: Mutex<HashMap<Handle, OpenRec>>,
}

impl CachedSource {
    fn normalize(&self, rel: &str) -> String {
        // Fold-equal spellings are one file only if the source says so.
        match self.caps.case {
            CaseMatch::Insensitive => vfs_core::fold(rel),
            CaseMatch::Sensitive => rel.to_string(),
        }
    }

    /// Block `b` of `f`: RAM tier, else store, else one (coalesced) fetch.
    /// The flag is true if the block came from RAM or the store.
    fn block(&self, f: &CachedFile, inner: Handle, b: u64) -> Result<(Arc<[u8]>, bool), i32> {
        let s = &*self.storage;
        if let Some(d) = s.ram.get(&f.id, b) {
            s.cache.ram_hits.fetch_add(1, Ordering::Relaxed);
            return Ok((d, true));
        }
        if let Some(d) = self.read_stored(f, b)? {
            return Ok((d, true));
        }
        let key = (f.id, b);
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
        let got = cell.get_or_init(|| self.fetch(f, inner, b)).clone();
        let mut inflight = lock(&s.cache.inflight);
        if inflight.get(&key).is_some_and(|c| Arc::ptr_eq(c, &cell)) {
            inflight.remove(&key);
        }
        got
    }

    fn read_stored(&self, f: &CachedFile, b: u64) -> Result<Option<Arc<[u8]>>, i32> {
        let s = &*self.storage;
        let bs = s.block_size();
        let len = bs.min(f.size - b * bs) as usize;
        let mut buf = vec![0u8; len];
        let r = s
            .store
            .read(&f.id, b * bs, &mut buf)
            .map_err(|e| StorageError::from(e).to_status())?;
        if !r.missing.is_empty() || r.bytes != len {
            return Ok(None);
        }
        let d: Arc<[u8]> = buf.into();
        s.cache.store_hits.fetch_add(1, Ordering::Relaxed);
        s.ram.put(&f.id, b, Arc::clone(&d));
        Ok(Some(d))
    }

    /// Fetches block `b` whole from the source, stores it and returns it. Runs
    /// once per concurrent miss.
    fn fetch(&self, f: &CachedFile, inner: Handle, b: u64) -> Result<(Arc<[u8]>, bool), i32> {
        // A fetch that finished between our store miss and our joining the
        // in-flight map has already stored the block.
        if let Some(d) = self.read_stored(f, b)? {
            return Ok((d, true));
        }
        let s = &*self.storage;
        let bs = s.block_size();
        let start = b * bs;
        let len = bs.min(f.size - start) as usize;
        let mut buf = vec![0u8; len];
        let mut filled = 0;
        while filled < len {
            let n = self
                .inner
                .read_at(inner, start + filled as u64, &mut buf[filled..])?;
            if n == 0 {
                tracing::warn!(
                    offset = start + filled as u64,
                    size = f.size,
                    "cached source ended before the size it reported at open"
                );
                return Err(map_io_err());
            }
            filled += n;
        }
        s.cache.misses.fetch_add(1, Ordering::Relaxed);
        s.cache
            .bytes_from_source
            .fetch_add(len as u64, Ordering::Relaxed);
        // The source read succeeded, so a store failure costs only caching.
        if let Err(e) = s.store.write_blocks(&f.id, b, &buf) {
            tracing::warn!(error = %e, "writing a fetched block to the store failed");
        }
        let d: Arc<[u8]> = buf.into();
        s.ram.put(&f.id, b, Arc::clone(&d));
        crate::evict::maybe_evict(&self.storage);
        Ok((d, false))
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
        let file = if is_dir {
            None
        } else {
            let mtime = st.map_or(0, |s| s.mtime);
            let hash = identity(
                &self.key,
                p.root,
                &self.normalize(p.rel),
                size,
                &mtime.to_le_bytes(),
            );
            match self.storage.cache_acquire(&hash, size) {
                Ok(new) => {
                    if new {
                        crate::evict::maybe_evict(&self.storage);
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, path = p.rel, "opening a cache file failed");
                    let _ = self.inner.close(inner);
                    return Err(e.to_status());
                }
            }
            Some(CachedFile {
                hash,
                id: cache_file_id(&hash),
                size,
            })
        };
        let h = self.next.fetch_add(1, Ordering::Relaxed);
        lock(&self.opens).insert(h, OpenRec { inner, file });
        Ok((h, size, is_dir))
    }

    fn close(&self, h: Handle) -> Result<(), i32> {
        let rec = lock(&self.opens).remove(&h).ok_or_else(bad_fh)?;
        if let Some(f) = rec.file {
            self.storage.cache_release(&f.hash, f.size);
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
                self.storage.cache_release(&f.hash, f.size);
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
                preferred_block: None,
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
}
