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

mod state;

pub(crate) use state::{sub_logical, CacheState};
use state::{identity, normalize};

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;

use vfs_provider::{
    bad_fh, map_io_err, Access, Capabilities, DirEntry, Handle, Provider, Stat,
    VPath, KIND_FILE,
};

use crate::catalog::CacheRec;
use crate::ids::cache_file_id;
use crate::storage::{Storage, StorageError};
use crate::util::{lock, now_minute};

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
        // Decoded straight into the allocation the RAM tier keeps (see
        // `FileCell::committed_block`).
        let mut d = crate::ram::zeroed_block(len);
        let buf = Arc::get_mut(&mut d).expect("a new block has one owner");
        #[cfg(test)]
        let r = if s.cache.fail_store_reads.load(Ordering::SeqCst) {
            Err(vfs_block_store::Error::Corrupt(
                "injected read failure".into(),
            ))
        } else {
            s.store.read(&f.id, b * bs, buf)
        };
        #[cfg(not(test))]
        let r = s.store.read(&f.id, b * bs, buf);
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
        s.cache.store_hits.fetch_add(1, Ordering::Relaxed);
        // The content is immutable: a block another reader filled meanwhile
        // holds the same bytes, and the tier keeps it.
        Some(s.ram.fill(&f.id, b, d))
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
                // The cache file may now exist with a row that counts no
                // bytes, or with blocks its row does not count.
                s.needs_reconcile("writing a fetched unit to the store failed");
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

    fn stored_name(&self, p: VPath) -> Result<Option<String>, i32> {
        self.inner.stored_name(p)
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
pub(crate) mod tests;
