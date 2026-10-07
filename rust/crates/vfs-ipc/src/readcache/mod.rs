//! A client-side block cache for **small reads of immutable files**.
//!
//! A game and its plugins make huge numbers of tiny reads: one handle on
//! `Skyrim.esm` (≈240 MB) made 838,643 reads of 4 KiB in one launch, and
//! `plugins.txt` was read one byte per call. Over the ring each of those is a
//! round trip (a few µs plus the provider) where Windows would have answered
//! from its page cache. This cache sits in front of the ring for reads shorter
//! than [`CacheConfig::threshold`]: it serves them from aligned **units** of
//! [`CacheConfig::block`] bytes (64 KiB), fetched from the director with one
//! bulk read per miss. Reads at or above the threshold never touch it.
//!
//! OS-free and generic over how a block is fetched, so the shim, the native
//! tests and `ring-bench` all run this same code.
//!
//! # How much a miss fetches
//!
//! Every byte fetched costs: a provider serving content from a compressed
//! store spends about a millisecond a MiB (measured on a 3,472-mod list's
//! traces), roughly what a hundred small round trips cost. A miss therefore
//! fetches **one 64 KiB unit**, and the run grows — 2, 4, … up to
//! [`CacheConfig::max_run`] units (1 MiB) in one request — only while a
//! file's misses keep landing exactly where its last fetch ended, as a
//! sequential reader's do. A random read costs 64 KiB, not 1 MiB; a read
//! through a master file in 4 KiB pieces costs one round trip a MiB. A unit
//! never extends past the end of its file, so a small file costs its size.
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
//! installs *loading* slots for the units it fetches and fetches, the second
//! waits for that fetch (single flight) — but never past the fetch's own
//! deadline ([`CacheConfig::wait`], counted from when it started). A fetch
//! that outlives it is removed by whichever reader finds it, so a thread
//! killed mid-fetch (which runs no cleanup) costs the survivors at most one
//! deadline, once, as the ring's `DataGate` promises for its own permits.
//! Memory is bounded under contention because a fetch reserves its bytes
//! against [`CacheConfig::max_bytes`] **before** it starts, evicting the
//! least recently used units of any file to make room; when nothing can be
//! evicted (everything is mid-fetch) the read is simply served uncached.
//! Lock order: file table → file → LRU index / retired diagnostics; neither of
//! those is ever held while a file lock is taken.
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
use std::sync::{Arc, Mutex, MutexGuard, Weak};

mod config;
mod file;
mod stats;
#[cfg(test)]
mod tests;

pub use config::{
    CacheConfig, DEFAULT_BLOCK, DEFAULT_BLOCKS_PER_FILE, DEFAULT_COLD_HITS_PER_MISS,
    DEFAULT_MAX_BYTES, DEFAULT_MAX_RUN, DEFAULT_THRESHOLD,
};
pub use file::FileRef;
pub use stats::{CacheStats, FileDiag, FileReport};
use config::{
    COLD_AFTER_FAILED_FETCHES, COLD_AFTER_MISSES, COLD_MAX_READS, COLD_READS, PRESSURE_MEMORY,
    RETIRED_DIAGS,
};
use file::{Entry, Flight, Name, Slot, SlotKind, State, Version};
use stats::{bump, Counters};


fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}


struct FileTable {
    by_name: HashMap<Name, Arc<Entry>>,
    /// Size past which the next registration sweeps out unused entries.
    sweep_at: usize,
}

const SWEEP_MIN: usize = 1024;

/// How a read came by one of its units.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Got {
    /// Already held.
    Hit,
    /// Fetched by this read: a locality miss (`counts`), or one that says
    /// nothing about locality — the cap had evicted it (`pressure`), or it
    /// is the file's first fetch of it.
    Fetched { counts: bool, pressure: bool },
    /// Fetched by another thread, which this read waited for.
    Waited,
}

/// The cache. One per process in the shim; see the module docs.
pub struct ReadCache {
    cfg: CacheConfig,
    files: Mutex<FileTable>,
    /// Ready units by the tick they were filed under, oldest first: the
    /// process-wide LRU. A unit read since it was filed carries a newer
    /// tick of its own and is re-filed when it reaches the front, rather
    /// than on every hit, so a hit takes only its file's lock.
    lru: Mutex<BTreeMap<u64, (Weak<Entry>, u64)>>,
    /// Bytes held: ready units plus fetches in flight.
    used: AtomicUsize,
    tick: AtomicU64,
    counters: Counters,
    /// Diagnostics of files swept out of the file table, so the per-file
    /// table covers the whole run.
    retired: Mutex<HashMap<Name, FileDiag>>,
}

impl Default for ReadCache {
    fn default() -> Self {
        Self::new(CacheConfig::default())
    }
}

impl ReadCache {
    pub fn new(cfg: CacheConfig) -> Self {
        assert!(
            cfg.block > 0 && cfg.blocks_per_file > 0 && cfg.max_run > 0,
            "an empty cache"
        );
        assert!(
            cfg.threshold <= cfg.block,
            "a read the cache serves must fit in two units"
        );
        ReadCache {
            cfg,
            files: Mutex::new(FileTable {
                by_name: HashMap::new(),
                sweep_at: SWEEP_MIN,
            }),
            lru: Mutex::new(BTreeMap::new()),
            used: AtomicUsize::new(0),
            tick: AtomicU64::new(1),
            counters: Counters::default(),
            retired: Mutex::new(HashMap::new()),
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
                self.sweep(&mut reg);
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
            st.pressure_evicted.clear();
            st.fetched_before.clear();
            st.next_seq = None;
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
    /// changed file being cached again). What they did is kept in
    /// `retired` for the per-file table.
    fn sweep(&self, reg: &mut FileTable) {
        let mut gone: Vec<(Name, FileDiag)> = Vec::new();
        reg.by_name.retain(|name, e| {
            if Arc::strong_count(e) > 1 {
                return true;
            }
            match e.state.try_lock() {
                Ok(st) => {
                    let keep = st.poisoned || !st.slots.is_empty();
                    if !keep && st.diag.reads > 0 {
                        gone.push((name.clone(), st.diag));
                    }
                    keep
                }
                Err(_) => true,
            }
        });
        reg.sweep_at = (reg.by_name.len() * 2).max(SWEEP_MIN);
        if !gone.is_empty() {
            let mut retired = lock(&self.retired);
            for (name, d) in gone {
                retired.entry(name).or_default().add(&d);
            }
            if retired.len() > RETIRED_DIAGS {
                // Keep the busiest half.
                let mut reads: Vec<u64> = retired.values().map(|d| d.reads).collect();
                reads.sort_unstable();
                let cut = reads[reads.len() / 2];
                retired.retain(|_, d| d.reads > cut);
            }
        }
    }

    /// The `n` files with the most small reads offered to the cache, busiest
    /// first: live ones and ones already swept out. Holds the file table for
    /// one pass over it (and each file only if it is free), so it is for a
    /// periodic report, not a hot path.
    pub fn top_files(&self, n: usize) -> Vec<FileReport> {
        let mut all: HashMap<Name, FileReport> = HashMap::new();
        {
            let reg = lock(&self.files);
            for e in reg.by_name.values() {
                // A report must not wait on a file mid-read; one skipped
                // file is a row short, not a stall.
                let Ok(st) = e.state.try_lock() else {
                    continue;
                };
                if st.diag.reads == 0 {
                    continue;
                }
                all.insert(
                    e.name.clone(),
                    FileReport {
                        root: e.name.root,
                        path: e.name.path.clone(),
                        diag: st.diag,
                        cold_now: st.cold_left > 0,
                        poisoned: st.poisoned,
                    },
                );
            }
        }
        for (name, d) in lock(&self.retired).iter() {
            all.entry(name.clone())
                .or_insert_with(|| FileReport {
                    root: name.root,
                    path: name.path.clone(),
                    diag: FileDiag::default(),
                    cold_now: false,
                    poisoned: false,
                })
                .diag
                .add(d);
        }
        let mut rows: Vec<FileReport> = all.into_values().collect();
        rows.sort_by(|a, b| b.diag.reads.cmp(&a.diag.reads).then(a.path.cmp(&b.path)));
        rows.truncate(n);
        rows
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
        st.pressure_evicted.clear();
        st.version = None;
    }

    /// Remove every slot: ready units give their bytes back; a fetch in
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
    /// cache, fetching missing units with `fetch(offset, fetch_buf)`, which
    /// must read `fetch_buf.len()` bytes at `offset` (one or more whole
    /// units, cut short at end of file) and return how many it read.
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
            st.diag.reads += 1;
            let refuse = if st.poisoned || st.version != Some(ver) {
                true
            } else if st.cold_left > 0 {
                st.cold_left -= 1;
                true
            } else {
                false
            };
            if refuse {
                st.diag.declined += 1;
                drop(st);
                bump(&self.counters.declined, 1);
                return None;
            }
        }
        let unit = self.cfg.block as u64;
        let first = off / unit;
        let last = (off + len as u64 - 1) / unit;
        let mut how = Got::Hit;
        let mut done = 0usize;
        for idx in first..=last {
            let Some((data, got)) = self.unit(f, ver, idx, &mut fetch) else {
                bump(&self.counters.declined, 1);
                lock(&f.entry.state).diag.declined += 1;
                return None;
            };
            how = match (how, got) {
                (
                    Got::Fetched {
                        counts: c1,
                        pressure: p1,
                    },
                    Got::Fetched {
                        counts: c2,
                        pressure: p2,
                    },
                ) => Got::Fetched {
                    counts: c1 || c2,
                    pressure: p1 || p2,
                },
                (f @ Got::Fetched { .. }, _) | (_, f @ Got::Fetched { .. }) => f,
                (Got::Waited, _) | (_, Got::Waited) => Got::Waited,
                _ => Got::Hit,
            };
            let at = off + done as u64;
            let from = (at - idx * unit) as usize;
            let n = (len - done).min(data.len() - from);
            buf[done..done + n].copy_from_slice(&data[from..from + n]);
            done += n;
        }
        debug_assert_eq!(done, len);
        match how {
            Got::Hit => bump(&self.counters.hits, 1),
            Got::Fetched { pressure: true, .. } => {
                bump(&self.counters.misses, 1);
                bump(&self.counters.pressure_misses, 1);
            }
            _ => bump(&self.counters.misses, 1),
        }
        self.judge(f, how);
        Some(len)
    }

    /// Count a served read toward deciding whether `f` is worth caching.
    /// Only a **locality** miss counts against it: a unit fetched again
    /// after the file's own LRU dropped it (and, if
    /// [`CacheConfig::cold_counts_first_fetch`], a first fetch). A unit the
    /// process-wide cap evicted, or one another thread fetched, says
    /// nothing about how the file is read.
    fn judge(&self, f: &FileRef, how: Got) {
        let mut st = lock(&f.entry.state);
        match how {
            Got::Hit => {
                st.hits = st.hits.saturating_add(1);
                st.diag.hits += 1;
                return;
            }
            Got::Waited => {
                st.diag.misses += 1;
                return;
            }
            Got::Fetched { pressure: true, .. } => {
                st.diag.misses += 1;
                st.diag.pressure_misses += 1;
                return;
            }
            Got::Fetched { counts: false, .. } => {
                st.diag.misses += 1;
                return;
            }
            Got::Fetched { counts: true, .. } => {
                st.diag.misses += 1;
                st.misses += 1;
            }
        }
        if st.misses < COLD_AFTER_MISSES {
            return;
        }
        if st.hits < st.misses * self.cfg.cold_hits_per_miss {
            st.diag.cold_guard += 1;
            self.go_cold(&mut st);
        }
        st.hits = 0;
        st.misses = 0;
    }

    /// Serve the file uncached for a spell, doubling with each one.
    fn go_cold(&self, st: &mut State) {
        let spell = if st.cold_next == 0 {
            COLD_READS
        } else {
            st.cold_next
        };
        st.cold_left = spell;
        st.cold_next = (spell * 2).min(COLD_MAX_READS);
        st.failed_fetches = 0;
        st.next_seq = None;
        st.pressure_evicted.clear();
        bump(&self.counters.cold, 1);
        // Its units are of no use while it is cold.
        self.clear_slots(st);
    }

    /// Unit `idx` of `f`, and how this read came by it. `None` if it cannot
    /// be had from the cache.
    fn unit<F>(
        &self,
        f: &FileRef,
        ver: Version,
        idx: u64,
        fetch: &mut F,
    ) -> Option<(Arc<[u8]>, Got)>
    where
        F: FnMut(u64, &mut [u8]) -> Result<usize, i32>,
    {
        let flight = loop {
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
                    return Some((Arc::clone(data), Got::Hit));
                }
                Some(Slot {
                    kind: SlotKind::Loading(flight),
                    ..
                }) => {
                    if !flight.overdue(self.cfg.wait) {
                        break Arc::clone(flight);
                    }
                    // A fetch that has outlived its deadline: its thread is
                    // gone (killed mid-fetch, which runs no cleanup) or its
                    // provider stopped answering. Waiting on it would cost
                    // every reader of these units the whole deadline, again
                    // and again; drop it and fetch here.
                    let stale = Arc::clone(flight);
                    st.slots.retain(
                        |s| !matches!(&s.kind, SlotKind::Loading(fl) if Arc::ptr_eq(fl, &stale)),
                    );
                    stale.complete(None);
                    bump(&self.counters.fetches_abandoned, 1);
                    continue;
                }
                None => return self.miss(f, ver, idx, st, fetch),
            }
        };
        // Someone else is fetching it: wait for that, holding no lock, for
        // at most what is left of its deadline.
        match flight.wait(self.cfg.wait) {
            Ok(Some((base, run))) => {
                let unit = self.cfg.block;
                let from = (idx - base) as usize * unit;
                let to = (from + unit).min(run.len());
                Some((Arc::from(&run[from..to]), Got::Waited))
            }
            Ok(None) => None,
            Err(()) => {
                // It never finished. Remove it — if the slots still hold
                // this same fetch — so the next reader does not wait on it.
                let mut st = lock(&f.entry.state);
                let before = st.slots.len();
                st.slots.retain(
                    |s| !matches!(&s.kind, SlotKind::Loading(fl) if Arc::ptr_eq(fl, &flight)),
                );
                if st.slots.len() != before {
                    bump(&self.counters.fetches_abandoned, 1);
                }
                drop(st);
                flight.complete(None);
                None
            }
        }
    }

    /// `idx` is not held: decide how many units to fetch, install loading
    /// slots for them, and fetch. Called with the file locked.
    fn miss<F>(
        &self,
        f: &FileRef,
        ver: Version,
        idx: u64,
        mut st: MutexGuard<'_, State>,
        fetch: &mut F,
    ) -> Option<(Arc<[u8]>, Got)>
    where
        F: FnMut(u64, &mut [u8]) -> Result<usize, i32>,
    {
        let unit = self.cfg.block as u64;
        let units = ver.size.div_ceil(unit);
        let pressure = st.pressure_evicted.remove(&idx);
        let counts = !pressure && (st.refetch(idx) || self.cfg.cold_counts_first_fetch);
        // A run grows only while misses land where the last fetch ended.
        let run = if st.next_seq == Some(idx) {
            st.run.max(1)
        } else {
            1
        };
        let mut end = idx + 1;
        while end < idx + run as u64 && end < units && !st.slots.iter().any(|s| s.idx == end) {
            end += 1;
        }
        // Room in this file, from its own least recently used units.
        while st.slots.len() + (end - idx) as usize > self.cfg.blocks_per_file {
            if !self.evict_in_file(&mut st) {
                if st.slots.len() < self.cfg.blocks_per_file {
                    end = idx + (self.cfg.blocks_per_file - st.slots.len()) as u64;
                    break;
                }
                // Every slot is mid-fetch.
                return None;
            }
        }
        st.next_seq = Some(end);
        st.run = (run * 2).min(self.cfg.max_run);
        let flight = Arc::new(Flight::new());
        for j in idx..end {
            st.slots.push(Slot {
                idx: j,
                kind: SlotKind::Loading(Arc::clone(&flight)),
            });
        }
        drop(st);
        let data = self.load(f, ver, idx, end, flight, fetch)?;
        Some((data, Got::Fetched { counts, pressure }))
    }

    /// Drop the least recently used ready unit of one file to make room
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

    /// Fetch units `[idx, end)` of `f` into the loading slots this thread
    /// installed, and return unit `idx`.
    fn load<F>(
        &self,
        f: &FileRef,
        ver: Version,
        idx: u64,
        end: u64,
        flight: Arc<Flight>,
        fetch: &mut F,
    ) -> Option<Arc<[u8]>>
    where
        F: FnMut(u64, &mut [u8]) -> Result<usize, i32>,
    {
        let unit = self.cfg.block as u64;
        let start = idx * unit;
        let want = ((end * unit).min(ver.size) - start) as usize;
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
            // Short of the run means short of what the director said the
            // file holds: not something to keep. The caller's uncached read
            // gets whatever the director answers now.
            Ok(n) if n == want => {}
            _ => {
                bump(&self.counters.fetch_failures, 1);
                let mut st = lock(&f.entry.state);
                st.failed_fetches += 1;
                if st.failed_fetches >= COLD_AFTER_FAILED_FETCHES && st.cold_left == 0 {
                    st.diag.cold_failures += 1;
                    self.go_cold(&mut st);
                }
                return None;
            }
        }
        bump(&self.counters.fetches, 1);
        bump(&self.counters.bytes_fetched, want as u64);
        let run: Arc<[u8]> = buf.into();
        let mut first: Option<Arc<[u8]>> = None;
        {
            let mut st = lock(&f.entry.state);
            st.diag.fetches += 1;
            st.diag.bytes_fetched += want as u64;
            if st.poisoned || st.version != Some(ver) {
                return None;
            }
            let mut installed = 0usize;
            for j in idx..end {
                let Some(slot) = st.slots.iter_mut().find(|s| {
                    s.idx == j
                        && matches!(&s.kind, SlotKind::Loading(fl) if Arc::ptr_eq(fl, &flight))
                }) else {
                    // Dropped meanwhile (the file went cold, or changed).
                    continue;
                };
                let from = (j - idx) as usize * unit as usize;
                let to = (from + unit as usize).min(run.len());
                let data: Arc<[u8]> = Arc::from(&run[from..to]);
                let tick = self.now();
                slot.kind = SlotKind::Ready {
                    data: Arc::clone(&data),
                    last: tick,
                    lru_key: tick,
                };
                lock(&self.lru).insert(tick, (Arc::downgrade(&f.entry), j));
                installed += data.len();
                if j == idx {
                    first = Some(data);
                }
            }
            // What was reserved for units no longer wanted goes back.
            self.used.fetch_sub(want - installed, Ordering::AcqRel);
            guard.reserved = 0;
            first.as_ref()?;
        }
        flight.complete(Some((idx, run)));
        std::mem::forget(guard);
        first
    }

    /// Take `n` bytes of the budget, evicting the least recently used
    /// units of any file until they fit. `false` if they cannot.
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

    /// Evict the process-wide least recently used ready unit. `false` if
    /// there is none. The file remembers the unit went for capacity, so a
    /// later miss on it is not held against the file's locality.
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
            if st.pressure_evicted.len() < PRESSURE_MEMORY {
                st.pressure_evicted.insert(idx);
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
            pressure_misses: get(&c.pressure_misses),
            declined: get(&c.declined),
            fetches: get(&c.fetches),
            bytes_fetched: get(&c.bytes_fetched),
            evictions: get(&c.evictions),
            invalidations: get(&c.invalidations),
            blocks_invalidated: get(&c.blocks_invalidated),
            cold: get(&c.cold),
            fetch_failures: get(&c.fetch_failures),
            fetches_abandoned: get(&c.fetches_abandoned),
            resident_bytes: self.used.load(Ordering::Relaxed) as u64,
            max_bytes: self.cfg.max_bytes as u64,
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
