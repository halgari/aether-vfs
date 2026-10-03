//! A client-side block cache for **small reads of immutable files**.
//!
//! A game and its plugins make huge numbers of tiny reads: one handle on
//! `Skyrim.esm` (≈240 MB) made 838,643 reads of 4 KiB in one launch, and
//! `plugins.txt` was read one byte per call. Over the ring each of those is a
//! round trip (a few µs plus the provider) where Windows would have answered
//! from its page cache. This cache sits in front of the ring for reads shorter
//! than [`CacheConfig::threshold`]: it serves them from aligned blocks of
//! [`CacheConfig::block`] bytes, fetching a missing block with one bulk read.
//! Reads at or above the threshold never touch it.
//!
//! OS-free and generic over how a block is fetched, so the shim, the native
//! tests and `ring-bench` all run this same code.
//!
//! # What is cached: the coherence rule
//!
//! Only content that **cannot change underneath the cache**:
//!
//! - A file is cached only through handles the director opened **read-only
//!   and immutable** (`OpenResp::immutable`: the provider holding the handle
//!   says its bytes never change). Mod content served from an immutable store
//!   is; a write layer (saves, INIs, logs) and a plain disk directory are not.
//!   This is what makes the cache safe against *other processes*: a tool
//!   writing the same write layer writes files this cache never holds, and
//!   when it copies an immutable file up into that layer, the path's next open
//!   here is served by the layer, reported mutable, and drops the file.
//! - Blocks are keyed per **file** — root and folded path — not per handle,
//!   so every handle on one file shares them; and per **version** (the size
//!   and the director's mount generation at open), so a remount that puts
//!   other content at the same path is a different file.
//! - Any sign of change through this process **drops the file's blocks and
//!   stops caching it for the rest of the process** ("poisons" it): an open
//!   for write, an open the director reports mutable, a write, a truncate or
//!   end-of-file change, a delete or a rename through any handle — the last
//!   two by path, for everything at or under it.
//!
//! # Concurrency
//!
//! Each file has its own lock, held only to look a block up or install one;
//! bytes are copied out after it is released, and **no lock is held across a
//! fetch**. Two threads missing the same block fetch it once: the first
//! installs a *loading* slot and fetches, the second waits for that fetch
//! (single flight). Memory is bounded under contention because a fetch
//! reserves its block's bytes against [`CacheConfig::max_bytes`] **before** it
//! starts, evicting the least recently used blocks of any file to make room;
//! when nothing can be evicted (every block is mid-fetch) the read is simply
//! served uncached. Lock order: registry → file → LRU index; the LRU index is
//! never held while a file lock is taken.
//!
//! # What a caller must do
//!
//! [`ReadCache::read`] answers `None` whenever it does not serve a read — too
//! large, not cacheable, the file changed, a fetch failed or came back short,
//! no room — and the caller then reads uncached exactly as it would have
//! without the cache. So a cache answer is always the bytes the director
//! would have returned, or no answer at all.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::time::Duration;

/// Bytes per cached block.
pub const DEFAULT_BLOCK: usize = 1 << 20;
/// Reads shorter than this are served from the cache; others bypass it.
pub const DEFAULT_THRESHOLD: usize = 64 * 1024;
/// Blocks one file may hold at once.
pub const DEFAULT_BLOCKS_PER_FILE: usize = 8;
/// Bytes the whole cache may hold, in-flight fetches included.
pub const DEFAULT_MAX_BYTES: usize = 64 << 20;

/// A file goes cold after this many missing reads in a row of evaluation…
const COLD_AFTER_MISSES: u32 = 16;
/// …if it averaged fewer hits than this per miss: a fetch costs a block, and
/// a file read at random across more than [`CacheConfig::blocks_per_file`]
/// blocks would otherwise turn every small read into one.
const COLD_MIN_HITS_PER_MISS: u32 = 8;
/// Reads a cold file is served uncached before it is tried again, doubling
/// each time it goes cold again, up to [`COLD_MAX_READS`].
const COLD_READS: u32 = 4096;
const COLD_MAX_READS: u32 = 1 << 16;

/// How the cache is cut up. [`Default`] is what the shim uses.
#[derive(Debug, Clone, Copy)]
pub struct CacheConfig {
    pub block: usize,
    pub threshold: usize,
    pub blocks_per_file: usize,
    pub max_bytes: usize,
    /// Longest a reader waits for another thread's fetch of the block it
    /// needs before reading uncached instead. A fetch is itself bounded by
    /// the ring's deadline, so this only matters if that one hangs.
    pub wait: Duration,
}

impl Default for CacheConfig {
    fn default() -> Self {
        CacheConfig {
            block: DEFAULT_BLOCK,
            threshold: DEFAULT_THRESHOLD,
            blocks_per_file: DEFAULT_BLOCKS_PER_FILE,
            max_bytes: DEFAULT_MAX_BYTES,
            wait: crate::RESPONSE_DEADLINE * 2,
        }
    }
}

/// Counters, as of one [`ReadCache::stats`] call.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CacheStats {
    /// Small reads served wholly from blocks already held.
    pub hits: u64,
    /// Small reads served after fetching (or waiting for) a block.
    pub misses: u64,
    /// Small reads on a cacheable file the cache declined (file gone cold,
    /// changed, no room, or a fetch that failed), and so read uncached.
    pub declined: u64,
    /// Block fetches, and the bytes they brought in.
    pub fetches: u64,
    pub bytes_fetched: u64,
    /// Blocks dropped to make room (per file or process-wide).
    pub evictions: u64,
    /// Files dropped because they changed or might have, and the blocks that
    /// held between them.
    pub invalidations: u64,
    pub blocks_invalidated: u64,
    /// Times a file was found to be read too randomly to be worth caching.
    pub cold: u64,
    /// Bytes held now (in-flight fetches included), and files known.
    pub resident_bytes: u64,
    pub files: u64,
}

#[derive(Default)]
struct Counters {
    hits: AtomicU64,
    misses: AtomicU64,
    declined: AtomicU64,
    fetches: AtomicU64,
    bytes_fetched: AtomicU64,
    evictions: AtomicU64,
    invalidations: AtomicU64,
    blocks_invalidated: AtomicU64,
    cold: AtomicU64,
}

fn bump(c: &AtomicU64, n: u64) {
    c.fetch_add(n, Ordering::Relaxed);
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Which file: a root and its folded path under it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Name {
    root: u32,
    path: String,
}

/// Which content of that file: equal for every immutable open of it within
/// one mount generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Version {
    size: u64,
    mount_gen: u32,
}

/// What one handle holds of the cache: its file, and — if the handle may be
/// served from it — the version it saw. Cheap to clone.
#[derive(Clone)]
pub struct FileRef {
    entry: Arc<Entry>,
    version: Option<Version>,
}

impl FileRef {
    /// Whether reads through this handle may be served from the cache.
    pub fn cacheable(&self) -> bool {
        self.version.is_some()
    }

    /// The file size this handle was opened with, if it is cacheable.
    pub fn size(&self) -> Option<u64> {
        self.version.map(|v| v.size)
    }
}

impl std::fmt::Debug for FileRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileRef")
            .field("root", &self.entry.name.root)
            .field("path", &self.entry.name.path)
            .field("version", &self.version)
            .finish()
    }
}

struct Entry {
    name: Name,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// Changed, or might have: never cached again in this process.
    poisoned: bool,
    /// The version the blocks below hold.
    version: Option<Version>,
    slots: Vec<Slot>,
    /// Since the file was last judged: reads that hit, and that missed.
    hits: u32,
    misses: u32,
    /// Reads left to serve uncached while cold, and the next cold spell.
    cold_left: u32,
    cold_next: u32,
}

struct Slot {
    idx: u64,
    kind: SlotKind,
}

enum SlotKind {
    Ready {
        data: Arc<[u8]>,
        /// Tick of the last read that used it.
        last: u64,
        /// Its key in the LRU index (the tick it was filed under).
        lru_key: u64,
    },
    Loading(Arc<Flight>),
}

/// One block fetch other readers of that block can wait for.
struct Flight {
    done: Mutex<Option<Option<Arc<[u8]>>>>,
    cv: Condvar,
}

impl Flight {
    fn new() -> Self {
        Flight {
            done: Mutex::new(None),
            cv: Condvar::new(),
        }
    }

    /// The first completion wins; later ones are ignored.
    fn complete(&self, v: Option<Arc<[u8]>>) {
        let mut g = lock(&self.done);
        if g.is_none() {
            *g = Some(v);
            self.cv.notify_all();
        }
    }

    fn wait(&self, patience: Duration) -> Option<Arc<[u8]>> {
        let g = lock(&self.done);
        let (g, _) = self
            .cv
            .wait_timeout_while(g, patience, |d| d.is_none())
            .unwrap_or_else(|e| e.into_inner());
        g.clone().flatten()
    }
}

struct Registry {
    by_name: HashMap<Name, Arc<Entry>>,
    /// Size past which the next registration sweeps out unused entries.
    sweep_at: usize,
}

const SWEEP_MIN: usize = 1024;

/// The cache. One per process in the shim; see the module docs.
pub struct ReadCache {
    cfg: CacheConfig,
    files: Mutex<Registry>,
    /// Ready blocks by the tick they were filed under, oldest first: the
    /// process-wide LRU. A block read since it was filed carries a newer
    /// tick of its own and is re-filed when it reaches the front, rather
    /// than on every hit, so a hit takes only its file's lock.
    lru: Mutex<BTreeMap<u64, (Weak<Entry>, u64)>>,
    /// Bytes held: ready blocks plus fetches in flight.
    used: AtomicUsize,
    tick: AtomicU64,
    counters: Counters,
}

impl Default for ReadCache {
    fn default() -> Self {
        Self::new(CacheConfig::default())
    }
}

impl ReadCache {
    pub fn new(cfg: CacheConfig) -> Self {
        assert!(cfg.block > 0 && cfg.blocks_per_file > 0, "an empty cache");
        assert!(
            cfg.threshold <= cfg.block,
            "a read the cache serves must fit in two blocks"
        );
        ReadCache {
            cfg,
            files: Mutex::new(Registry {
                by_name: HashMap::new(),
                sweep_at: SWEEP_MIN,
            }),
            lru: Mutex::new(BTreeMap::new()),
            used: AtomicUsize::new(0),
            tick: AtomicU64::new(1),
            counters: Counters::default(),
        }
    }

    pub fn config(&self) -> &CacheConfig {
        &self.cfg
    }

    /// Whether a read of `len` bytes is small enough to be offered to
    /// [`ReadCache::read`].
    pub fn wants(&self, len: usize) -> bool {
        len > 0 && len < self.cfg.threshold
    }

    fn now(&self) -> u64 {
        self.tick.fetch_add(1, Ordering::Relaxed)
    }

    /// A handle on `path` (folded, under `root`) was opened, `size` bytes,
    /// under the director's mount generation `mount_gen`. `immutable` is the
    /// director's word that the handle's bytes cannot change; `write` that
    /// the handle was opened for writing.
    ///
    /// A write open, or one the director does not call immutable, poisons
    /// the file: its blocks go and it is not cached again. Otherwise the
    /// returned ref is cacheable unless the file is already poisoned.
    pub fn register(
        &self,
        root: u32,
        path: &str,
        size: u64,
        mount_gen: u32,
        immutable: bool,
        write: bool,
    ) -> FileRef {
        let name = Name {
            root,
            path: path.to_string(),
        };
        let entry = {
            let mut reg = lock(&self.files);
            if reg.by_name.len() >= reg.sweep_at {
                Self::sweep(&mut reg);
            }
            Arc::clone(reg.by_name.entry(name.clone()).or_insert_with(|| {
                Arc::new(Entry {
                    name,
                    state: Mutex::new(State::default()),
                })
            }))
        };
        let version = Version { size, mount_gen };
        let mut st = lock(&entry.state);
        if write || !immutable {
            self.poison(&mut st);
            drop(st);
            return FileRef {
                entry,
                version: None,
            };
        }
        if st.poisoned {
            drop(st);
            return FileRef {
                entry,
                version: None,
            };
        }
        if st.version != Some(version) {
            // Other content at this path now (a remount): what is held is
            // another file's. Handles still open on the old one stop being
            // served (their version no longer matches) and read uncached.
            self.clear_slots(&mut st);
            st.version = Some(version);
        }
        drop(st);
        FileRef {
            entry,
            version: Some(version),
        }
    }

    /// Drop entries nothing uses: no handle holds them, they hold no
    /// blocks, and they are not poisoned (a poisoned entry is what stops a
    /// changed file being cached again).
    fn sweep(reg: &mut Registry) {
        reg.by_name.retain(|_, e| {
            if Arc::strong_count(e) > 1 {
                return true;
            }
            match e.state.try_lock() {
                Ok(st) => st.poisoned || !st.slots.is_empty(),
                Err(_) => true,
            }
        });
        reg.sweep_at = (reg.by_name.len() * 2).max(SWEEP_MIN);
    }

    /// The file behind `f` was written, truncated or otherwise changed
    /// through this process: drop it, and never cache it again.
    pub fn invalidate(&self, f: &FileRef) {
        let mut st = lock(&f.entry.state);
        self.poison(&mut st);
    }

    /// `path` (folded, under `root`) was deleted or renamed, or something was
    /// renamed onto it: drop it and everything under it, and never cache any
    /// of it again. An empty path or `"."` is the whole root.
    pub fn invalidate_path(&self, root: u32, path: &str) {
        let path = path.trim_matches('/');
        let whole_root = path.is_empty() || path == ".";
        let mut reg = lock(&self.files);
        let hit: Vec<Arc<Entry>> = reg
            .by_name
            .values()
            .filter(|e| {
                e.name.root == root
                    && (whole_root
                        || e.name.path == path
                        || (e.name.path.len() > path.len()
                            && e.name.path.starts_with(path)
                            && e.name.path.as_bytes()[path.len()] == b'/'))
            })
            .cloned()
            .collect();
        if !whole_root {
            // Not opened yet, perhaps, but a later immutable open of a path
            // this process has renamed or deleted must not be believed.
            let name = Name {
                root,
                path: path.to_string(),
            };
            reg.by_name.entry(name.clone()).or_insert_with(|| {
                Arc::new(Entry {
                    name,
                    state: Mutex::new(State {
                        poisoned: true,
                        ..State::default()
                    }),
                })
            });
        }
        for e in hit {
            let mut st = lock(&e.state);
            self.poison(&mut st);
        }
    }

    fn poison(&self, st: &mut State) {
        if !st.poisoned {
            st.poisoned = true;
            bump(&self.counters.invalidations, 1);
        }
        let n = st
            .slots
            .iter()
            .filter(|s| matches!(s.kind, SlotKind::Ready { .. }))
            .count();
        bump(&self.counters.blocks_invalidated, n as u64);
        self.clear_slots(st);
        st.version = None;
    }

    /// Remove every slot: ready blocks give their bytes back; a fetch in
    /// flight is told it is not wanted (its waiters read uncached) and gives
    /// its own reservation back when it finishes.
    fn clear_slots(&self, st: &mut State) {
        for slot in st.slots.drain(..) {
            match slot.kind {
                SlotKind::Ready { data, lru_key, .. } => {
                    lock(&self.lru).remove(&lru_key);
                    self.used.fetch_sub(data.len(), Ordering::AcqRel);
                }
                SlotKind::Loading(flight) => flight.complete(None),
            }
        }
    }

    /// Serve a read of `buf.len()` bytes at `off` through `f` from the
    /// cache, fetching missing blocks with `fetch(block_offset, block_buf)`,
    /// which must read `block_buf.len()` bytes at `block_offset` (the block,
    /// cut short at end of file) and return how many it read.
    ///
    /// `Some(n)` when the read was served: `n` is `buf.len()` cut short at
    /// end of file, and `buf[..n]` holds the bytes. `None` when it was not —
    /// the caller then reads uncached, as it would have without the cache
    /// (`buf` may have been written to meanwhile). A read at or past end of
    /// file is never served: the caller's own end-of-file answer stands.
    pub fn read<F>(&self, f: &FileRef, off: u64, buf: &mut [u8], mut fetch: F) -> Option<usize>
    where
        F: FnMut(u64, &mut [u8]) -> Result<usize, i32>,
    {
        let ver = f.version?;
        if !self.wants(buf.len()) || off >= ver.size {
            return None;
        }
        let len = buf.len().min((ver.size - off) as usize);
        {
            let mut st = lock(&f.entry.state);
            if st.poisoned || st.version != Some(ver) {
                drop(st);
                bump(&self.counters.declined, 1);
                return None;
            }
            if st.cold_left > 0 {
                st.cold_left -= 1;
                drop(st);
                bump(&self.counters.declined, 1);
                return None;
            }
        }
        let block = self.cfg.block as u64;
        let first = off / block;
        let last = (off + len as u64 - 1) / block;
        let mut fetched = false;
        let mut done = 0usize;
        for idx in first..=last {
            let Some((data, was_fetched)) = self.block(f, ver, idx, &mut fetch) else {
                bump(&self.counters.declined, 1);
                return None;
            };
            fetched |= was_fetched;
            let at = off + done as u64;
            let from = (at - idx * block) as usize;
            let n = (len - done).min(data.len() - from);
            buf[done..done + n].copy_from_slice(&data[from..from + n]);
            done += n;
        }
        debug_assert_eq!(done, len);
        if fetched {
            bump(&self.counters.misses, 1);
        } else {
            bump(&self.counters.hits, 1);
        }
        self.judge(f, fetched);
        Some(len)
    }

    /// Count a served read toward deciding whether `f` is worth caching.
    fn judge(&self, f: &FileRef, missed: bool) {
        let mut st = lock(&f.entry.state);
        if missed {
            st.misses += 1;
        } else {
            st.hits = st.hits.saturating_add(1);
        }
        if st.misses < COLD_AFTER_MISSES {
            return;
        }
        if st.hits < st.misses * COLD_MIN_HITS_PER_MISS {
            let spell = if st.cold_next == 0 {
                COLD_READS
            } else {
                st.cold_next
            };
            st.cold_left = spell;
            st.cold_next = (spell * 2).min(COLD_MAX_READS);
            bump(&self.counters.cold, 1);
            // Its blocks are of no use while it is cold.
            self.clear_slots(&mut st);
        }
        st.hits = 0;
        st.misses = 0;
    }

    /// Block `idx` of `f`, and whether this call had to fetch (or wait for
    /// a fetch of) it. `None` if it cannot be had from the cache.
    fn block<F>(
        &self,
        f: &FileRef,
        ver: Version,
        idx: u64,
        fetch: &mut F,
    ) -> Option<(Arc<[u8]>, bool)>
    where
        F: FnMut(u64, &mut [u8]) -> Result<usize, i32>,
    {
        let flight = {
            let mut st = lock(&f.entry.state);
            if st.poisoned || st.version != Some(ver) {
                return None;
            }
            let tick = self.now();
            match st.slots.iter_mut().find(|s| s.idx == idx) {
                Some(Slot {
                    kind: SlotKind::Ready { data, last, .. },
                    ..
                }) => {
                    *last = tick;
                    return Some((Arc::clone(data), false));
                }
                Some(Slot {
                    kind: SlotKind::Loading(flight),
                    ..
                }) => Arc::clone(flight),
                None => {
                    if st.slots.len() >= self.cfg.blocks_per_file && !self.evict_in_file(&mut st) {
                        // Every slot is mid-fetch.
                        return None;
                    }
                    let flight = Arc::new(Flight::new());
                    st.slots.push(Slot {
                        idx,
                        kind: SlotKind::Loading(Arc::clone(&flight)),
                    });
                    drop(st);
                    return self.load(f, ver, idx, flight, fetch);
                }
            }
        };
        // Someone else is fetching it: wait for that, holding no lock.
        flight.wait(self.cfg.wait).map(|d| (d, true))
    }

    /// Drop the least recently used ready block of one file to make room
    /// for another of it. `false` if it holds none.
    fn evict_in_file(&self, st: &mut State) -> bool {
        let oldest = st
            .slots
            .iter()
            .enumerate()
            .filter_map(|(i, s)| match &s.kind {
                SlotKind::Ready { last, .. } => Some((*last, i)),
                SlotKind::Loading(_) => None,
            })
            .min();
        let Some((_, i)) = oldest else {
            return false;
        };
        if let SlotKind::Ready { data, lru_key, .. } = st.slots.swap_remove(i).kind {
            lock(&self.lru).remove(&lru_key);
            self.used.fetch_sub(data.len(), Ordering::AcqRel);
            bump(&self.counters.evictions, 1);
        }
        true
    }

    /// Fetch block `idx` of `f` into the loading slot this thread installed.
    fn load<F>(
        &self,
        f: &FileRef,
        ver: Version,
        idx: u64,
        flight: Arc<Flight>,
        fetch: &mut F,
    ) -> Option<(Arc<[u8]>, bool)>
    where
        F: FnMut(u64, &mut [u8]) -> Result<usize, i32>,
    {
        let start = idx * self.cfg.block as u64;
        let want = (ver.size - start).min(self.cfg.block as u64) as usize;
        // Undoes everything this load set up, unless it is disarmed by a
        // successful install — including when `fetch` unwinds.
        let mut guard = LoadGuard {
            cache: self,
            entry: &f.entry,
            flight: &flight,
            reserved: 0,
        };
        if !self.reserve(want) {
            return None;
        }
        guard.reserved = want;
        let mut buf = vec![0u8; want];
        match fetch(start, &mut buf) {
            // Short of the block means short of what the director said the
            // file holds: not something to keep. The caller's uncached read
            // gets whatever the director answers now.
            Ok(n) if n == want => {}
            _ => return None,
        }
        bump(&self.counters.fetches, 1);
        bump(&self.counters.bytes_fetched, want as u64);
        let data: Arc<[u8]> = buf.into();
        {
            let mut st = lock(&f.entry.state);
            if st.poisoned || st.version != Some(ver) {
                return None;
            }
            let Some(slot) = st
                .slots
                .iter_mut()
                .find(|s| matches!(&s.kind, SlotKind::Loading(fl) if Arc::ptr_eq(fl, &flight)))
            else {
                // Dropped meanwhile (the file went cold, or changed).
                return None;
            };
            let tick = self.now();
            slot.kind = SlotKind::Ready {
                data: Arc::clone(&data),
                last: tick,
                lru_key: tick,
            };
            lock(&self.lru).insert(tick, (Arc::downgrade(&f.entry), idx));
            guard.reserved = 0;
        }
        flight.complete(Some(Arc::clone(&data)));
        std::mem::forget(guard);
        Some((data, true))
    }

    /// Take `n` bytes of the budget, evicting the least recently used
    /// blocks of any file until they fit. `false` if they cannot.
    fn reserve(&self, n: usize) -> bool {
        if n > self.cfg.max_bytes {
            return false;
        }
        loop {
            let cur = self.used.load(Ordering::Acquire);
            if cur + n <= self.cfg.max_bytes {
                if self
                    .used
                    .compare_exchange(cur, cur + n, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    return true;
                }
                continue;
            }
            if !self.evict_oldest() {
                return false;
            }
        }
    }

    /// Evict the process-wide least recently used ready block. `false` if
    /// there is none.
    fn evict_oldest(&self) -> bool {
        loop {
            let Some((key, (weak, idx))) = lock(&self.lru).pop_first() else {
                return false;
            };
            let Some(entry) = weak.upgrade() else {
                continue;
            };
            let mut st = lock(&entry.state);
            let Some(pos) = st.slots.iter().position(|s| {
                s.idx == idx
                    && matches!(&s.kind, SlotKind::Ready { lru_key, .. } if *lru_key == key)
            }) else {
                // Filed under a key that no longer names it: already gone.
                continue;
            };
            if let SlotKind::Ready { last, lru_key, .. } = &mut st.slots[pos].kind {
                if *last > key {
                    // Read since it was filed: file it again under that
                    // read, and look at the next oldest.
                    *lru_key = *last;
                    lock(&self.lru).insert(*last, (weak, idx));
                    continue;
                }
            }
            if let SlotKind::Ready { data, .. } = st.slots.swap_remove(pos).kind {
                self.used.fetch_sub(data.len(), Ordering::AcqRel);
                bump(&self.counters.evictions, 1);
            }
            return true;
        }
    }

    pub fn stats(&self) -> CacheStats {
        let c = &self.counters;
        let get = |a: &AtomicU64| a.load(Ordering::Relaxed);
        CacheStats {
            hits: get(&c.hits),
            misses: get(&c.misses),
            declined: get(&c.declined),
            fetches: get(&c.fetches),
            bytes_fetched: get(&c.bytes_fetched),
            evictions: get(&c.evictions),
            invalidations: get(&c.invalidations),
            blocks_invalidated: get(&c.blocks_invalidated),
            cold: get(&c.cold),
            resident_bytes: self.used.load(Ordering::Relaxed) as u64,
            files: self
                .files
                .try_lock()
                .map(|r| r.by_name.len() as u64)
                .unwrap_or(0),
        }
    }
}

/// See [`ReadCache::load`].
struct LoadGuard<'a> {
    cache: &'a ReadCache,
    entry: &'a Arc<Entry>,
    flight: &'a Arc<Flight>,
    reserved: usize,
}

impl Drop for LoadGuard<'_> {
    fn drop(&mut self) {
        if self.reserved > 0 {
            self.cache.used.fetch_sub(self.reserved, Ordering::AcqRel);
        }
        {
            let mut st = lock(&self.entry.state);
            st.slots.retain(
                |s| !matches!(&s.kind, SlotKind::Loading(fl) if Arc::ptr_eq(fl, self.flight)),
            );
        }
        self.flight.complete(None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    const KIB: usize = 1024;

    /// A small cache: 16-byte blocks, reads under 8 bytes, 2 blocks a file,
    /// 64 bytes in all.
    fn tiny() -> ReadCache {
        ReadCache::new(CacheConfig {
            block: 16,
            threshold: 8,
            blocks_per_file: 2,
            max_bytes: 64,
            wait: Duration::from_secs(10),
        })
    }

    fn content(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i * 7 + i / 251) as u8).collect()
    }

    /// A fetcher over `data` that counts its calls and their offsets.
    struct Source {
        data: Vec<u8>,
        calls: AtomicUsize,
        offsets: Mutex<Vec<u64>>,
    }

    impl Source {
        fn new(data: Vec<u8>) -> Self {
            Source {
                data,
                calls: AtomicUsize::new(0),
                offsets: Mutex::new(Vec::new()),
            }
        }
        fn fetch(&self, off: u64, buf: &mut [u8]) -> Result<usize, i32> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            lock(&self.offsets).push(off);
            let off = off as usize;
            let n = buf.len().min(self.data.len().saturating_sub(off));
            buf[..n].copy_from_slice(&self.data[off..off + n]);
            Ok(n)
        }
        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    fn reg(c: &ReadCache, path: &str, size: usize) -> FileRef {
        c.register(0, path, size as u64, 1, true, false)
    }

    fn read(c: &ReadCache, f: &FileRef, s: &Source, off: u64, len: usize) -> Option<Vec<u8>> {
        let mut buf = vec![0xEEu8; len];
        let n = c.read(f, off, &mut buf, |o, b| s.fetch(o, b))?;
        buf.truncate(n);
        Some(buf)
    }

    #[test]
    fn a_small_read_is_served_from_one_fetched_block_and_then_from_memory() {
        let c = tiny();
        let s = Source::new(content(100));
        let f = reg(&c, "a", 100);
        assert_eq!(read(&c, &f, &s, 3, 5).unwrap(), &s.data[3..8]);
        assert_eq!(s.calls(), 1);
        assert_eq!(
            *lock(&s.offsets),
            vec![0],
            "the aligned block, not the read"
        );
        for off in 0..12 {
            assert_eq!(
                read(&c, &f, &s, off, 4).unwrap(),
                &s.data[off as usize..off as usize + 4]
            );
        }
        assert_eq!(s.calls(), 1, "every read inside block 0 is a hit");
        let st = c.stats();
        assert_eq!(
            (st.misses, st.hits, st.fetches, st.bytes_fetched),
            (1, 12, 1, 16)
        );
    }

    #[test]
    fn a_read_straddling_two_blocks_fetches_both_and_joins_them() {
        let c = tiny();
        let s = Source::new(content(100));
        let f = reg(&c, "a", 100);
        assert_eq!(read(&c, &f, &s, 13, 7).unwrap(), &s.data[13..20]);
        assert_eq!(*lock(&s.offsets), vec![0, 16]);
        assert_eq!(read(&c, &f, &s, 14, 7).unwrap(), &s.data[14..21]);
        assert_eq!(s.calls(), 2);
    }

    #[test]
    fn the_block_at_end_of_file_is_short_and_reads_are_cut_at_eof() {
        let c = tiny();
        let s = Source::new(content(37));
        let f = reg(&c, "a", 37);
        // Block 2 holds bytes 32..37.
        assert_eq!(
            read(&c, &f, &s, 33, 7).unwrap(),
            &s.data[33..37],
            "cut short at EOF"
        );
        assert_eq!(
            c.stats().bytes_fetched,
            5,
            "a block straddling EOF is short"
        );
        assert_eq!(read(&c, &f, &s, 36, 1).unwrap(), &s.data[36..37]);
        // At and past EOF: not served; the caller's own EOF answer stands.
        assert_eq!(read(&c, &f, &s, 37, 1), None);
        assert_eq!(read(&c, &f, &s, 1000, 1), None);
        // A read ending exactly at EOF across a block boundary.
        assert_eq!(read(&c, &f, &s, 30, 7).unwrap(), &s.data[30..37]);
        // An empty file has nothing to serve.
        let e = reg(&c, "empty", 0);
        assert_eq!(read(&c, &e, &s, 0, 1), None);
    }

    #[test]
    fn large_and_empty_reads_bypass_the_cache() {
        let c = tiny();
        let s = Source::new(content(100));
        let f = reg(&c, "a", 100);
        assert_eq!(read(&c, &f, &s, 0, 8), None, "at the threshold: bypass");
        assert_eq!(read(&c, &f, &s, 0, 0), None, "zero-length: bypass");
        assert_eq!(s.calls(), 0);
        assert!(!c.wants(8) && !c.wants(0) && c.wants(7));
    }

    #[test]
    fn a_fetch_that_comes_back_short_or_fails_is_not_kept() {
        let c = tiny();
        let f = reg(&c, "a", 100);
        let mut buf = [0u8; 4];
        assert_eq!(c.read(&f, 0, &mut buf, |_, b| Ok(b.len() - 1)), None);
        assert_eq!(c.read(&f, 0, &mut buf, |_, _| Err(-5)), None);
        assert_eq!(
            c.stats().resident_bytes,
            0,
            "the reservations were given back"
        );
        let s = Source::new(content(100));
        assert_eq!(
            read(&c, &f, &s, 0, 4).unwrap(),
            &s.data[0..4],
            "and the slot was freed"
        );
    }

    #[test]
    fn a_file_keeps_its_most_recently_used_blocks() {
        let c = tiny(); // two blocks a file
        let s = Source::new(content(100));
        let f = reg(&c, "a", 100);
        read(&c, &f, &s, 0, 1); // block 0
        read(&c, &f, &s, 16, 1); // block 1
        read(&c, &f, &s, 1, 1); // block 0 again: now the most recent
        read(&c, &f, &s, 32, 1); // block 2 evicts block 1
        assert_eq!(s.calls(), 3);
        read(&c, &f, &s, 2, 1);
        assert_eq!(s.calls(), 3, "block 0 stayed");
        read(&c, &f, &s, 17, 1);
        assert_eq!(s.calls(), 4, "block 1 went");
        assert_eq!(c.stats().evictions, 2);
    }

    #[test]
    fn the_process_wide_cap_evicts_the_least_recently_used_block_of_any_file() {
        let c = tiny(); // 64 bytes: four blocks in all
        let s = Source::new(content(100));
        let files: Vec<FileRef> = (0..4).map(|i| reg(&c, &format!("f{i}"), 100)).collect();
        for f in &files {
            read(&c, f, &s, 0, 1);
        }
        assert_eq!(c.stats().resident_bytes, 64);
        read(&c, &files[0], &s, 1, 1); // f0 is now the most recent
        let e = reg(&c, "f4", 100);
        read(&c, &e, &s, 0, 1); // must evict f1, the oldest untouched
        assert_eq!(c.stats().resident_bytes, 64, "never over the cap");
        let before = s.calls();
        read(&c, &files[0], &s, 2, 1);
        read(&c, &files[2], &s, 2, 1);
        read(&c, &files[3], &s, 2, 1);
        assert_eq!(s.calls(), before, "f0, f2, f3 kept their blocks");
        read(&c, &files[1], &s, 2, 1);
        assert_eq!(s.calls(), before + 1, "f1's was the one evicted");
    }

    #[test]
    fn a_block_larger_than_the_whole_budget_is_never_reserved() {
        let c = ReadCache::new(CacheConfig {
            block: 32,
            threshold: 8,
            blocks_per_file: 2,
            max_bytes: 16,
            wait: Duration::from_secs(1),
        });
        let s = Source::new(content(100));
        let f = reg(&c, "a", 100);
        assert_eq!(read(&c, &f, &s, 0, 4), None);
        assert_eq!(s.calls(), 0);
    }

    #[test]
    fn handles_on_one_file_share_its_blocks() {
        let c = tiny();
        let s = Source::new(content(100));
        let a = reg(&c, "data/x.esm", 100);
        let b = reg(&c, "data/x.esm", 100);
        read(&c, &a, &s, 0, 4);
        read(&c, &b, &s, 4, 4);
        assert_eq!(s.calls(), 1);
        // Another root's file of the same name is another file.
        let other = c.register(1, "data/x.esm", 100, 1, true, false);
        read(&c, &other, &s, 0, 4);
        assert_eq!(s.calls(), 2);
    }

    #[test]
    fn two_threads_missing_one_block_fetch_it_once() {
        let c = tiny();
        let f = reg(&c, "a", 100);
        let data = content(100);
        let calls = AtomicUsize::new(0);
        let entered = std::sync::Barrier::new(2);
        let (gate_tx, gate_rx) = std::sync::mpsc::channel::<()>();
        let gate_rx = Mutex::new(gate_rx);
        std::thread::scope(|scope| {
            let (calls, gate_rx, data, entered, c, f) = (&calls, &gate_rx, &data, &entered, &c, &f);
            let fetcher = move |o: u64, b: &mut [u8]| {
                calls.fetch_add(1, Ordering::SeqCst);
                // Hold the fetch until the other reader is waiting on it.
                let _ = lock(gate_rx).recv();
                let o = o as usize;
                b.copy_from_slice(&data[o..o + b.len()]);
                Ok(b.len())
            };
            let first = scope.spawn(move || {
                let mut buf = [0u8; 4];
                entered.wait();
                c.read(f, 0, &mut buf, fetcher).map(|n| buf[..n].to_vec())
            });
            entered.wait();
            // The first thread is inside its fetch once a loading slot exists.
            while !lock(&f.entry.state)
                .slots
                .iter()
                .any(|s| matches!(s.kind, SlotKind::Loading(_)))
            {
                std::thread::yield_now();
            }
            let second = scope.spawn(move || {
                let mut buf = [0u8; 4];
                c.read(f, 8, &mut buf, |_, _| {
                    panic!("the second reader must not fetch")
                })
                .map(|n| buf[..n].to_vec())
            });
            std::thread::sleep(Duration::from_millis(50));
            gate_tx.send(()).unwrap();
            assert_eq!(first.join().unwrap().unwrap(), &data[0..4]);
            assert_eq!(second.join().unwrap().unwrap(), &data[8..12]);
        });
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            c.stats().misses,
            2,
            "the waiter counts as a miss, not a hit"
        );
    }

    #[test]
    fn a_failed_fetch_releases_its_waiters_to_read_uncached() {
        let c = tiny();
        let f = reg(&c, "a", 100);
        let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
        let go_rx = Mutex::new(go_rx);
        std::thread::scope(|scope| {
            let first = scope.spawn(|| {
                let mut buf = [0u8; 4];
                c.read(&f, 0, &mut buf, |_, _| {
                    let _ = lock(&go_rx).recv();
                    Err(-5)
                })
            });
            while !lock(&f.entry.state)
                .slots
                .iter()
                .any(|s| matches!(s.kind, SlotKind::Loading(_)))
            {
                std::thread::yield_now();
            }
            let second = scope.spawn(|| {
                let mut buf = [0u8; 4];
                c.read(&f, 0, &mut buf, |_, _| panic!("must wait, not fetch"))
            });
            std::thread::sleep(Duration::from_millis(20));
            go_tx.send(()).unwrap();
            assert_eq!(first.join().unwrap(), None);
            assert_eq!(second.join().unwrap(), None);
        });
        assert_eq!(c.stats().resident_bytes, 0);
    }

    #[test]
    fn many_threads_reading_many_files_stay_correct_and_under_the_cap() {
        let c = ReadCache::new(CacheConfig {
            block: 4 * KIB,
            threshold: KIB,
            blocks_per_file: 3,
            max_bytes: 40 * KIB,
            wait: Duration::from_secs(10),
        });
        let data = content(64 * KIB);
        let peak = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for t in 0..8u64 {
                let (c, data, peak) = (&c, &data, &peak);
                scope.spawn(move || {
                    let mut x = 0x9E37_79B9_7F4A_7C15u64 ^ t;
                    for _ in 0..4000 {
                        x ^= x << 13;
                        x ^= x >> 7;
                        x ^= x << 17;
                        let file = (x % 6) as usize;
                        let f =
                            c.register(0, &format!("f{file}"), data.len() as u64, 1, true, false);
                        let off = (x >> 8) % (data.len() as u64);
                        let len = 1 + ((x >> 40) % 1000) as usize;
                        let mut buf = vec![0u8; len];
                        let got = c.read(&f, off, &mut buf, |o, b| {
                            let o = o as usize;
                            b.copy_from_slice(&data[o..o + b.len()]);
                            Ok(b.len())
                        });
                        if let Some(n) = got {
                            let o = off as usize;
                            assert_eq!(&buf[..n], &data[o..o + n]);
                            assert_eq!(n, len.min(data.len() - o));
                        }
                        peak.fetch_max(c.used.load(Ordering::SeqCst), Ordering::SeqCst);
                    }
                });
            }
        });
        assert!(
            peak.load(Ordering::SeqCst) <= 40 * KIB,
            "bounded memory under contention"
        );
        assert!(c.stats().hits > 0);
    }

    // ---- coherence ---------------------------------------------------------

    #[test]
    fn a_write_open_by_any_handle_drops_the_file_and_it_is_never_cached_again() {
        let c = tiny();
        let s = Source::new(content(100));
        let r = reg(&c, "data/a.ini", 100);
        read(&c, &r, &s, 0, 4).unwrap();
        assert_eq!(c.stats().resident_bytes, 16);
        let w = c.register(0, "data/a.ini", 100, 1, false, true);
        assert!(
            !w.cacheable(),
            "a write handle is never served from the cache"
        );
        assert_eq!(c.stats().resident_bytes, 0, "the blocks went");
        assert_eq!(
            read(&c, &r, &s, 0, 4),
            None,
            "the earlier handle reads uncached now"
        );
        let again = reg(&c, "data/a.ini", 100);
        assert!(!again.cacheable(), "and so does every later open");
        let st = c.stats();
        assert_eq!((st.invalidations, st.blocks_invalidated), (1, 1));
    }

    #[test]
    fn a_file_the_director_calls_mutable_is_never_cached() {
        let c = tiny();
        let s = Source::new(content(100));
        let m = c.register(0, "saves/a.ess", 100, 1, false, false);
        assert!(!m.cacheable());
        assert_eq!(read(&c, &m, &s, 0, 4), None);
        assert_eq!(s.calls(), 0);
        // An immutable open of a path once served mutable is not believed.
        let r = c.register(0, "saves/a.ess", 100, 1, true, false);
        assert!(!r.cacheable());
        // And one served mutable after being cached drops the cached copy.
        let x = reg(&c, "data/x", 100);
        read(&c, &x, &s, 0, 4).unwrap();
        let _ = c.register(0, "data/x", 100, 1, false, false);
        assert_eq!(read(&c, &x, &s, 0, 4), None);
        assert_eq!(c.stats().resident_bytes, 0);
    }

    #[test]
    fn a_write_or_truncate_through_another_handle_invalidates() {
        let c = tiny();
        let s = Source::new(content(100));
        let r = reg(&c, "a", 100);
        let other = reg(&c, "a", 100);
        read(&c, &r, &s, 0, 4).unwrap();
        c.invalidate(&other);
        assert_eq!(read(&c, &r, &s, 0, 4), None);
        assert_eq!(c.stats().resident_bytes, 0);
    }

    #[test]
    fn a_delete_or_rename_drops_the_path_and_everything_under_it() {
        let c = tiny();
        let s = Source::new(content(100));
        let a = reg(&c, "data/a.esp", 100);
        let ab = reg(&c, "data/ab.esp", 100);
        let sub = reg(&c, "data/sub/x.dds", 100);
        let other = c.register(1, "data/sub/x.dds", 100, 1, true, false);
        for f in [&a, &ab, &sub, &other] {
            read(&c, f, &s, 0, 4).unwrap();
        }
        c.invalidate_path(0, "data/sub");
        assert_eq!(read(&c, &sub, &s, 0, 4), None, "under the path");
        assert!(read(&c, &a, &s, 0, 4).is_some(), "beside it");
        assert!(read(&c, &other, &s, 0, 4).is_some(), "another root");
        c.invalidate_path(0, "data/a.esp");
        assert_eq!(read(&c, &a, &s, 0, 4), None);
        assert!(
            read(&c, &ab, &s, 0, 4).is_some(),
            "a sibling sharing a prefix"
        );
        // A rename's target, not opened before, is not believed afterwards.
        c.invalidate_path(0, "data/new.esp");
        assert!(!reg(&c, "data/new.esp", 100).cacheable());
        // The whole root.
        c.invalidate_path(0, ".");
        assert_eq!(read(&c, &ab, &s, 0, 4), None);
    }

    #[test]
    fn a_remount_with_other_content_at_the_path_is_another_file() {
        let c = tiny();
        let old = Source::new(content(100));
        let new = Source::new(content(100).into_iter().map(|b| !b).collect());
        let a = c.register(0, "a", 100, 1, true, false);
        read(&c, &a, &old, 0, 4).unwrap();
        let b = c.register(0, "a", 100, 2, true, false);
        assert_eq!(read(&c, &b, &new, 0, 4).unwrap(), &new.data[0..4]);
        assert_eq!(
            read(&c, &a, &old, 0, 4),
            None,
            "the old handle reads uncached"
        );
        // A different size is a different version too.
        let d = c.register(0, "a", 99, 2, true, false);
        assert_eq!(read(&c, &d, &new, 0, 4).unwrap(), &new.data[0..4]);
    }

    #[test]
    fn a_fetch_overtaken_by_an_invalidation_is_not_installed() {
        let c = tiny();
        let f = reg(&c, "a", 100);
        let data = content(100);
        let got = {
            let mut buf = [0u8; 4];
            c.read(&f, 0, &mut buf, |o, b| {
                c.invalidate(&f); // a write lands while the block is in flight
                let o = o as usize;
                b.copy_from_slice(&data[o..o + b.len()]);
                Ok(b.len())
            })
        };
        assert_eq!(got, None, "served uncached instead");
        assert_eq!(c.stats().resident_bytes, 0);
        assert!(lock(&f.entry.state).slots.is_empty());
    }

    // ---- locality -----------------------------------------------------------

    #[test]
    fn a_file_read_at_random_goes_cold_and_is_read_uncached_for_a_while() {
        let c = ReadCache::new(CacheConfig {
            block: 16,
            threshold: 8,
            blocks_per_file: 2,
            max_bytes: 1 << 20,
            wait: Duration::from_secs(1),
        });
        let s = Source::new(content(16 * 64));
        let f = reg(&c, "big", 16 * 64);
        // Every read a new block: all misses.
        for i in 0..COLD_AFTER_MISSES as u64 {
            assert!(read(&c, &f, &s, i * 16 * 3 % (16 * 64), 1).is_some());
        }
        assert_eq!(c.stats().cold, 1);
        assert_eq!(c.stats().resident_bytes, 0, "a cold file holds nothing");
        let calls = s.calls();
        for _ in 0..COLD_READS {
            assert_eq!(read(&c, &f, &s, 0, 1), None);
        }
        assert_eq!(s.calls(), calls);
        assert!(
            read(&c, &f, &s, 0, 1).is_some(),
            "tried again after the spell"
        );
    }

    #[test]
    fn a_file_with_good_locality_never_goes_cold() {
        let c = tiny();
        let s = Source::new(content(16 * 100));
        let f = reg(&c, "seq", 16 * 100);
        // 1-byte sequential reads: one miss per 16 reads.
        for off in 0..16 * 100u64 {
            assert!(read(&c, &f, &s, off, 1).is_some());
        }
        assert_eq!(c.stats().cold, 0);
        assert_eq!(s.calls(), 100);
    }

    #[test]
    fn unused_entries_are_swept_but_poisoned_and_held_ones_are_not() {
        let c = tiny();
        let s = Source::new(content(100));
        let held = reg(&c, "held", 100);
        let _ = c.register(0, "written", 100, 1, false, true);
        let cached = reg(&c, "cached", 100);
        read(&c, &cached, &s, 0, 1).unwrap();
        drop(cached);
        for i in 0..SWEEP_MIN + 10 {
            let _ = reg(&c, &format!("x{i}"), 100);
        }
        let reg_ = lock(&c.files);
        let has = |p: &str| {
            reg_.by_name.contains_key(&Name {
                root: 0,
                path: p.to_string(),
            })
        };
        assert!(has("held") && has("written") && has("cached"));
        assert!(reg_.by_name.len() < SWEEP_MIN, "the unused ones went");
        drop(reg_);
        drop(held);
    }
}
